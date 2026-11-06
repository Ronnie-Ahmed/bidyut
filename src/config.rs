use std::{net::SocketAddr, str::FromStr, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

/// Deliberately no Debug derive: this struct holds secrets.
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub device_token_secret: String,
    pub trust_proxy: bool,
    pub bind_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub log_format: LogFormat,
    pub db_max_connections: u32,
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    pub run_migrations: bool,
}

/// Trimmed value, with empty treated as unset.
fn value(get: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn required(get: &dyn Fn(&str) -> Option<String>, key: &str, errors: &mut Vec<String>) -> String {
    value(get, key).unwrap_or_else(|| {
        errors.push(format!("{key} is required"));
        String::new()
    })
}

fn parsed<T: FromStr + Copy>(
    get: &dyn Fn(&str) -> Option<String>,
    key: &str,
    default: T,
    errors: &mut Vec<String>,
) -> T
where
    T::Err: std::fmt::Display,
{
    match value(get, key) {
        None => default,
        Some(raw) => match raw.parse::<T>() {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{key}: invalid value ({e})"));
                default
            }
        },
    }
}

fn in_range<T: PartialOrd + std::fmt::Display + Copy>(
    key: &str,
    v: T,
    min: T,
    max: T,
    errors: &mut Vec<String>,
) {
    if v < min || v > max {
        errors.push(format!("{key} must be between {min} and {max} (got {v})"));
    }
}

impl Config {
    pub fn from_env() -> Result<Config, Vec<String>> {
        Config::from_lookup(&|key| std::env::var(key).ok())
    }

    /// Takes a lookup function so tests don't have to touch real env vars.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Config, Vec<String>> {
        let mut errors = Vec::new();

        let database_url = required(get, "DATABASE_URL", &mut errors);
        if !database_url.is_empty()
            && !(database_url.starts_with("postgres://")
                || database_url.starts_with("postgresql://"))
        {
            errors.push("DATABASE_URL must start with postgres:// or postgresql://".into());
        }

        let redis_url = required(get, "REDIS_URL", &mut errors);
        if !redis_url.is_empty()
            && !(redis_url.starts_with("redis://") || redis_url.starts_with("rediss://"))
        {
            errors.push("REDIS_URL must start with redis:// or rediss://".into());
        }

        let device_token_secret = required(get, "DEVICE_TOKEN_SECRET", &mut errors);
        if !device_token_secret.is_empty() && device_token_secret.len() < 32 {
            errors.push("DEVICE_TOKEN_SECRET must be at least 32 characters".into());
        }

        let trust_proxy = match value(get, "TRUST_PROXY").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(other) => {
                errors.push(format!(
                    "TRUST_PROXY must be \"true\" or \"false\" (got \"{other}\")"
                ));
                false
            }
        };
        let run_migrations = match value(get, "RUN_MIGRATIONS").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(other) => {
                errors.push(format!(
                    "RUN_MIGRATIONS must be \"true\" or \"false\" (got \"{other}\")"
                ));
                false
            }
        };

        let log_format = match value(get, "LOG_FORMAT").as_deref() {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(other) => {
                errors.push(format!(
                    "LOG_FORMAT must be \"pretty\" or \"json\" (got \"{other}\")"
                ));
                LogFormat::Pretty
            }
        };

        let bind_addr = parsed(
            get,
            "BIND_ADDR",
            SocketAddr::from(([0, 0, 0, 0], 3000)),
            &mut errors,
        );
        let metrics_addr = parsed(
            get,
            "METRICS_ADDR",
            SocketAddr::from(([127, 0, 0, 1], 9090)),
            &mut errors,
        );
        if bind_addr == metrics_addr {
            errors.push("BIND_ADDR and METRICS_ADDR must differ".into());
        }

        let db_max_connections = parsed(get, "DB_MAX_CONNECTIONS", 10u32, &mut errors);
        in_range(
            "DB_MAX_CONNECTIONS",
            db_max_connections,
            1,
            100,
            &mut errors,
        );

        let timeout_secs = parsed(get, "REQUEST_TIMEOUT_SECS", 10u64, &mut errors);
        in_range("REQUEST_TIMEOUT_SECS", timeout_secs, 1, 60, &mut errors);

        let max_body_bytes = parsed(get, "MAX_BODY_BYTES", 16_384usize, &mut errors);
        in_range(
            "MAX_BODY_BYTES",
            max_body_bytes,
            1_024,
            1_048_576,
            &mut errors,
        );

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Config {
            database_url,
            redis_url,
            device_token_secret,
            trust_proxy,
            bind_addr,
            metrics_addr,
            log_format,
            db_max_connections,
            request_timeout: Duration::from_secs(timeout_secs),
            max_body_bytes,
            run_migrations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn valid() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DATABASE_URL", "postgres://u:p@localhost/db"),
            ("REDIS_URL", "redis://localhost:6379"),
            ("DEVICE_TOKEN_SECRET", SECRET),
        ]
    }

    #[test]
    fn minimal_config_gets_safe_defaults() {
        let get = lookup(&valid());
        let cfg = Config::from_lookup(&get).expect("should be valid");
        assert_eq!(cfg.bind_addr.port(), 3000);
        assert!(!cfg.trust_proxy);
        assert_eq!(cfg.log_format, LogFormat::Pretty);
        assert_eq!(cfg.request_timeout, Duration::from_secs(10));
        assert!(cfg.metrics_addr.ip().is_loopback());
    }

    #[test]
    fn all_problems_are_reported_together() {
        let get = lookup(&[]);
        let errors = Config::from_lookup(&get).err().expect("should fail");
        assert_eq!(errors.len(), 3);
    }

    #[test]
    fn short_secret_is_rejected() {
        let mut pairs = valid();
        pairs.push(("DEVICE_TOKEN_SECRET", "short"));
        let get = lookup(&pairs);
        let errors = Config::from_lookup(&get).err().expect("should fail");
        assert!(errors.iter().any(|e| e.contains("DEVICE_TOKEN_SECRET")));
    }

    #[test]
    fn trust_proxy_must_be_strict() {
        let mut pairs = valid();
        pairs.push(("TRUST_PROXY", "yes"));
        let get = lookup(&pairs);
        let errors = Config::from_lookup(&get).err().expect("should fail");
        assert!(errors.iter().any(|e| e.contains("TRUST_PROXY")));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        let mut pairs = valid();
        pairs.push(("DB_MAX_CONNECTIONS", "0"));
        let get = lookup(&pairs);
        let errors = Config::from_lookup(&get).err().expect("should fail");
        assert!(errors.iter().any(|e| e.contains("DB_MAX_CONNECTIONS")));
    }
}
