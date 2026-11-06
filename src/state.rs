use std::sync::Arc;

use axum::extract::FromRef;
use redis::aio::ConnectionManager;
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub redis: ConnectionManager,
    pub device_secret: Arc<Vec<u8>>,
    pub trust_proxy: bool,
}

// Lets handlers that only need the database keep using State<PgPool>,
// so read.rs and the health check need no changes.
impl FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> PgPool {
        state.pool.clone()
    }
}
