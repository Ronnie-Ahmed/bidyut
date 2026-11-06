use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};

use crate::state::AppState;

/// Liveness: the process is running. Never touches dependencies, so a
/// database blip can't make an orchestrator restart healthy instances.
pub async fn livez() -> StatusCode {
    StatusCode::OK
}

/// Readiness: can this instance serve traffic? Fails only when the database
/// is unreachable. Redis is reported but doesn't fail readiness, because the
/// rate limiter deliberately fails open when Redis is down.
pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let db_ok = matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query("SELECT 1").execute(&state.pool)
        )
        .await,
        Ok(Ok(_))
    );

    let mut conn = state.redis.clone();
    let redis_ok = matches!(
        tokio::time::timeout(Duration::from_secs(1), async {
            let reply: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
            reply
        })
        .await,
        Ok(Ok(_))
    );

    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({ "database": db_ok, "redis": redis_ok })),
    )
}
