mod config;
mod device;
mod error;
mod grid;
mod health;
mod layers;
mod observability;
mod ratelimit;
mod read;
mod reports;
mod shutdown;
mod state;
mod status;
mod subscriptions;
mod worker;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::{from_fn, from_fn_with_state},
    routing::{delete, get, post},
};
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::{
    catch_panic::CatchPanicLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::Level;

use crate::{config::Config, state::AppState};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(errors) => {
            eprintln!("invalid configuration:");
            for e in errors {
                eprintln!("  - {e}");
            }
            std::process::exit(1);
        }
    };

    observability::init_logging(config.log_format);
    let metrics_handle = observability::install_metrics();

    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await
        .expect("failed to connect to database");

    let redis = redis::Client::open(config.redis_url.as_str())
        .expect("invalid REDIS_URL")
        .get_connection_manager()
        .await
        .expect("failed to connect to redis");

    let state = AppState {
        pool: pool.clone(),
        redis,
        device_secret: Arc::new(config.device_token_secret.clone().into_bytes()),
        trust_proxy: config.trust_proxy,
    };

    // One token tells everything to stop: the HTTP servers and the workers.
    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown::wait_for_signal().await;
            shutdown.cancel();
        }
    });

    let workers = worker::spawn(pool.clone(), shutdown.clone());
    let metrics_server = tokio::spawn(observability::serve_metrics(
        config.metrics_addr,
        metrics_handle,
        shutdown.clone(),
    ));

    let app = build_router(state, &config);

    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .expect("failed to bind api address");
    tracing::info!(
        addr = %config.bind_addr,
        trust_proxy = config.trust_proxy,
        "api listening"
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await
    .expect("server error");

    tracing::info!("http server stopped, draining background tasks");
    shutdown.cancel(); // in case serve returned for a reason other than a signal

    let drain = async {
        for w in workers {
            let _ = w.await;
        }
        let _ = metrics_server.await;
    };
    if tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .is_err()
    {
        tracing::warn!("background tasks did not stop in time");
    }

    pool.close().await;
    tracing::info!("shutdown complete");
}

fn build_router(state: AppState, config: &Config) -> Router {
    // Listed outermost first: a request passes through these top to bottom.
    let stack = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    let request_id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    // Path only. The query string holds lat/lng, so it is never logged.
                    tracing::info_span!(
                        "request",
                        method = %req.method(),
                        path = %req.uri().path(),
                        request_id = %request_id
                    )
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(from_fn(layers::track_metrics))
        .layer(from_fn(layers::json_errors))
        .layer(CatchPanicLayer::new())
        .layer(from_fn_with_state(
            config.request_timeout,
            layers::request_timeout,
        ));

    Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
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
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .layer(stack)
        .with_state(state)
}
