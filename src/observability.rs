use std::{net::SocketAddr, time::Duration};

use axum::{Router, routing::get};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

pub fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init(),
        LogFormat::Pretty => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

pub fn install_metrics() -> PrometheusHandle {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Suffix("duration_seconds".to_string()),
            &[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
        )
        .expect("invalid histogram buckets")
        .install_recorder()
        .expect("failed to install metrics recorder")
}

/// Serves /metrics on its own internal address, never on the public port.
pub async fn serve_metrics(
    addr: SocketAddr,
    handle: PrometheusHandle,
    shutdown: CancellationToken,
) {
    // Histograms need periodic housekeeping when we don't use the built-in listener.
    let upkeep_handle = handle.clone();
    let upkeep_shutdown = shutdown.clone();
    let upkeep = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => upkeep_handle.run_upkeep(),
                _ = upkeep_shutdown.cancelled() => break,
            }
        }
    });

    let app = Router::new().route(
        "/metrics",
        get(move || {
            let handle = handle.clone();
            async move { handle.render() }
        }),
    );

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind metrics listener on {addr}: {e}");
            return;
        }
    };
    tracing::info!(%addr, "metrics listening");

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
    {
        tracing::error!("metrics server error: {e}");
    }
    let _ = upkeep.await;
}
