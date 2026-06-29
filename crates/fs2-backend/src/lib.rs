#![allow(
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate
)]
//! Backend HTTP server skeleton.

use axum::{http::StatusCode, routing::get, Router};
use std::{
    env, fmt,
    future::Future,
    net::{AddrParseError, SocketAddr},
    path::PathBuf,
};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{fmt as tracing_fmt, EnvFilter};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:3000";
pub const DEFAULT_DATABASE_URL: &str = "postgres://fs2:fs2@localhost:5432/fs2";
pub const DEFAULT_OBJECT_STORE: &str = "local:./.fs2-dev/blobs";
const DEFAULT_DEV_SECRET: &str = "dev-only-secret-change-before-production";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendConfig {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub jwt_secret: RedactedSecret,
    pub session_secret: RedactedSecret,
}

impl BackendConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    pub fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let bind_addr = lookup("FS2_BIND_ADDR")
            .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned())
            .parse()
            .map_err(ConfigError::BindAddress)?;
        let database_url =
            lookup("DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned());
        let object_store = ObjectStoreConfig::parse(
            lookup("FS2_OBJECT_STORE")
                .as_deref()
                .unwrap_or(DEFAULT_OBJECT_STORE),
        )?;
        let jwt_secret = RedactedSecret::new(
            lookup("FS2_JWT_SECRET").unwrap_or_else(|| DEFAULT_DEV_SECRET.to_owned()),
        )?;
        let session_secret = RedactedSecret::new(
            lookup("FS2_SESSION_SECRET").unwrap_or_else(|| DEFAULT_DEV_SECRET.to_owned()),
        )?;
        Ok(Self {
            bind_addr,
            database_url,
            object_store,
            jwt_secret,
            session_secret,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectStoreConfig {
    Local { root: PathBuf },
    S3 { bucket: String, endpoint: String },
}

impl ObjectStoreConfig {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        if let Some(root) = value.strip_prefix("local:") {
            if root.is_empty() {
                return Err(ConfigError::ObjectStore(
                    "local object store root must not be empty".to_owned(),
                ));
            }
            return Ok(Self::Local {
                root: PathBuf::from(root),
            });
        }

        if let Some(rest) = value.strip_prefix("s3:") {
            let (bucket, endpoint) = rest.split_once('@').ok_or_else(|| {
                ConfigError::ObjectStore(
                    "S3 object store must be s3:<bucket>@<endpoint>".to_owned(),
                )
            })?;
            if bucket.is_empty() || endpoint.is_empty() {
                return Err(ConfigError::ObjectStore(
                    "S3 object store bucket and endpoint must not be empty".to_owned(),
                ));
            }
            return Ok(Self::S3 {
                bucket: bucket.to_owned(),
                endpoint: endpoint.to_owned(),
            });
        }

        Err(ConfigError::ObjectStore(
            "object store must start with local: or s3:".to_owned(),
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RedactedSecret(String);

impl RedactedSecret {
    fn new(value: String) -> Result<Self, ConfigError> {
        if value.is_empty() {
            return Err(ConfigError::Secret("secret must not be empty".to_owned()));
        }
        Ok(Self(value))
    }

    pub fn expose_for_signing(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug)]
pub enum ConfigError {
    BindAddress(AddrParseError),
    ObjectStore(String),
    Secret(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BindAddress(error) => write!(formatter, "invalid bind address: {error}"),
            Self::ObjectStore(message) | Self::Secret(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

pub fn app() -> Router {
    Router::new().route("/healthz", get(healthz))
}

pub async fn serve(
    config: BackendConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(bind_addr = %config.bind_addr, "fs2-backend listening");
    axum::serve(listener, app())
        .with_graceful_shutdown(shutdown)
        .await
}

pub fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fs2_backend=info"));
    let _ = tracing_fmt().with_env_filter(filter).try_init();
}

async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "OK")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::util::ServiceExt;

    #[test]
    fn config_loads_defaults_and_redacts_secrets() -> Result<(), Box<dyn std::error::Error>> {
        let config = BackendConfig::from_lookup(|_| None)?;

        assert_eq!(config.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(config.database_url, DEFAULT_DATABASE_URL);
        assert!(matches!(
            config.object_store,
            ObjectStoreConfig::Local { .. }
        ));
        assert!(format!("{config:?}").contains("<redacted>"));
        assert!(!format!("{config:?}").contains(DEFAULT_DEV_SECRET));
        Ok(())
    }

    #[test]
    fn config_parses_object_store_and_rejects_bad_values() {
        let s3 = ObjectStoreConfig::parse("s3:bucket@http://localhost:9000");
        assert!(matches!(s3, Ok(ObjectStoreConfig::S3 { .. })));
        assert!(ObjectStoreConfig::parse("s3:bucket").is_err());
        assert!(ObjectStoreConfig::parse("local:").is_err());
    }

    #[tokio::test]
    async fn healthz_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
        let response = app()
            .oneshot(Request::builder().uri("/healthz").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }
}
