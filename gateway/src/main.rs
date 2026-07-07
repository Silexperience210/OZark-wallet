//! OZark tapd gateway.
//!
//! Runs on the Umbrel node next to litd/tapd. Holds the tapd macaroon locally and
//! exposes a small, NIP-98-authenticated HTTP API that the wallet reaches over the
//! node's Tor onion service. Enforces per-owner isolation on a shared tapd via a
//! SQLite ownership registry, so no client ever needs (or gets) the macaroon.
//!
//! Phase 1: skeleton + NIP-98 auth + read endpoints + ownership registry.

mod auth;
mod backup;
mod config;
mod error;
mod fees;
mod reconcile;
mod registry;
mod routes;
mod state;
mod tapd;

use std::sync::Arc;
use std::time::Duration;

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

    // Optional lnd macaroon (invoices:read) for LN-receive settlement detection.
    let lnd_macaroon_hex = match &cfg.lnd_macaroon_path {
        Some(p) => {
            let bytes =
                std::fs::read(p).map_err(|e| format!("read lnd macaroon {}: {e}", p.display()))?;
            Some(hex::encode(bytes))
        }
        None => None,
    };

    let tapd = tapd::TapdClient::connect(
        &cfg.tapd_host,
        &cert_pem,
        &macaroon_hex,
        lnd_macaroon_hex.as_deref(),
    )
    .await?;
    let registry = Arc::new(registry::Registry::open(&cfg.db_path).map_err(|e| e.to_string())?);

    let state = state::AppState {
        tapd,
        registry,
        auth: state::AuthConfig {
            public_base_url: cfg.public_base_url.clone(),
            max_skew_secs: cfg.max_skew_secs,
            admin_pubkey: cfg.admin_pubkey.clone(),
            allow_admin_claim: cfg.allow_admin_claim,
        },
        fees: fees::FeePolicy {
            charge: cfg.charge_fees,
            margin_bps: cfg.fee_margin_bps,
            floor_sats: cfg.fee_floor_sats,
            mint_vsize: cfg.mint_vsize,
            send_vsize: cfg.send_vsize,
            default_rate: cfg.default_fee_rate_sat_vb,
        },
    };

    // Background maintenance: periodic reconciliation + solvency audit + stale
    // invoice purge, and (if configured) encrypted ledger snapshots. Additive to
    // the opportunistic per-request reconciliation.
    spawn_maintenance(&cfg, state.tapd.clone(), state.registry.clone());

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

/// Spawn the background maintenance tasks. One loop reconciles, audits solvency,
/// and purges stale invoices on an interval; a second (if a backup dir is set)
/// snapshots the ledger. Both are best-effort and never touch the request path.
fn spawn_maintenance(
    cfg: &config::Config,
    tapd: tapd::TapdClient,
    registry: Arc<registry::Registry>,
) {
    if cfg.reconcile_interval_secs > 0 {
        let period = Duration::from_secs(cfg.reconcile_interval_secs.max(5));
        let ttl = cfg.ln_receive_ttl_secs as i64;
        let mut tapd = tapd.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                // interval's first tick fires immediately -> maintenance at boot,
                // so in-flight payments are recovered promptly after a restart.
                tick.tick().await;
                reconcile::reconcile_all(&mut tapd, &registry).await;
                if let Err(e) = reconcile::recover_in_flight(&mut tapd, &registry).await {
                    log::warn!("recover in-flight: {e}");
                }
                if let Err(e) = reconcile::audit_solvency(&mut tapd, &registry).await {
                    log::warn!("solvency audit: {e}");
                }
                if ttl > 0 {
                    let cutoff = auth::now_secs() as i64 - ttl;
                    match registry.purge_stale_ln_receives(cutoff) {
                        Ok(n) if n > 0 => log::info!("purged {n} stale ln invoice(s)"),
                        Ok(_) => {}
                        Err(e) => log::warn!("purge stale ln invoices: {e}"),
                    }
                }
            }
        });
    }

    if let Some(dir) = cfg.backup_dir.clone() {
        let period = Duration::from_secs(cfg.backup_interval_secs.max(60));
        let retention = cfg.backup_retention;
        let key = cfg.backup_key.as_ref().map(|k| k.0);
        if key.is_none() {
            log::warn!(
                "ledger backups enabled WITHOUT encryption (set OZARK_GATEWAY_BACKUP_KEY); \
                 snapshots written in the clear to {}",
                dir.display()
            );
        }
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await; // first tick fires immediately -> snapshot at boot
                let registry = registry.clone();
                let dir = dir.clone();
                let res = tokio::task::spawn_blocking(move || {
                    backup::run_backup(&registry, &dir, retention, key.as_ref())
                })
                .await;
                match res {
                    Ok(Ok(path)) => log::info!("ledger snapshot written: {}", path.display()),
                    Ok(Err(e)) => log::error!("ledger backup failed: {e}"),
                    Err(e) => log::error!("ledger backup task panicked: {e}"),
                }
            }
        });
    }
}

/// Resolve on Ctrl-C / SIGTERM so systemd/docker can stop the gateway cleanly.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    log::info!("shutdown signal received");
}
