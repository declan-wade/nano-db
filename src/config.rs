use std::{collections::HashMap, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use anyhow::{Context, Result, bail, ensure};

pub struct Config {
    pub bind: SocketAddr,
    pub path: PathBuf,
    pub auth_token: Option<String>,
    pub allow_unauthenticated: bool,
    pub max_document_bytes: usize,
    pub max_depth: usize,
    pub cache_bytes: usize,
    pub max_in_flight: usize,
    pub max_queued: usize,
    pub queue_timeout: Duration,
    pub body_timeout: Duration,
    pub max_connections: usize,
    pub connection_idle_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 7070)),
            path: PathBuf::from("nano-db.redb"),
            auth_token: None,
            allow_unauthenticated: false,
            max_document_bytes: 1024 * 1024,
            max_depth: 64,
            cache_bytes: 16 * 1024 * 1024,
            max_in_flight: 16,
            max_queued: 64,
            queue_timeout: Duration::from_millis(100),
            body_timeout: Duration::from_secs(10),
            max_connections: 128,
            connection_idle_timeout: Duration::from_secs(30),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mut values = HashMap::new();
        for (key, value) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("NANO_DB_") {
                let key = key
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid configuration key"))?;
                let value = value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("{key} must contain valid UTF-8"))?;
                values.insert(key, value);
            }
        }
        Self::from_values(&values)
    }

    fn from_values(values: &HashMap<String, String>) -> Result<Self> {
        const KEYS: &[&str] = &[
            "NANO_DB_BIND",
            "NANO_DB_PATH",
            "NANO_DB_AUTH_TOKEN",
            "NANO_DB_ALLOW_UNAUTHENTICATED",
            "NANO_DB_MAX_DOCUMENT_BYTES",
            "NANO_DB_MAX_DEPTH",
            "NANO_DB_CACHE_BYTES",
            "NANO_DB_MAX_IN_FLIGHT",
            "NANO_DB_MAX_QUEUED",
            "NANO_DB_QUEUE_TIMEOUT_MS",
            "NANO_DB_BODY_TIMEOUT_MS",
            "NANO_DB_MAX_CONNECTIONS",
            "NANO_DB_CONNECTION_IDLE_TIMEOUT_MS",
        ];
        for key in values.keys() {
            ensure!(
                KEYS.contains(&key.as_str()),
                "unknown configuration key: {key}"
            );
        }
        let defaults = Self::default();
        let config = Self {
            bind: setting(values, "NANO_DB_BIND", defaults.bind)?,
            path: values
                .get("NANO_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or(defaults.path),
            auth_token: values.get("NANO_DB_AUTH_TOKEN").cloned(),
            allow_unauthenticated: setting(values, "NANO_DB_ALLOW_UNAUTHENTICATED", false)?,
            max_document_bytes: setting(
                values,
                "NANO_DB_MAX_DOCUMENT_BYTES",
                defaults.max_document_bytes,
            )?,
            max_depth: setting(values, "NANO_DB_MAX_DEPTH", defaults.max_depth)?,
            cache_bytes: setting(values, "NANO_DB_CACHE_BYTES", defaults.cache_bytes)?,
            max_in_flight: setting(values, "NANO_DB_MAX_IN_FLIGHT", defaults.max_in_flight)?,
            max_queued: setting(values, "NANO_DB_MAX_QUEUED", defaults.max_queued)?,
            queue_timeout: Duration::from_millis(setting(values, "NANO_DB_QUEUE_TIMEOUT_MS", 100)?),
            body_timeout: Duration::from_millis(setting(
                values,
                "NANO_DB_BODY_TIMEOUT_MS",
                10_000,
            )?),
            max_connections: setting(values, "NANO_DB_MAX_CONNECTIONS", defaults.max_connections)?,
            connection_idle_timeout: Duration::from_millis(setting(
                values,
                "NANO_DB_CONNECTION_IDLE_TIMEOUT_MS",
                30_000,
            )?),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.path.as_os_str().is_empty(),
            "NANO_DB_PATH must not be empty"
        );
        range(
            "NANO_DB_MAX_DOCUMENT_BYTES",
            self.max_document_bytes,
            1,
            64 * 1024 * 1024,
        )?;
        range("NANO_DB_MAX_DEPTH", self.max_depth, 1, 128)?;
        range(
            "NANO_DB_CACHE_BYTES",
            self.cache_bytes,
            1024 * 1024,
            1024 * 1024 * 1024,
        )?;
        range("NANO_DB_MAX_IN_FLIGHT", self.max_in_flight, 1, 1024)?;
        range("NANO_DB_MAX_QUEUED", self.max_queued, 0, 4096)?;
        range("NANO_DB_MAX_CONNECTIONS", self.max_connections, 1, 65_536)?;
        duration("NANO_DB_QUEUE_TIMEOUT_MS", self.queue_timeout, 1, 60_000)?;
        duration("NANO_DB_BODY_TIMEOUT_MS", self.body_timeout, 1, 300_000)?;
        duration(
            "NANO_DB_CONNECTION_IDLE_TIMEOUT_MS",
            self.connection_idle_timeout,
            100,
            300_000,
        )?;
        match &self.auth_token {
            Some(token) => {
                ensure!(
                    token.len() >= 32 && token.len() <= 1024,
                    "NANO_DB_AUTH_TOKEN must be 32..=1024 bytes"
                );
                let credential = token.trim_end_matches('=');
                ensure!(
                    !credential.is_empty()
                        && credential
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b)),
                    "NANO_DB_AUTH_TOKEN must be a valid bearer token"
                );
            }
            None if !self.allow_unauthenticated => {
                bail!(
                    "set NANO_DB_AUTH_TOKEN, or explicitly allow unauthenticated loopback development"
                );
            }
            None => ensure!(
                self.bind.ip().is_loopback(),
                "unauthenticated binding must be loopback"
            ),
        }
        Ok(())
    }
}

fn setting<T: FromStr>(values: &HashMap<String, String>, name: &str, default: T) -> Result<T> {
    match values.get(name) {
        Some(value) => value.parse().map_err(|_| anyhow::anyhow!("invalid {name}")),
        None => Ok(default),
    }
}

fn range(name: &str, value: usize, min: usize, max: usize) -> Result<()> {
    ensure!((min..=max).contains(&value), "{name} must be {min}..={max}");
    Ok(())
}

fn duration(name: &str, value: Duration, min: u64, max: u64) -> Result<()> {
    let millis = u64::try_from(value.as_millis()).context("duration is too large")?;
    ensure!(
        value == Duration::from_millis(millis) && (min..=max).contains(&millis),
        "{name} must be {min}..={max} milliseconds"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_defaults_and_explicit_development_mode() {
        assert!(Config::default().validate().is_err());
        let mut config = Config {
            allow_unauthenticated: true,
            ..Config::default()
        };
        config.validate().unwrap();
        config.bind = "0.0.0.0:7070".parse().unwrap();
        assert!(config.validate().is_err());
        config.auth_token = Some("a".repeat(32));
        config.validate().unwrap();
    }

    #[test]
    fn configuration_rejects_typos_invalid_values_and_limits() {
        let mut values = HashMap::from([("NANO_DB_ALLOW_UNAUTHENTICATED".into(), "true".into())]);
        assert!(Config::from_values(&values).is_ok());
        values.insert("NANO_DB_MAX_DEPTH".into(), "129".into());
        assert!(Config::from_values(&values).is_err());
        values.remove("NANO_DB_MAX_DEPTH");
        values.insert("NANO_DB_MAX_QUED".into(), "12".into());
        assert!(Config::from_values(&values).is_err());
        values.remove("NANO_DB_MAX_QUED");
        values.insert("NANO_DB_ALLOW_UNAUTHENTICATED".into(), "yes".into());
        assert!(Config::from_values(&values).is_err());
    }
}
