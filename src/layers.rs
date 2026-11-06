use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Turns ANY error response that isn't already JSON (404, 405, 413, extractor
/// rejections, panics, timeouts) into {"error": "..."}.
/// 4xx keeps the framework's message because it describes the client's mistake.
/// 5xx never exposes body text, only the status reason.
pub async fn json_errors(req: Request, next: Next) -> Response {
    let res = next.run(req).await;
    let status = res.status();

    let is_error = status.is_client_error() || status.is_server_error();
    let is_json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false);

    if !is_error || is_json {
        return res;
    }

    let (mut parts, body) = res.into_parts();

    let message = if status.is_server_error() {
        status
            .canonical_reason()
            .unwrap_or("server error")
            .to_lowercase()
    } else {
        let bytes = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        if text.is_empty() {
            status
                .canonical_reason()
                .unwrap_or("client error")
                .to_lowercase()
        } else {
            text
        }
    };

    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    Response::from_parts(parts, Body::from(json!({ "error": message }).to_string()))
}

/// Hard cap on how long any request may run.
pub async fn request_timeout(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(timeout_secs = limit.as_secs(), "request timed out");
            metrics::counter!("request_timeouts_total").increment(1);
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// Request count and latency per route. The label is the route *pattern*
/// (e.g. /subscriptions/{id}), never the raw URL, so label cardinality stays bounded.
pub async fn track_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_string());

    let res = next.run(req).await;

    let status = res.status().as_u16().to_string();
    let elapsed = start.elapsed().as_secs_f64();

    metrics::counter!(
        "http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status
    )
    .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "path" => path
    )
    .record(elapsed);

    res
}
