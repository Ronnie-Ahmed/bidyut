use sqlx::{
    PgConnection,
    types::chrono::{DateTime, Utc},
};

const WINDOW_SECS: f64 = 600.0; // 10 minutes
const DOWN_THRESHOLD: i64 = 3;
const UP_THRESHOLD: i64 = 2;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Status {
    Up,
    Down,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Up => "up",
            Status::Down => "down",
        }
    }

    // The CHECK constraint on cells.status guarantees only 'up' or 'down' exist.
    fn from_db(s: &str) -> Status {
        match s {
            "down" => Status::Down,
            _ => Status::Up,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    GoDown,
    GoUp,
    NoChange,
}

/// Pure decision logic: no database, easy to test.
pub fn decide(current: Status, down_votes: i64, up_votes: i64) -> Decision {
    match current {
        Status::Up if down_votes >= DOWN_THRESHOLD => Decision::GoDown,
        Status::Down if up_votes >= UP_THRESHOLD => Decision::GoUp,
        _ => Decision::NoChange,
    }
}

const VOTES_SQL: &str = "
    WITH latest AS (
        SELECT DISTINCT ON (device_id) device_id, kind, created_at
        FROM reports
        WHERE cell_id = $1
          AND created_at > now() - make_interval(secs => $2)
          AND created_at >= $3
        ORDER BY device_id, created_at DESC, id DESC
    )
    SELECT
        COUNT(*) FILTER (WHERE kind = 'outage')   AS down_votes,
        COUNT(*) FILTER (WHERE kind = 'restored') AS up_votes,
        MIN(created_at) FILTER (WHERE kind = 'outage') AS first_down_at
    FROM latest";

/// Re-evaluates one cell. Must be called inside the transaction that
/// inserted the report.
///
/// LOCKING: the caller must already hold the cell's row lock. The report
/// handler gets it from the `INSERT ... ON CONFLICT DO UPDATE` on `cells`,
/// which runs before the report insert. Do not insert a report and call this
/// without locking the cell first: the foreign key takes a weaker lock that
/// conflicts with the FOR UPDATE below, and two concurrent requests can deadlock.
pub async fn evaluate_cell(conn: &mut PgConnection, cell_id: i64) -> Result<Status, sqlx::Error> {
    // Locks the cell row, so concurrent reports for this cell queue up here.
    let (current, status_since): (String, DateTime<Utc>) =
        sqlx::query_as("SELECT status, status_since FROM cells WHERE id = $1 FOR UPDATE")
            .bind(cell_id)
            .fetch_one(&mut *conn)
            .await?;
    let current = Status::from_db(&current);

    let (down_votes, up_votes, first_down_at): (i64, i64, Option<DateTime<Utc>>) =
        sqlx::query_as(VOTES_SQL)
            .bind(cell_id)
            .bind(WINDOW_SECS)
            .bind(status_since)
            .fetch_one(&mut *conn)
            .await?;

    match decide(current, down_votes, up_votes) {
        Decision::GoDown => {
            let started_at = first_down_at.unwrap_or_else(Utc::now);

            sqlx::query(
                "UPDATE cells SET status = 'down', status_since = clock_timestamp() WHERE id = $1",
            )
            .bind(cell_id)
            .execute(&mut *conn)
            .await?;

            sqlx::query("INSERT INTO outage_events (cell_id, started_at) VALUES ($1, $2)")
                .bind(cell_id)
                .bind(started_at)
                .execute(&mut *conn)
                .await?;

            let queued = enqueue_notifications(&mut *conn, cell_id, "down").await?;
            tracing::info!(cell_id, down_votes, queued, "cell went down");
            metrics::counter!("outages_started_total").increment(1);
            Ok(Status::Down)
        }
        Decision::GoUp => {
            sqlx::query(
                "UPDATE cells SET status = 'up', status_since = clock_timestamp() WHERE id = $1",
            )
            .bind(cell_id)
            .execute(&mut *conn)
            .await?;

            sqlx::query(
                "UPDATE outage_events SET ended_at = clock_timestamp(), closed_by = 'reports'
                 WHERE cell_id = $1 AND ended_at IS NULL",
            )
            .bind(cell_id)
            .execute(&mut *conn)
            .await?;

            let queued = enqueue_notifications(&mut *conn, cell_id, "restored").await?;
            tracing::info!(cell_id, up_votes, queued, "cell restored");
            metrics::counter!("outages_restored_total").increment(1);
            Ok(Status::Up)
        }
        Decision::NoChange => Ok(current),
    }
}

/// Queues one notification per subscriber of the cell. Call it inside the
/// same transaction that changes the status (the "outbox" pattern).
pub async fn enqueue_notifications(
    conn: &mut PgConnection,
    cell_id: i64,
    kind: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO notifications (subscription_id, cell_id, kind)
         SELECT id, cell_id, $2 FROM subscriptions WHERE cell_id = $1",
    )
    .bind(cell_id)
    .bind(kind)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_down_votes_is_not_enough() {
        assert_eq!(decide(Status::Up, 2, 0), Decision::NoChange);
    }

    #[test]
    fn three_down_votes_takes_cell_down() {
        assert_eq!(decide(Status::Up, 3, 0), Decision::GoDown);
    }

    #[test]
    fn restored_votes_do_nothing_while_up() {
        assert_eq!(decide(Status::Up, 0, 5), Decision::NoChange);
    }

    #[test]
    fn one_restored_vote_is_not_enough() {
        assert_eq!(decide(Status::Down, 3, 1), Decision::NoChange);
    }

    #[test]
    fn two_restored_votes_bring_cell_back() {
        assert_eq!(decide(Status::Down, 3, 2), Decision::GoUp);
    }

    #[test]
    fn more_outage_reports_never_close_a_down_cell() {
        assert_eq!(decide(Status::Down, 10, 0), Decision::NoChange);
    }
}
