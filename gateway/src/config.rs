//! Gateway configuration, loaded from environment variables at startup.
//!
//! Everything sensitive (the tapd macaroon, the TLS cert) is referenced by
//! **file path** and read from the local filesystem on the Umbrel node — it never
//! lives in the binary or in the wallet APK.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// TCP address the HTTP server binds to. On Umbrel the system Tor daemon
    /// fronts this port and publishes it as the onion service the wallet dials.
    pub listen_addr: String,

    /// tapd gRPC host, `host:port` (e.g. `127.0.0.1:8443` for a local litd).
    pub tapd_host: String,
    /// Path to tapd's `tls.cert` (PEM). Pinned by the TLS connector.
    pub tapd_cert_path: PathBuf,
    /// Path to the tapd macaroon (raw bytes) authorizing gRPC calls.
    pub tapd_macaroon_path: PathBuf,
    /// Optional path to an **lnd** macaroon (raw bytes) with `invoices:read`.
    /// Needed only for Lightning-asset receive (`/v1/ln/receive`) to detect
    /// invoice settlement via lnd's `LookupInvoice` — the tapd macaroon does not
    /// authorize lnd RPCs. When absent, LN receive still generates invoices but
    /// auto-credit is disabled (settlement can't be observed).
    pub lnd_macaroon_path: Option<PathBuf>,

    /// Path to the SQLite ownership registry (created if absent).
    pub db_path: PathBuf,

    /// Optional canonical base URL (e.g. `http://<onion>.onion`) the wallet signs
    /// in the NIP-98 `u` tag. When set, auth requires the full URL to match; when
    /// unset, only the request path+query is matched (host-agnostic — tolerant of
    /// reverse proxies, still binds the token to one endpoint).
    pub public_base_url: Option<String>,

    /// Max allowed clock skew (seconds) between the NIP-98 event and now.
    pub max_skew_secs: u64,
}

impl Config {
    /// Load from environment. Returns a human-readable error naming the first
    /// missing/invalid variable so a misconfigured deploy fails loudly at boot.
    pub fn from_env() -> Result<Self, String> {
        let listen_addr = env_or("OZARK_GATEWAY_LISTEN", "127.0.0.1:8787");
        let tapd_host = req("OZARK_GATEWAY_TAPD_HOST")?;
        let tapd_cert_path = PathBuf::from(req("OZARK_GATEWAY_TAPD_CERT")?);
        let tapd_macaroon_path = PathBuf::from(req("OZARK_GATEWAY_TAPD_MACAROON")?);
        let lnd_macaroon_path = std::env::var("OZARK_GATEWAY_LND_MACAROON")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        let db_path = PathBuf::from(env_or("OZARK_GATEWAY_DB", "ozark-gateway.sqlite"));
        let public_base_url = std::env::var("OZARK_GATEWAY_PUBLIC_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let max_skew_secs = std::env::var("OZARK_GATEWAY_MAX_SKEW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);

        Ok(Self {
            listen_addr,
            tapd_host,
            tapd_cert_path,
            tapd_macaroon_path,
            lnd_macaroon_path,
            db_path,
            public_base_url,
            max_skew_secs,
        })
    }
}

fn req(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!("required env var {key} is missing or empty")),
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}
