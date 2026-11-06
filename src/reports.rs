use std::net::SocketAddr;

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    device,
    error::AppError,
    grid, ratelimit,
    state::AppState,
    status::{self, Status},
};

const REPORT_WINDOW_SECS: i64 = 600; // 10 minutes
const REPORTS_PER_IP: i64 = 60;
const REPORTS_PER_DEVICE: i64 = 6;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportKind {
    Outage,
    Restored,
}

impl ReportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReportKind::Outage => "outage",
            ReportKind::Restored => "restored",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateReport {
    kind: ReportKind,
    lat: f64,
    lng: f64,
}

#[derive(Serialize)]
pub struct ReportCreated {
    report_id: i64,
    cell_id: i64,
    lat_idx: i32,
    lng_idx: i32,
    cell_status: &'static str,
}

pub struct Recorded {
    pub report_id: i64,
    pub cell_id: i64,
    pub cell: grid::CellIndex,
    pub status: Status,
}

/// The core of a report: validate the location, store the report, and
/// re-evaluate the cell, all in one transaction. No HTTP concerns in here.
pub async fn record_report(
    pool: &PgPool,
    device_id: Uuid,
    kind: ReportKind,
    lat: f64,
    lng: f64,
) -> Result<Recorded, AppError> {
    if !lat.is_finite() || !lng.is_finite() || !grid::in_bangladesh(lat, lng) {
        return Err(AppError::Validation(
            "location must be inside Bangladesh".to_string(),
        ));
    }

    let cell = grid::cell_for(lat, lng);

    let mut tx = pool.begin().await?;

    // This upsert takes the cell's row lock, so concurrent reports for the
    // same cell are evaluated one after another.
    let cell_id: i64 = sqlx::query_scalar(
        "INSERT INTO cells (lat_idx, lng_idx) VALUES ($1, $2)
         ON CONFLICT (lat_idx, lng_idx) DO UPDATE SET lat_idx = EXCLUDED.lat_idx
         RETURNING id",
    )
    .bind(cell.lat_idx)
    .bind(cell.lng_idx)
    .fetch_one(&mut *tx)
    .await?;

    let report_id: i64 = sqlx::query_scalar(
        "INSERT INTO reports (cell_id, device_id, kind, lat, lng, created_at)
         VALUES ($1, $2, $3, $4, $5, clock_timestamp())
         RETURNING id",
    )
    .bind(cell_id)
    .bind(device_id)
    .bind(kind.as_str())
    .bind(lat)
    .bind(lng)
    .fetch_one(&mut *tx)
    .await?;

    let status = status::evaluate_cell(&mut *tx, cell_id).await?;

    tx.commit().await?;

    metrics::counter!("reports_total", "kind" => kind.as_str()).increment(1);

    Ok(Recorded {
        report_id,
        cell_id,
        cell,
        status,
    })
}

pub async fn create_report(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<CreateReport>,
) -> Result<(StatusCode, Json<ReportCreated>), AppError> {
    // Cheapest checks first: per-IP limit, then token, then per-device limit.
    let ip = ratelimit::client_ip(peer, &headers, state.trust_proxy);
    ratelimit::check(
        &state.redis,
        &format!("rl:reports:ip:{}", ratelimit::ip_key(ip)),
        REPORTS_PER_IP,
        REPORT_WINDOW_SECS,
    )
    .await?;

    let device_id = device::device_from_headers(&state.device_secret, &headers)?;

    ratelimit::check(
        &state.redis,
        &format!("rl:reports:dev:{device_id}"),
        REPORTS_PER_DEVICE,
        REPORT_WINDOW_SECS,
    )
    .await?;

    let recorded = record_report(&state.pool, device_id, body.kind, body.lat, body.lng).await?;

    Ok((
        StatusCode::CREATED,
        Json(ReportCreated {
            report_id: recorded.report_id,
            cell_id: recorded.cell_id,
            lat_idx: recorded.cell.lat_idx,
            lng_idx: recorded.cell.lng_idx,
            cell_status: recorded.status.as_str(),
        }),
    ))
}
