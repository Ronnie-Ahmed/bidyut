mod error;
mod grid;
mod read;
mod reports;
mod status;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use sqlx::{PgPool, postgres::PgPoolOptions};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("failed to connect to database");

    let app = Router::new()
        .route("/health", get(health))
        .route("/reports", post(reports::create_report))
        .route("/status", get(read::get_status))
        .route("/history", get(read::get_history))
        .route("/stats", get(read::get_stats))
        .with_state(pool);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind port 3000");

    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.expect("server error");
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
