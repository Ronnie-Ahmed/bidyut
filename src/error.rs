use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    Validation(String),
    Unauthorized(String),
    NotFound,
    RateLimited { retry_after_secs: u64 },
    Database(sqlx::Error),
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Database(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Validation(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": msg })),
            )
                .into_response(),
            AppError::Unauthorized(msg) => {
                (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg }))).into_response()
            }
            AppError::NotFound => {
                (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
            }
            AppError::RateLimited { retry_after_secs } => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_after_secs.to_string())],
                Json(json!({ "error": "too many requests" })),
            )
                .into_response(),
            AppError::Database(e) => {
                tracing::error!("database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal server error" })),
                )
                    .into_response()
            }
        }
    }
}
