use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::status;

use std::future::Future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DISPATCH_INTERVAL: Duration = Duration::from_secs(5);
const EXPIRE_INTERVAL: Duration = Duration::from_secs(60);
const BATCH: i64 = 50;
const LEASE_SECS: f64 = 60.0;
const MAX_ATTEMPTS: i32 = 5;
/// An open outage with no outage report for this long is presumed over.
const STALE_OUTAGE_SECS: f64 = 7200.0; // 2 hours

pub fn spawn(pool: PgPool, shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
    vec![
        tokio::spawn(dispatch_loop(pool.clone(), shutdown.clone())),
        tokio::spawn(expire_loop(pool, shutdown)),
    ]
}

// ---------- notification delivery ----------

const CLAIM_SQL: &str = "
    WITH claimed AS (
        UPDATE notifications
        SET attempts = attempts + 1,
            next_attempt_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT id FROM notifications
            WHERE status = 'pending' AND next_attempt_at <= now()
            ORDER BY id
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, subscription_id, cell_id, kind, attempts
    )
    SELECT c.id, s.device_id, ce.lat_idx, ce.lng_idx, c.kind, c.attempts
    FROM claimed c
    JOIN subscriptions s ON s.id = c.subscription_id
    JOIN cells ce ON ce.id = c.cell_id";

async fn default_send(
    device_id: Uuid,
    lat_idx: i32,
    lng_idx: i32,
    kind: String,
) -> Result<(), String> {
    deliver(device_id, lat_idx, lng_idx, &kind).await
}
async fn dispatch_loop(pool: PgPool, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(DISPATCH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.cancelled() => break,
        }

        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match dispatch_batch(&pool, default_send).await {
                // A full batch means more may be waiting: go again immediately.
                Ok(n) if n >= BATCH as usize => continue,
                Ok(_) => break,
                Err(e) => {
                    tracing::error!("notification dispatch failed: {e}");
                    break;
                }
            }
        }
    }
    tracing::info!("notification dispatcher stopped");
}
pub(crate) async fn dispatch_batch<F, Fut>(pool: &PgPool, send: F) -> Result<usize, sqlx::Error>
where
    F: Fn(Uuid, i32, i32, String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let rows: Vec<(i64, Uuid, i32, i32, String, i32)> = sqlx::query_as(CLAIM_SQL)
        .bind(BATCH)
        .bind(LEASE_SECS)
        .fetch_all(pool)
        .await?;

    let count = rows.len();

    for (id, device_id, lat_idx, lng_idx, kind, attempts) in rows {
        match send(device_id, lat_idx, lng_idx, kind).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE notifications
                     SET status = 'sent', sent_at = now(), last_error = NULL
                     WHERE id = $1",
                )
                .bind(id)
                .execute(pool)
                .await?;
                metrics::counter!("notifications_sent_total").increment(1);
            }
            Err(err) => {
                tracing::warn!(id, attempts, "delivery failed: {err}");
                if attempts >= MAX_ATTEMPTS {
                    sqlx::query(
                        "UPDATE notifications SET status = 'failed', last_error = $2 WHERE id = $1",
                    )
                    .bind(id)
                    .bind(&err)
                    .execute(pool)
                    .await?;
                    metrics::counter!("notifications_failed_total").increment(1);
                } else {
                    sqlx::query(
                        "UPDATE notifications
                         SET next_attempt_at = now() + make_interval(secs => $2),
                             last_error = $3
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(backoff_secs(attempts))
                    .bind(&err)
                    .execute(pool)
                    .await?;
                }
            }
        }
    }

    Ok(count)
}

fn backoff_secs(attempts: i32) -> f64 {
    (30.0 * 2f64.powi((attempts - 1).clamp(0, 10))).min(1800.0)
}

fn message(kind: &str) -> &'static str {
    match kind {
        "down" => "Power outage reported in your area",
        "restored" => "Power has been reported back in your area",
        "expired" => "No recent reports: the outage in your area is presumed over",
        _ => "Status update for your area",
    }
}

/// The only place that knows how an alert leaves the system.
/// For now it just logs. A real channel (e.g. a Telegram bot) replaces this body.
async fn deliver(device_id: Uuid, lat_idx: i32, lng_idx: i32, kind: &str) -> Result<(), String> {
    tracing::info!(%device_id, lat_idx, lng_idx, kind, "ALERT: {}", message(kind));
    Ok(())
}

// ---------- stale outage cleanup ----------

const CANDIDATES_SQL: &str = "
    SELECT e.cell_id
    FROM outage_events e
    WHERE e.ended_at IS NULL
      AND GREATEST(
            e.started_at,
            COALESCE((SELECT MAX(r.created_at) FROM reports r
                      WHERE r.cell_id = e.cell_id AND r.kind = 'outage'), e.started_at)
          ) < now() - make_interval(secs => $1)
    LIMIT 100";

// The staleness test is repeated here, under the cell lock, because the
// situation may have changed between listing candidates and locking.
// ended_at is the last outage report we saw (never earlier than started_at + 1s,
// because of the CHECK constraint), so we do not invent downtime.
const CLOSE_SQL: &str = "
    WITH last AS (
        SELECT e.id,
               GREATEST(
                   e.started_at,
                   COALESCE((SELECT MAX(r.created_at) FROM reports r
                             WHERE r.cell_id = e.cell_id AND r.kind = 'outage'), e.started_at)
               ) AS last_seen
        FROM outage_events e
        WHERE e.cell_id = $1 AND e.ended_at IS NULL
    )
    UPDATE outage_events e
    SET ended_at = GREATEST(l.last_seen, e.started_at + interval '1 second'),
        closed_by = 'timeout'
    FROM last l
    WHERE e.id = l.id
      AND l.last_seen < now() - make_interval(secs => $2)";

async fn expire_loop(pool: PgPool, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(EXPIRE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.cancelled() => break,
        }

        if let Err(e) = expire_stale_outages(&pool).await {
            tracing::error!("stale outage scan failed: {e}");
        }
    }
    tracing::info!("stale outage scanner stopped");
}

pub(crate) async fn expire_stale_outages(pool: &PgPool) -> Result<(), sqlx::Error> {
    let candidates: Vec<i64> = sqlx::query_scalar(CANDIDATES_SQL)
        .bind(STALE_OUTAGE_SECS)
        .fetch_all(pool)
        .await?;

    for cell_id in candidates {
        if let Err(e) = expire_cell(pool, cell_id).await {
            tracing::error!(cell_id, "failed to expire outage: {e}");
        }
    }
    Ok(())
}

async fn expire_cell(pool: &PgPool, cell_id: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Same lock the report path takes, so we never race a live report.
    sqlx::query("SELECT 1 FROM cells WHERE id = $1 FOR UPDATE")
        .bind(cell_id)
        .execute(&mut *tx)
        .await?;

    let closed = sqlx::query(CLOSE_SQL)
        .bind(cell_id)
        .bind(STALE_OUTAGE_SECS)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    if closed > 0 {
        sqlx::query(
            "UPDATE cells SET status = 'up', status_since = clock_timestamp() WHERE id = $1",
        )
        .bind(cell_id)
        .execute(&mut *tx)
        .await?;

        let queued = status::enqueue_notifications(&mut *tx, cell_id, "expired").await?;
        tracing::info!(cell_id, queued, "stale outage expired");
        metrics::counter!("outages_expired_total").increment(1);
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_secs(1), 30.0);
        assert_eq!(backoff_secs(2), 60.0);
        assert_eq!(backoff_secs(3), 120.0);
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff_secs(10), 1800.0);
        assert_eq!(backoff_secs(100), 1800.0);
    }
}
