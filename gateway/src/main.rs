//! OZark tapd gateway.
//!
//! Runs on the Umbrel node next to litd/tapd. Holds the tapd macaroon locally and
//! exposes a small, NIP-98-authenticated HTTP API that the wallet reaches over the
//! node's Tor onion service. Enforces per-owner isolation on a shared tapd via a
//! SQLite ownership registry, so no client ever needs (or gets) the macaroon.
//!
//! Phase 1: skeleton + NIP-98 auth + read endpoints + ownership registry.

mod auth;
mod config;
mod error;
mod reconcile;
mod registry;
mod routes;
mod state;
mod tapd;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run().await {
        log::error!("gateway fatal: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cfg = config::Config::from_env()?;

    // Read the tapd secrets from local files — they never live in the binary.
    let cert_pem = std::fs::read_to_string(&cfg.tapd_cert_path)
        .map_err(|e| format!("read tapd cert {}: {e}", cfg.tapd_cert_path.display()))?;
    let macaroon_bytes = std::fs::read(&cfg.tapd_macaroon_path).map_err(|e| {
        format!(
            "read tapd macaroon {}: {e}",
            cfg.tapd_macaroon_path.display()
        )
    })?;
    let macaroon_hex = hex::encode(macaroon_bytes);

    let tapd = tapd::TapdClient::connect(&cfg.tapd_host, &cert_pem, &macaroon_hex).await?;
    let registry = Arc::new(registry::Registry::open(&cfg.db_path).map_err(|e| e.to_string())?);

    let state = state::AppState {
        tapd,
        registry,
        auth: state::AuthConfig {
            public_base_url: cfg.public_base_url.clone(),
            max_skew_secs: cfg.max_skew_secs,
        },
    };

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr)
        .await
        .map_err(|e| format!("bind {}: {e}", cfg.listen_addr))?;
    log::info!(
        "ozark-gateway listening on {} (tapd={}, url-binding={})",
        cfg.listen_addr,
        cfg.tapd_host,
        if cfg.public_base_url.is_some() {
            "strict"
        } else {
            "path-only"
        }
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("serve: {e}"))?;
    Ok(())
}

/// Resolve on Ctrl-C / SIGTERM so systemd/docker can stop the gateway cleanly.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    log::info!("shutdown signal received");
}
