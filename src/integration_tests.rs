//! Integration tests. Each #[sqlx::test] gets its own freshly migrated database.

use std::{
    collections::HashSet,
    future::Ready,
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    grid,
    reports::{ReportKind, record_report},
    status::Status,
    worker::{dispatch_batch, expire_stale_outages},
};

// Dhaka (cell 2381/9041) and a second, far-away cell.
const LAT: f64 = 23.8103;
const LNG: f64 = 90.4125;
const OTHER_LAT: f64 = 24.9;
const OTHER_LNG: f64 = 91.87;

// ---------- helpers ----------

fn device() -> Uuid {
    Uuid::new_v4()
}

async fn report(pool: &PgPool, who: Uuid, kind: ReportKind) -> Status {
    record_report(pool, who, kind, LAT, LNG)
        .await
        .expect("report should succeed")
        .status
}

/// Three outage reports from fresh devices: the cell must go down on the third.
async fn take_down(pool: &PgPool) {
    for i in 0..3 {
        let status = report(pool, device(), ReportKind::Outage).await;
        assert_eq!(status, if i == 2 { Status::Down } else { Status::Up });
    }
}

/// Two restored reports from fresh devices: the cell must come back on the second.
async fn restore(pool: &PgPool) {
    for i in 0..2 {
        let status = report(pool, device(), ReportKind::Restored).await;
        assert_eq!(status, if i == 1 { Status::Up } else { Status::Down });
    }
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}
async fn cell_status(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT status FROM cells WHERE lat_idx = 2381 AND lng_idx = 9041")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Moves everything back in time, as if it all happened `minutes` ago.
async fn age_everything(pool: &PgPool, minutes: i32) {
    for sql in [
        "UPDATE reports SET created_at = created_at - make_interval(mins => $1)",
        "UPDATE cells SET status_since = status_since - make_interval(mins => $1)",
        "UPDATE outage_events SET started_at = started_at - make_interval(mins => $1),
                                  ended_at = ended_at - make_interval(mins => $1)",
    ] {
        sqlx::query(sql).bind(minutes).execute(pool).await.unwrap();
    }
}

async fn ensure_cell(pool: &PgPool, lat: f64, lng: f64) -> i64 {
    let cell = grid::cell_for(lat, lng);
    sqlx::query_scalar(
        "INSERT INTO cells (lat_idx, lng_idx) VALUES ($1, $2)
         ON CONFLICT (lat_idx, lng_idx) DO UPDATE SET lat_idx = EXCLUDED.lat_idx
         RETURNING id",
    )
    .bind(cell.lat_idx)
    .bind(cell.lng_idx)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn subscribe(pool: &PgPool, who: Uuid, lat: f64, lng: f64) -> i64 {
    let cell_id = ensure_cell(pool, lat, lng).await;
    sqlx::query_scalar(
        "INSERT INTO subscriptions (device_id, cell_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(who)
    .bind(cell_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Pretends every pending notification just became due (backoff or lease expired).
async fn make_due(pool: &PgPool) {
    sqlx::query("UPDATE notifications SET next_attempt_at = now() WHERE status = 'pending'")
        .execute(pool)
        .await
        .unwrap();
}

// ---------- fake delivery channels ----------

type Sent = Arc<Mutex<Vec<(Uuid, String)>>>;

fn delivered(sent: &Sent) -> Vec<(Uuid, String)> {
    sent.lock().unwrap().clone()
}

/// A sender that records every delivery. `+ use<>`: the closure owns its own
/// Arc clone and does not borrow `sink` (edition 2024 would otherwise assume it does).
fn recorder(sink: &Sent) -> impl Fn(Uuid, i32, i32, String) -> Ready<Result<(), String>> + use<> {
    let sink = sink.clone();
    move |who: Uuid, _lat: i32, _lng: i32, kind: String| {
        sink.lock().unwrap().push((who, kind));
        std::future::ready(Ok::<(), String>(()))
    }
}

fn failing() -> impl Fn(Uuid, i32, i32, String) -> Ready<Result<(), String>> {
    |_: Uuid, _: i32, _: i32, _: String| {
        std::future::ready(Err::<(), String>("simulated failure".to_string()))
    }
}

// ---------- vote counting ----------

#[sqlx::test]
async fn two_distinct_devices_are_not_enough(pool: PgPool) {
    for _ in 0..2 {
        assert_eq!(
            report(&pool, device(), ReportKind::Outage).await,
            Status::Up
        );
    }
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 0);
}

#[sqlx::test]
async fn three_distinct_devices_take_the_cell_down(pool: PgPool) {
    take_down(&pool).await;

    assert_eq!(cell_status(&pool).await, "down");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 1);
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM outage_events WHERE ended_at IS NULL"
        )
        .await,
        1
    );

    // More outage reports while down never open a second event.
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Down
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 1);
}

#[sqlx::test]
async fn one_device_cannot_vote_twice(pool: PgPool) {
    let a = device();
    for _ in 0..3 {
        assert_eq!(report(&pool, a, ReportKind::Outage).await, Status::Up);
    }
    // a + b = 2 distinct devices
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );
    // a + b + c = 3
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Down
    );
}

#[sqlx::test]
async fn a_devices_latest_report_wins(pool: PgPool) {
    let a = device();
    assert_eq!(report(&pool, a, ReportKind::Outage).await, Status::Up);
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );

    // a changes its mind: its outage vote no longer counts.
    assert_eq!(report(&pool, a, ReportKind::Restored).await, Status::Up);

    // b + c = 2 votes (a is out), so still up...
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );
    // ...and b + c + d = 3 takes it down.
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Down
    );
}

#[sqlx::test]
async fn reports_outside_the_window_do_not_count(pool: PgPool) {
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );

    // Those two votes are now 11 minutes old: outside the 10-minute window.
    age_everything(&pool, 11).await;

    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    ); // 1 fresh vote
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    ); // 2 fresh votes
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Down
    ); // 3 fresh votes
}

#[sqlx::test]
async fn restored_needs_two_votes_and_closes_the_event(pool: PgPool) {
    take_down(&pool).await;

    // Outage reports never close a down cell.
    for _ in 0..5 {
        assert_eq!(
            report(&pool, device(), ReportKind::Outage).await,
            Status::Down
        );
    }

    assert_eq!(
        report(&pool, device(), ReportKind::Restored).await,
        Status::Down
    ); // 1 vote
    assert_eq!(
        report(&pool, device(), ReportKind::Restored).await,
        Status::Up
    ); // 2 votes

    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM outage_events WHERE ended_at IS NULL"
        )
        .await,
        0
    );
    let closed_by: Option<String> = sqlx::query_scalar("SELECT closed_by FROM outage_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(closed_by.as_deref(), Some("reports"));
}

#[sqlx::test]
async fn restored_reports_are_ignored_while_up(pool: PgPool) {
    for _ in 0..5 {
        assert_eq!(
            report(&pool, device(), ReportKind::Restored).await,
            Status::Up
        );
    }
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 0);
}

#[sqlx::test]
async fn old_outage_evidence_cannot_reopen_a_restored_cell(pool: PgPool) {
    take_down(&pool).await;
    restore(&pool).await;

    // The three old outage reports are still inside the 10-minute window,
    // but they are older than the last status change, so they must not count.
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 1);

    // Fresh evidence still works: f + g + h = 3 new votes.
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Up
    );
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Down
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 2);
}

// ---------- concurrency ----------

#[sqlx::test]
async fn simultaneous_reports_stay_consistent(pool: PgPool) {
    const ROUNDS: usize = 5;

    // Each round hammers one brand-new cell with 10 simultaneous reports.
    for round in 0..ROUNDS {
        let lat = 22.0 + round as f64 * 0.5;

        let tasks: Vec<_> = (0..10)
            .map(|_| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    record_report(&pool, Uuid::new_v4(), ReportKind::Outage, lat, LNG)
                        .await
                        .map(|r| r.status)
                })
            })
            .collect();

        let (mut up, mut down) = (0, 0);
        for task in tasks {
            match task.await.expect("task panicked").expect("report failed") {
                Status::Up => up += 1,
                Status::Down => down += 1,
            }
        }

        // The reports are serialized by the cell lock: the first two see
        // 1 and 2 votes, the third flips the cell, everything after sees "down".
        assert_eq!((up, down), (2, 8), "round {round}");
    }

    assert_eq!(count(&pool, "SELECT COUNT(*) FROM reports").await, 50);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM cells WHERE status = 'down'").await,
        5
    );
    // Exactly one event per cell, all still open.
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM outage_events").await, 5);
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM outage_events WHERE ended_at IS NULL"
        )
        .await,
        5
    );
}

#[sqlx::test]
async fn concurrent_dispatchers_never_double_deliver(pool: PgPool) {
    // 130 subscribers on one cell: one outage queues 130 alerts,
    // more than two full batches of 50.
    let cell_id = ensure_cell(&pool, LAT, LNG).await;
    sqlx::query(
        "INSERT INTO subscriptions (device_id, cell_id)
         SELECT gen_random_uuid(), $1 FROM generate_series(1, 130)",
    )
    .bind(cell_id)
    .execute(&pool)
    .await
    .unwrap();

    take_down(&pool).await;
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM notifications").await,
        130
    );

    async fn drain(pool: PgPool, sent: Sent) -> usize {
        let mut total = 0;
        loop {
            let n = dispatch_batch(&pool, recorder(&sent))
                .await
                .expect("dispatch failed");
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    }

    let sent: Sent = Sent::default();
    let first = tokio::spawn(drain(pool.clone(), sent.clone()));
    let second = tokio::spawn(drain(pool.clone(), sent.clone()));
    let (a, b) = (first.await.unwrap(), second.await.unwrap());

    assert_eq!(a + b, 130);

    let (total, unique) = {
        let log = sent.lock().unwrap();
        (
            log.len(),
            log.iter().map(|(d, _)| *d).collect::<HashSet<_>>().len(),
        )
    };
    assert_eq!(total, 130);
    assert_eq!(unique, 130, "an alert was delivered twice");

    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM notifications WHERE status = 'sent' AND attempts = 1"
        )
        .await,
        130
    );
}

// ---------- stale outage expiry ----------

#[sqlx::test]
async fn a_stale_outage_is_closed_by_timeout_and_subscribers_are_told(pool: PgPool) {
    subscribe(&pool, device(), LAT, LNG).await;
    take_down(&pool).await;

    // Three hours with no further outage report.
    age_everything(&pool, 180).await;
    expire_stale_outages(&pool).await.unwrap();

    assert_eq!(cell_status(&pool).await, "up");

    let (closed_by, ended_ago_secs): (Option<String>, f64) = sqlx::query_as(
        "SELECT closed_by, EXTRACT(EPOCH FROM (now() - ended_at))::float8 FROM outage_events",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(closed_by.as_deref(), Some("timeout"));
    // The end time is the last outage report we saw (about 3 hours ago),
    // not "now": we never invent downtime.
    assert!(ended_ago_secs > 10_000.0, "ended {ended_ago_secs}s ago");

    let expired = "SELECT COUNT(*) FROM notifications WHERE kind = 'expired'";
    assert_eq!(count(&pool, expired).await, 1);

    // A second scan changes nothing.
    expire_stale_outages(&pool).await.unwrap();
    assert_eq!(count(&pool, expired).await, 1);
}

#[sqlx::test]
async fn a_fresh_outage_is_not_expired(pool: PgPool) {
    take_down(&pool).await;
    expire_stale_outages(&pool).await.unwrap();

    assert_eq!(cell_status(&pool).await, "down");
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM outage_events WHERE ended_at IS NULL"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn a_recent_outage_report_keeps_the_outage_alive(pool: PgPool) {
    take_down(&pool).await;
    age_everything(&pool, 180).await;

    // Someone confirms the outage just now.
    assert_eq!(
        report(&pool, device(), ReportKind::Outage).await,
        Status::Down
    );

    expire_stale_outages(&pool).await.unwrap();

    assert_eq!(cell_status(&pool).await, "down");
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM outage_events WHERE ended_at IS NULL"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn expiry_leaves_already_restored_outages_alone(pool: PgPool) {
    subscribe(&pool, device(), LAT, LNG).await;
    take_down(&pool).await;
    restore(&pool).await;

    age_everything(&pool, 180).await;
    expire_stale_outages(&pool).await.unwrap();

    let closed_by: Option<String> = sqlx::query_scalar("SELECT closed_by FROM outage_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(closed_by.as_deref(), Some("reports"));
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM notifications WHERE kind = 'expired'"
        )
        .await,
        0
    );
}

// ---------- notification delivery ----------

#[sqlx::test]
async fn subscribers_are_alerted_exactly_once(pool: PgPool) {
    let near = device();
    let far = device();
    subscribe(&pool, near, LAT, LNG).await;
    subscribe(&pool, far, OTHER_LAT, OTHER_LNG).await;

    take_down(&pool).await;

    let sent = Sent::default();
    assert_eq!(dispatch_batch(&pool, recorder(&sent)).await.unwrap(), 1);
    // Only the subscriber of the cell that changed is told.
    assert_eq!(delivered(&sent), vec![(near, "down".to_string())]);

    // Nothing is left to deliver, so nothing is sent twice.
    assert_eq!(dispatch_batch(&pool, recorder(&sent)).await.unwrap(), 0);

    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("sent", 1));

    // Recovery produces a separate, second alert.
    restore(&pool).await;
    assert_eq!(dispatch_batch(&pool, recorder(&sent)).await.unwrap(), 1);
    assert_eq!(
        delivered(&sent),
        vec![(near, "down".to_string()), (near, "restored".to_string())]
    );
}

#[sqlx::test]
async fn unsubscribing_cancels_pending_alerts(pool: PgPool) {
    let a = device();
    let b = device();
    let sub_a = subscribe(&pool, a, LAT, LNG).await;
    subscribe(&pool, b, LAT, LNG).await;

    take_down(&pool).await;
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM notifications").await, 2);

    // Deleting a subscription removes its pending alert (ON DELETE CASCADE).
    sqlx::query("DELETE FROM subscriptions WHERE id = $1")
        .bind(sub_a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM notifications").await, 1);

    let sent = Sent::default();
    dispatch_batch(&pool, recorder(&sent)).await.unwrap();
    assert_eq!(delivered(&sent), vec![(b, "down".to_string())]);

    // Someone who subscribed and unsubscribed before the next change gets nothing.
    let c = device();
    let sub_c = subscribe(&pool, c, LAT, LNG).await;
    sqlx::query("DELETE FROM subscriptions WHERE id = $1")
        .bind(sub_c)
        .execute(&pool)
        .await
        .unwrap();
    restore(&pool).await;
    // Only b's "down" and b's "restored" exist.
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM notifications").await, 2);
}

#[sqlx::test]
async fn failed_delivery_backs_off_then_gives_up(pool: PgPool) {
    subscribe(&pool, device(), LAT, LNG).await;
    take_down(&pool).await;

    // First attempt fails and is scheduled for later.
    assert_eq!(dispatch_batch(&pool, failing()).await.unwrap(), 1);

    let (status, attempts, last_error, delayed): (String, i32, Option<String>, bool) =
        sqlx::query_as(
            "SELECT status, attempts, last_error,
                    next_attempt_at > now() + interval '20 seconds'
             FROM notifications",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(attempts, 1);
    assert_eq!(last_error.as_deref(), Some("simulated failure"));
    assert!(delayed, "the retry should be pushed into the future");

    // Not due yet: nothing is claimed.
    assert_eq!(dispatch_batch(&pool, failing()).await.unwrap(), 0);

    // Attempts 2 to 5.
    for _ in 2..=5 {
        make_due(&pool).await;
        assert_eq!(dispatch_batch(&pool, failing()).await.unwrap(), 1);
    }

    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("failed", 5));

    // A failed notification is never claimed again.
    make_due(&pool).await;
    assert_eq!(dispatch_batch(&pool, failing()).await.unwrap(), 0);
}

#[sqlx::test]
async fn a_crashed_workers_claim_is_retried_after_the_lease(pool: PgPool) {
    let who = device();
    subscribe(&pool, who, LAT, LNG).await;
    take_down(&pool).await;

    // A worker claims the alert, then dies mid-delivery: its delivery never finishes.
    let crashed = tokio::time::timeout(
        Duration::from_millis(500),
        dispatch_batch(&pool, |_: Uuid, _: i32, _: i32, _: String| {
            std::future::pending::<Result<(), String>>()
        }),
    )
    .await;
    assert!(crashed.is_err(), "the delivery should have hung");

    // While the lease is held, other workers don't touch it.
    let sent = Sent::default();
    assert_eq!(dispatch_batch(&pool, recorder(&sent)).await.unwrap(), 0);
    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 1));

    // The lease runs out: another worker picks it up and delivers.
    make_due(&pool).await;
    assert_eq!(dispatch_batch(&pool, recorder(&sent)).await.unwrap(), 1);
    assert_eq!(delivered(&sent), vec![(who, "down".to_string())]);

    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("sent", 2));
}
