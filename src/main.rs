use std::{future::Future, sync::Arc};

use anyhow::{Context, Result, ensure};
use nano_db::{Config, Store, app, server::LimitedListener};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        std::env::args_os().len() == 1,
        "command-line arguments are not supported; configure NANO_DB_* environment variables (see README.md)"
    );
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).context("invalid RUST_LOG")?,
        Err(std::env::VarError::NotPresent) => EnvFilter::new("nano_db=info"),
        Err(error) => return Err(error.into()),
    };
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initializing logs: {error}"))?;
    let config = Config::from_env()?;
    let shutdown = shutdown_signal()?;
    let store = Arc::new(Store::open(&config.path, config.cache_bytes)?);
    let listener = TcpListener::bind(config.bind)
        .await
        .context("binding HTTP listener")?;
    let address = listener.local_addr()?;
    let listener = LimitedListener::new(
        listener,
        config.max_connections,
        config.connection_idle_timeout,
    );
    let router = app(config, store)?;
    tracing::info!(address = %address, "nano-db listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serving HTTP")?;
    tracing::info!("nano-db stopped");
    Ok(())
}

#[cfg(unix)]
fn shutdown_signal() -> Result<impl Future<Output = ()>> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;
    Ok(async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(error = %error, "receiving Ctrl-C failed");
                }
            }
            _ = terminate.recv() => {}
        }
        tracing::info!("shutdown requested; draining accepted requests");
    })
}

#[cfg(not(unix))]
fn shutdown_signal() -> Result<impl Future<Output = ()>> {
    Ok(async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %error, "receiving Ctrl-C failed");
        }
        tracing::info!("shutdown requested; draining accepted requests");
    })
}
