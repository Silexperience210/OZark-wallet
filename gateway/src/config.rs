//! Gateway configuration, loaded from environment variables at startup.
//!
//! Everything sensitive (the tapd macaroon, the TLS cert) is referenced by
//! **file path** and read from the local filesystem on the Umbrel node — it never
//! lives in the binary or in the wallet APK.

use std::path::PathBuf;

/// A 32-byte secret whose `Debug` never reveals its bytes (so a `Config` dump
/// can't leak the backup key into logs).
#[derive(Clone)]
pub struct RedactedKey(pub [u8; 32]);

impl std::fmt::Debug for RedactedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedactedKey(<32 bytes>)")
    }
}

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

    /// Optional operator pubkey (64-hex Nostr). When set, `/v1/admin/*` routes
    /// require a NIP-98 signature by exactly this key; when unset, the admin is
    /// whoever claimed it (see `allow_admin_claim`), else admin routes are 403.
    /// Operator actions: open/list asset channels, connect peers.
    pub admin_pubkey: Option<String>,

    /// When `true`, `POST /v1/admin/claim` lets the first authenticated caller
    /// become the operator (persisted) — as long as no admin exists yet. A
    /// one-tap, no-hex operator setup. Gate it on only during setup, since anyone
    /// who can reach the onion could otherwise claim. Ignored once an admin exists
    /// (env `admin_pubkey` or a prior claim).
    pub allow_admin_claim: bool,

    /// How often the background maintenance loop runs reconciliation + the
    /// solvency audit + pending-invoice purge, in seconds. `0` disables the loop
    /// (reconciliation still runs opportunistically on requests). Default 60.
    pub reconcile_interval_secs: u64,
    /// Pending Lightning-receive invoices older than this (seconds) are purged
    /// even if never observed (e.g. lnd forgot them). Default 86400 (24h).
    pub ln_receive_ttl_secs: u64,

    /// Directory for ledger snapshots (the ledger IS the custody record). `None`
    /// disables backups. Should be a volume separate from the live DB.
    pub backup_dir: Option<PathBuf>,
    /// How often to snapshot the ledger, in seconds. Default 3600 (1h).
    pub backup_interval_secs: u64,
    /// How many snapshots to keep; older ones are pruned. Default 24.
    pub backup_retention: usize,
    /// 32-byte key (hex) to encrypt snapshots with XChaCha20-Poly1305. When unset
    /// but a backup dir is configured, snapshots are written in the clear (warned).
    pub backup_key: Option<RedactedKey>,

    /// Charge users a sats fee (from their custodial sats balance) for on-chain
    /// operations (mint/send). When off (default), operations are free — the
    /// operator eats the on-chain cost. Requires an operator to exist (fees are
    /// credited to them).
    pub charge_fees: bool,
    /// Operator markup on the estimated network fee, in basis points (10000 =
    /// 100%). Default 300 = 3%.
    pub fee_margin_bps: u64,
    /// Minimum fee (sats) per chargeable op, covering fixed overhead. Default 100.
    pub fee_floor_sats: u64,
    /// Assumed vsize (vB) of a mint / send tx, for the network-fee estimate.
    pub mint_vsize: u64,
    pub send_vsize: u64,
    /// Fee rate (sat/vB) assumed when the request doesn't specify one. Default 5.
    pub default_fee_rate_sat_vb: u32,
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
        let admin_pubkey = std::env::var("OZARK_GATEWAY_ADMIN_PUBKEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_lowercase());
        let allow_admin_claim = std::env::var("OZARK_GATEWAY_ALLOW_ADMIN_CLAIM")
            .ok()
            .map(|s| matches!(s.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);

        let reconcile_interval_secs = env_u64("OZARK_GATEWAY_RECONCILE_INTERVAL_SECS", 60);
        let ln_receive_ttl_secs = env_u64("OZARK_GATEWAY_LN_RECEIVE_TTL_SECS", 86_400);
        let backup_dir = std::env::var("OZARK_GATEWAY_BACKUP_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        let backup_interval_secs = env_u64("OZARK_GATEWAY_BACKUP_INTERVAL_SECS", 3_600);
        let backup_retention = env_u64("OZARK_GATEWAY_BACKUP_RETENTION", 24) as usize;
        let charge_fees = std::env::var("OZARK_GATEWAY_CHARGE_FEES")
            .ok()
            .map(|s| matches!(s.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        let fee_margin_bps = env_u64("OZARK_GATEWAY_FEE_MARGIN_BPS", 300);
        let fee_floor_sats = env_u64("OZARK_GATEWAY_FEE_FLOOR_SATS", 100);
        let mint_vsize = env_u64("OZARK_GATEWAY_MINT_VSIZE", 250);
        let send_vsize = env_u64("OZARK_GATEWAY_SEND_VSIZE", 200);
        let default_fee_rate_sat_vb = env_u64("OZARK_GATEWAY_DEFAULT_FEE_RATE", 5) as u32;

        let backup_key = match std::env::var("OZARK_GATEWAY_BACKUP_KEY") {
            Ok(s) if !s.trim().is_empty() => {
                let bytes = hex::decode(s.trim())
                    .map_err(|e| format!("OZARK_GATEWAY_BACKUP_KEY must be hex: {e}"))?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| {
                    "OZARK_GATEWAY_BACKUP_KEY must be 32 bytes (64 hex chars)".to_string()
                })?;
                Some(RedactedKey(arr))
            }
            _ => None,
        };

        Ok(Self {
            listen_addr,
            tapd_host,
            tapd_cert_path,
            tapd_macaroon_path,
            lnd_macaroon_path,
            db_path,
            public_base_url,
            max_skew_secs,
            admin_pubkey,
            allow_admin_claim,
            reconcile_interval_secs,
            ln_receive_ttl_secs,
            backup_dir,
            backup_interval_secs,
            backup_retention,
            backup_key,
            charge_fees,
            fee_margin_bps,
            fee_floor_sats,
            mint_vsize,
            send_vsize,
            default_fee_rate_sat_vb,
        })
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
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
