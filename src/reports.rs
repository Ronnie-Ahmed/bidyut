use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppError, grid};

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
    device_id: Uuid,
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
}

pub async fn create_report(
    State(pool): State<PgPool>,
    Json(body): Json<CreateReport>,
) -> Result<(StatusCode, Json<ReportCreated>), AppError> {
    if !body.lat.is_finite() || !body.lng.is_finite() || !grid::in_bangladesh(body.lat, body.lng) {
        return Err(AppError::Validation(
            "location must be inside Bangladesh".to_string(),
        ));
    }

    let cell = grid::cell_for(body.lat, body.lng);

    let mut tx = pool.begin().await?;

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
    .bind(body.device_id)
    .bind(body.kind.as_str())
    .bind(body.lat)
    .bind(body.lng)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(ReportCreated {
            report_id,
            cell_id,
            lat_idx: cell.lat_idx,
            lng_idx: cell.lng_idx,
        }),
    ))
}
