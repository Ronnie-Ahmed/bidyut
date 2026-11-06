mod device;
mod error;
mod grid;
mod ratelimit;
mod read;
mod reports;
mod state;
mod status;
mod subscriptions;
mod worker;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get, post},
};
use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::state::AppState;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let redis_url = std::env::var("REDIS_URL").expect("REDIS_URL must be set");
    let secret = std::env::var("DEVICE_TOKEN_SECRET").expect("DEVICE_TOKEN_SECRET must be set");
    assert!(
        secret.len() >= 32,
        "DEVICE_TOKEN_SECRET must be at least 32 characters"
    );
    let trust_proxy = std::env::var("TRUST_PROXY")
        .map(|v| v == "true")
        .unwrap_or(false);

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("failed to connect to database");

    let redis = redis::Client::open(redis_url)
        .expect("invalid REDIS_URL")
        .get_connection_manager()
        .await
        .expect("failed to connect to redis");

    let state = AppState {
        pool,
        redis,
        device_secret: Arc::new(secret.into_bytes()),
        trust_proxy,
    };
    worker::spawn(state.pool.clone());

    let app = Router::new()
        .route("/health", get(health))
        .route("/devices", post(device::register_device))
        .route("/reports", post(reports::create_report))
        .route("/status", get(read::get_status))
        .route("/history", get(read::get_history))
        .route("/stats", get(read::get_stats))
        .route(
            "/subscriptions",
            post(subscriptions::subscribe).get(subscriptions::list_subscriptions),
        )
        .route("/subscriptions/{id}", delete(subscriptions::unsubscribe))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind port 3000");

    tracing::info!("listening on 0.0.0.0:3000 (trust_proxy={trust_proxy})");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server error");
}

async fn health(State(pool): State<PgPool>) -> StatusCode {
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::error!("health check failed: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}
