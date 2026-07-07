//! Shared application state handed to every axum handler.

use std::sync::Arc;

use crate::fees::FeePolicy;
use crate::registry::Registry;
use crate::security::Security;
use crate::tapd::TapdClient;

/// Auth policy, derived from [`crate::config::Config`].
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Canonical base URL the wallet signs in the NIP-98 `u` tag. `Some` => the
    /// full URL must match; `None` => only the path+query is matched.
    pub public_base_url: Option<String>,
    /// Max allowed clock skew (seconds) for the NIP-98 event timestamp.
    pub max_skew_secs: u64,
    /// Operator pubkey allowed to call `/v1/admin/*`. `None` => the admin is a
    /// prior claim (see registry) or, failing that, admin routes are 403.
    pub admin_pubkey: Option<String>,
    /// When true, `POST /v1/admin/claim` lets the first caller become operator.
    pub allow_admin_claim: bool,
}

#[derive(Clone)]
pub struct AppState {
    /// tapd gRPC client (holds the macaroon). `TapdClient` clones are cheap
    /// channel handles; handlers clone one out and call `&mut` on it.
    pub tapd: TapdClient,
    /// Ownership registry (`asset_id → owner_pubkey`).
    pub registry: Arc<Registry>,
    /// Auth policy.
    pub auth: AuthConfig,
    /// Fee policy for chargeable on-chain operations.
    pub fees: FeePolicy,
    /// Per-pubkey rate limiter + NIP-98 replay guard (shared across handlers).
    pub security: Arc<Security>,
}
