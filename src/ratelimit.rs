use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::http::HeaderMap;
use redis::aio::ConnectionManager;

use crate::error::AppError;

/// If Redis is slower than this, we give up and let the request through.
const REDIS_TIMEOUT: Duration = Duration::from_millis(250);

/// The client's IP. Behind a trusted proxy we take the LAST X-Forwarded-For
/// entry, the one our own proxy appended. Earlier entries are client-supplied.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trust_proxy: bool) -> IpAddr {
    if trust_proxy
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer.ip()
}

/// IPv4 addresses are used as-is. IPv6 is reduced to its /64 prefix.
pub fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}", s[0], s[1], s[2], s[3])
        }
    }
}

/// Fixed-window counter. INCR, EXPIRE NX and TTL run as one atomic
/// transaction, so a crash can never leave a counter without an expiry.
pub async fn check(
    redis: &ConnectionManager,
    key: &str,
    limit: i64,
    window_secs: i64,
) -> Result<(), AppError> {
    let mut conn = redis.clone();

    let mut pipe = redis::pipe();
    pipe.atomic()
        .cmd("INCR")
        .arg(key)
        .cmd("EXPIRE")
        .arg(key)
        .arg(window_secs)
        .arg("NX")
        .cmd("TTL")
        .arg(key);

    let outcome: Result<redis::RedisResult<(i64, i64, i64)>, _> =
        tokio::time::timeout(REDIS_TIMEOUT, pipe.query_async(&mut conn)).await;

    match outcome {
        Ok(Ok((count, _, ttl))) => {
            if count > limit {
                // "rl:reports:ip:1.2.3.4" becomes the label "reports:ip"
                let scope: String = key.split(':').skip(1).take(2).collect::<Vec<_>>().join(":");
                metrics::counter!("rate_limited_total", "scope" => scope).increment(1);
                Err(AppError::RateLimited {
                    retry_after_secs: ttl.max(1) as u64,
                })
            } else {
                Ok(())
            }
        }
        Ok(Err(e)) => {
            metrics::counter!("rate_limiter_failures_total").increment(1);
            tracing::error!("rate limiter error, allowing request: {e}");
            Ok(())
        }
        Err(_) => {
            metrics::counter!("rate_limiter_failures_total").increment(1);
            tracing::error!("rate limiter timed out, allowing request");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_in_same_64_share_a_key() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(ip_key(a), ip_key(b));
        assert_ne!(ip_key(a), ip_key(c));
    }
}
