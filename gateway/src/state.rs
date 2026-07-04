//! Shared application state handed to every axum handler.

use std::sync::Arc;

use crate::registry::Registry;
use crate::tapd::TapdClient;

/// Auth policy, derived from [`crate::config::Config`].
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Canonical base URL the wallet signs in the NIP-98 `u` tag. `Some` => the
    /// full URL must match; `None` => only the path+query is matched.
    pub public_base_url: Option<String>,
    /// Max allowed clock skew (seconds) for the NIP-98 event timestamp.
    pub max_skew_secs: u64,
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
}
