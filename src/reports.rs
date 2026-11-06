use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{device, error::AppError, grid, ratelimit, state::AppState, status};

const REPORT_WINDOW_SECS: i64 = 600; // 10 minutes
const REPORTS_PER_IP: i64 = 60;
const REPORTS_PER_DEVICE: i64 = 6;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportKind {
    Outage,
    Restored,
}

impl ReportKind {
    fn as_str(&self) -> &'static str {
        match self {
            ReportKind::Outage => "outage",
            ReportKind::Restored => "restored",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateReport {
    device_token: String,
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

    let device_id = device::verify_token(&state.device_secret, &body.device_token)
        .ok_or_else(|| AppError::Unauthorized("invalid device token".to_string()))?;

    ratelimit::check(
        &state.redis,
        &format!("rl:reports:dev:{device_id}"),
        REPORTS_PER_DEVICE,
        REPORT_WINDOW_SECS,
    )
    .await?;

    if !body.lat.is_finite()
        || !body.lng.is_finite()
        || !grid::in_bangladesh(body.lat, body.lng)
    {
        return Err(AppError::Validation(
            "location must be inside Bangladesh".to_string(),
        ));
    }

    let cell = grid::cell_for(body.lat, body.lng);

    let mut tx = state.pool.begin().await?;

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
        "INSERT INTO reports (cell_id, device_id, kind, lat, lng)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(cell_id)
    .bind(device_id)
    .bind(body.kind.as_str())
    .bind(body.lat)
    .bind(body.lng)
    .fetch_one(&mut *tx)
    .await?;

    let cell_status = status::evaluate_cell(&mut *tx, cell_id).await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(ReportCreated {
            report_id,
            cell_id,
            lat_idx: cell.lat_idx,
            lng_idx: cell.lng_idx,
            cell_status: cell_status.as_str(),
        }),
    ))
}