//! HTTP routes. Phase 1 is read-only: every endpoint (except `/health`) requires a
//! valid NIP-98 auth header, and `/v1/assets` is scoped to the caller's owned
//! assets via the registry. Mutating endpoints (mint/send/burn) arrive in later
//! phases.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::authenticate;
use crate::error::{GatewayError, GatewayResult};
use crate::state::AppState;
use crate::tapd::{AssetInfo, DecodedAddr, UniverseRoot, UniverseStats};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/assets", get(list_assets))
        .route("/v1/universe/stats", get(universe_stats))
        .route("/v1/universe/roots", get(universe_roots))
        .route("/v1/decode", get(decode))
        .with_state(state)
}

/// Unauthenticated liveness probe.
async fn health() -> &'static str {
    "ok"
}

/// Authenticate a GET request (no body) and return the caller's pubkey.
fn auth_get(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
) -> GatewayResult<String> {
    Ok(authenticate(&state.auth, headers, method, uri, &[])?)
}

/// The caller's assets: tapd's asset list intersected with the ownership
/// registry. Assets not registered to this pubkey are never returned — the core
/// isolation guarantee.
async fn list_assets(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<Vec<AssetInfo>>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;
    let owned: std::collections::HashSet<String> =
        state.registry.assets_of(&pubkey)?.into_iter().collect();
    let mut tapd = state.tapd.clone();
    let all = tapd.list_assets().await.map_err(GatewayError::Upstream)?;
    let mine = all
        .into_iter()
        .filter(|a| owned.contains(&a.asset_id))
        .collect();
    Ok(Json(mine))
}

/// Global universe stats (not owner-scoped — public aggregate data).
async fn universe_stats(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<UniverseStats>> {
    auth_get(&state, &headers, &method, &uri)?;
    let mut tapd = state.tapd.clone();
    let stats = tapd
        .universe_stats()
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(stats))
}

/// Global universe roots (not owner-scoped).
async fn universe_roots(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<Vec<UniverseRoot>>> {
    auth_get(&state, &headers, &method, &uri)?;
    let mut tapd = state.tapd.clone();
    let roots = tapd
        .universe_roots()
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(roots))
}

#[derive(Debug, Deserialize)]
struct DecodeQuery {
    addr: String,
}

/// Decode a Taproot Asset address (read-only helper). The `addr` query param is
/// part of the URL and is therefore covered by the signed NIP-98 `u` tag.
async fn decode(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<DecodeQuery>,
) -> GatewayResult<Json<DecodedAddr>> {
    auth_get(&state, &headers, &method, &uri)?;
    if q.addr.trim().is_empty() {
        return Err(GatewayError::BadRequest("addr is required".into()));
    }
    let mut tapd = state.tapd.clone();
    let decoded = tapd
        .decode_addr(q.addr.trim())
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(decoded))
}
