use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use uuid::Uuid;

use crate::{error::AppError, ratelimit, state::AppState};

type HmacSha256 = Hmac<Sha256>;

const DEVICES_PER_IP_PER_HOUR: i64 = 10;

fn sign(secret: &[u8], id: &Uuid) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(id.as_bytes());
    mac
}

/// Token format: "<uuid>.<hex hmac of the uuid>"
pub fn issue_token(secret: &[u8]) -> String {
    let id = Uuid::new_v4();
    let signature = hex::encode(sign(secret, &id).finalize().into_bytes());
    format!("{id}.{signature}")
}

/// Returns the device id only if the signature is valid.
/// verify_slice compares in constant time.
pub fn verify_token(secret: &[u8], token: &str) -> Option<Uuid> {
    let (id_part, sig_part) = token.split_once('.')?;
    let id = Uuid::parse_str(id_part).ok()?;
    let signature = hex::decode(sig_part).ok()?;
    sign(secret, &id).verify_slice(&signature).ok()?;
    Some(id)
}

#[derive(Serialize)]
pub struct DeviceCreated {
    device_token: String,
}

pub async fn register_device(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<DeviceCreated>), AppError> {
    let ip = ratelimit::client_ip(peer, &headers, state.trust_proxy);

    ratelimit::check(
        &state.redis,
        &format!("rl:devices:ip:{}", ratelimit::ip_key(ip)),
        DEVICES_PER_IP_PER_HOUR,
        3600,
    )
    .await?;

    let device_token = issue_token(&state.device_secret);
    Ok((StatusCode::CREATED, Json(DeviceCreated { device_token })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-test-secret-test-secret";

    #[test]
    fn issued_token_verifies() {
        let token = issue_token(SECRET);
        assert!(verify_token(SECRET, &token).is_some());
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let token = issue_token(SECRET);
        let (id, sig) = token.split_once('.').unwrap();
        let mut sig = sig.to_string();
        let flipped = if sig.starts_with('0') { '1' } else { '0' };
        sig.replace_range(0..1, &flipped.to_string());
        assert!(verify_token(SECRET, &format!("{id}.{sig}")).is_none());
    }

    #[test]
    fn token_from_another_secret_is_rejected() {
        let token = issue_token(b"some-other-secret-some-other-secret");
        assert!(verify_token(SECRET, &token).is_none());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(verify_token(SECRET, "").is_none());
        assert!(verify_token(SECRET, "not-a-token").is_none());
        assert!(verify_token(SECRET, "a.b").is_none());
    }
}