use axum::{routing::get,Router};
#[tokio::main]
async fn main(){
    tracing_subscriber::fmt::init();
    let app=Router::new().route("/health", get(|| async {"Ok"}));
    let listener=tokio::net::TcpListener::bind("0.0.0.0:3000").await.expect("failed to bind port 3000");
    tracing::info!("Listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.expect("server error");
}