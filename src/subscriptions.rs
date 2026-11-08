use crate::{device, error::AppError, grid, ratelimit, state::AppState};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

pub const SUBSCRIBE_PER_DEVICE_PER_HOUR: i64 = 20;
const MAX_SUBSCRIPTIONS_PER_DEVICE: i64 = 5;

#[derive(Debug, Deserialize)]
pub struct SubscribeBody {
    lat: f64,
    lng: f64,
}

#[derive(Serialize)]
pub struct SubscriptionView {
    id: i64,
    lat_idx: i32,
    lng_idx: i32,
    status: String,
}

#[derive(Serialize)]
pub struct SubscriptionList {
    subscriptions: Vec<SubscriptionView>,
}

/// The core of "subscribe": no HTTP concerns, shared by the API and the bot.
pub async fn add_subscription(
    pool: &PgPool,
    device_id: Uuid,
    cell: grid::CellIndex,
) -> Result<(i64, String), AppError> {
    let mut tx = pool.begin().await?;

    let (cell_id, status): (i64, String) = sqlx::query_as(
        "INSERT INTO cells (lat_idx, lng_idx) VALUES ($1, $2)
         ON CONFLICT (lat_idx, lng_idx) DO UPDATE SET lat_idx = EXCLUDED.lat_idx
         RETURNING id, status",
    )
    .bind(cell.lat_idx)
    .bind(cell.lng_idx)
    .fetch_one(&mut *tx)
    .await?;

    let others: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subscriptions WHERE device_id = $1 AND cell_id <> $2",
    )
    .bind(device_id)
    .bind(cell_id)
    .fetch_one(&mut *tx)
    .await?;

    if others >= MAX_SUBSCRIPTIONS_PER_DEVICE {
        return Err(AppError::Validation(format!(
            "subscription limit reached ({MAX_SUBSCRIPTIONS_PER_DEVICE} per device)"
        )));
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO subscriptions (device_id, cell_id) VALUES ($1, $2)
         ON CONFLICT (device_id, cell_id) DO UPDATE SET device_id = EXCLUDED.device_id
         RETURNING id",
    )
    .bind(device_id)
    .bind(cell_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((id, status))
}

pub async fn subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Result<Json<SubscriptionView>, AppError> {
    let device_id = device::device_from_headers(&state.device_secret, &headers)?;

    ratelimit::check(
        &state.redis,
        &format!("rl:subs:dev:{device_id}"),
        SUBSCRIBE_PER_DEVICE_PER_HOUR,
        3600,
    )
    .await?;

    if !body.lat.is_finite() || !body.lng.is_finite() || !grid::in_bangladesh(body.lat, body.lng) {
        return Err(AppError::Validation(
            "location must be inside Bangladesh".to_string(),
        ));
    }
    let cell = grid::cell_for(body.lat, body.lng);

    let (id, status) = add_subscription(&state.pool, device_id, cell).await?;

    Ok(Json(SubscriptionView {
        id,
        lat_idx: cell.lat_idx,
        lng_idx: cell.lng_idx,
        status,
    }))
}
pub async fn list_subscriptions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SubscriptionList>, AppError> {
    let device_id = device::device_from_headers(&state.device_secret, &headers)?;

    let rows: Vec<(i64, i32, i32, String)> = sqlx::query_as(
        "SELECT s.id, c.lat_idx, c.lng_idx, c.status
         FROM subscriptions s
         JOIN cells c ON c.id = s.cell_id
         WHERE s.device_id = $1
         ORDER BY s.id",
    )
    .bind(device_id)
    .fetch_all(&state.pool)
    .await?;

    let subscriptions = rows
        .into_iter()
        .map(|(id, lat_idx, lng_idx, status)| SubscriptionView {
            id,
            lat_idx,
            lng_idx,
            status,
        })
        .collect();

    Ok(Json(SubscriptionList { subscriptions }))
}

pub async fn unsubscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let device_id = device::device_from_headers(&state.device_secret, &headers)?;

    // Matching on device_id too means you can't delete someone else's
    // subscription, and the 404 doesn't reveal whether the id exists.
    let result = sqlx::query("DELETE FROM subscriptions WHERE id = $1 AND device_id = $2")
        .bind(id)
        .bind(device_id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
