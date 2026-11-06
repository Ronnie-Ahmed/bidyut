use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{error::AppError, grid};

fn validate_location(lat: f64, lng: f64) -> Result<grid::CellIndex, AppError> {
    if !lat.is_finite() || !lng.is_finite() || !grid::in_bangladesh(lat, lng) {
        return Err(AppError::Validation(
            "location must be inside Bangladesh".to_string(),
        ));
    }
    Ok(grid::cell_for(lat, lng))
}

async fn find_cell_id(pool: &PgPool, cell: grid::CellIndex) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM cells WHERE lat_idx = $1 AND lng_idx = $2")
        .bind(cell.lat_idx)
        .bind(cell.lng_idx)
        .fetch_optional(pool)
        .await
}

// ---------- GET /status ----------

#[derive(Debug, Deserialize)]
pub struct LocationQuery {
    lat: f64,
    lng: f64,
}

#[derive(Serialize)]
pub struct StatusResponse {
    known: bool,
    cell_id: Option<i64>,
    lat_idx: i32,
    lng_idx: i32,
    status: String,
    status_since: Option<DateTime<Utc>>,
}

pub async fn get_status(
    State(pool): State<PgPool>,
    Query(q): Query<LocationQuery>,
) -> Result<Json<StatusResponse>, AppError> {
    let cell = validate_location(q.lat, q.lng)?;

    let row: Option<(i64, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, status, status_since FROM cells WHERE lat_idx = $1 AND lng_idx = $2",
    )
    .bind(cell.lat_idx)
    .bind(cell.lng_idx)
    .fetch_optional(&pool)
    .await?;

    let response = match row {
        Some((id, status, since)) => StatusResponse {
            known: true,
            cell_id: Some(id),
            lat_idx: cell.lat_idx,
            lng_idx: cell.lng_idx,
            status,
            status_since: Some(since),
        },
        None => StatusResponse {
            known: false,
            cell_id: None,
            lat_idx: cell.lat_idx,
            lng_idx: cell.lng_idx,
            status: "up".to_string(),
            status_since: None,
        },
    };

    Ok(Json(response))
}

// ---------- GET /history ----------

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    lat: f64,
    lng: f64,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

#[derive(Serialize)]
pub struct EventView {
    id: i64,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ongoing: bool,
    duration_secs: i64,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    events: Vec<EventView>,
}

pub async fn get_history(
    State(pool): State<PgPool>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, AppError> {
    let cell = validate_location(q.lat, q.lng)?;
    let limit = q.limit.clamp(1, 100);

    let Some(cell_id) = find_cell_id(&pool, cell).await? else {
        return Ok(Json(HistoryResponse { events: vec![] }));
    };

    let rows: Vec<(i64, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, started_at, ended_at FROM outage_events
         WHERE cell_id = $1
         ORDER BY started_at DESC
         LIMIT $2",
    )
    .bind(cell_id)
    .bind(limit)
    .fetch_all(&pool)
    .await?;

    let now = Utc::now();
    let events = rows
        .into_iter()
        .map(|(id, started_at, ended_at)| EventView {
            id,
            started_at,
            ended_at,
            ongoing: ended_at.is_none(),
            duration_secs: (ended_at.unwrap_or(now) - started_at).num_seconds().max(0),
        })
        .collect();

    Ok(Json(HistoryResponse { events }))
}

// ---------- GET /stats ----------

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    lat: f64,
    lng: f64,
    #[serde(default = "default_days")]
    days: i32,
}

fn default_days() -> i32 {
    30
}

#[derive(Serialize)]
pub struct StatsResponse {
    known: bool,
    window_days: i32,
    outage_count: i64,
    total_downtime_secs: i64,
    avg_outage_secs: i64,
    /// Index 0..23 is the hour of day in Asia/Dhaka time.
    outages_by_hour_dhaka: Vec<i64>,
}

pub async fn get_stats(
    State(pool): State<PgPool>,
    Query(q): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, AppError> {
    let cell = validate_location(q.lat, q.lng)?;
    let days = q.days.clamp(1, 365);

    let Some(cell_id) = find_cell_id(&pool, cell).await? else {
        return Ok(Json(StatsResponse {
            known: false,
            window_days: days,
            outage_count: 0,
            total_downtime_secs: 0,
            avg_outage_secs: 0,
            outages_by_hour_dhaka: vec![0; 24],
        }));
    };

    let (outage_count, total_downtime): (i64, f64) = sqlx::query_as(
        "SELECT
            COUNT(*),
            COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(ended_at, now()) - started_at))), 0)::float8
         FROM outage_events
         WHERE cell_id = $1
           AND started_at >= now() - make_interval(days => $2)",
    )
    .bind(cell_id)
    .bind(days)
    .fetch_one(&pool)
    .await?;

    let hour_rows: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT EXTRACT(HOUR FROM started_at AT TIME ZONE 'Asia/Dhaka')::int, COUNT(*)
         FROM outage_events
         WHERE cell_id = $1
           AND started_at >= now() - make_interval(days => $2)
         GROUP BY 1
         ORDER BY 1",
    )
    .bind(cell_id)
    .bind(days)
    .fetch_all(&pool)
    .await?;

    let mut by_hour = vec![0i64; 24];
    for (hour, count) in hour_rows {
        if let Some(slot) = by_hour.get_mut(hour as usize) {
            *slot = count;
        }
    }

    let total_downtime_secs = total_downtime as i64;
    let avg_outage_secs = if outage_count > 0 {
        total_downtime_secs / outage_count
    } else {
        0
    };

    Ok(Json(StatsResponse {
        known: true,
        window_days: days,
        outage_count,
        total_downtime_secs,
        avg_outage_secs,
        outages_by_hour_dhaka: by_hour,
    }))
}
