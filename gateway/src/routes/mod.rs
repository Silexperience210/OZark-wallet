//! HTTP routes. Every endpoint (except `/health`) requires a valid NIP-98 auth
//! header. Reads are scoped to the caller's owned assets; `POST /v1/mint` records
//! a new asset's ownership (async — resolved by reconciliation). Send/burn (which
//! enforce owner == caller) arrive in Phase 3.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{authenticate, now_secs};
use crate::error::{GatewayError, GatewayResult};
use crate::reconcile::reconcile_mints;
use crate::state::AppState;
use crate::tapd::{AssetInfo, DecodedAddr, UniverseRoot, UniverseStats};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/assets", get(list_assets))
        .route("/v1/universe/stats", get(universe_stats))
        .route("/v1/universe/roots", get(universe_roots))
        .route("/v1/decode", get(decode))
        .route("/v1/mint", post(mint))
        .route("/v1/mint/status", get(mint_status))
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

/// Authenticate a request whose body is signed via the NIP-98 `payload` tag.
fn auth_body(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    body: &[u8],
) -> GatewayResult<String> {
    Ok(authenticate(&state.auth, headers, method, uri, body)?)
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
    let mut tapd = state.tapd.clone();
    // Opportunistically resolve any of the caller's pending mints so a freshly
    // confirmed asset shows up here without a separate status poll. Best-effort.
    if let Err(e) = reconcile_mints(&mut tapd, &state.registry).await {
        log::warn!("reconcile during list_assets: {e}");
    }
    let owned: std::collections::HashSet<String> =
        state.registry.assets_of(&pubkey)?.into_iter().collect();
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

#[derive(Debug, Deserialize)]
struct MintRequest {
    name: String,
    #[serde(default)]
    amount: u64,
    #[serde(default)]
    meta: Option<String>,
    #[serde(default)]
    collectible: Option<bool>,
    #[serde(default)]
    fee_rate_sat_vb: Option<u32>,
}

#[derive(Debug, Serialize)]
struct MintResponse {
    batch_key: String,
    batch_txid: String,
    /// Always `pending` right after mint — the asset id appears once it confirms.
    status: String,
}

/// Mint a new asset on the shared node and record the caller as its owner. The
/// request body is bound to the NIP-98 signature (payload tag), so the recorded
/// owner is exactly the signer. Because minting is async, ownership is held as a
/// pending claim keyed by the batch and resolved to the asset id by reconciliation.
///
/// `Bytes` must be the last extractor (it consumes the request body).
async fn mint(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<MintResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: MintRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid mint body: {e}")))?;

    if req.name.trim().is_empty() {
        return Err(GatewayError::BadRequest("name is required".into()));
    }
    let collectible = req.collectible.unwrap_or(false);
    if !collectible && req.amount == 0 {
        return Err(GatewayError::BadRequest(
            "amount must be > 0 for a normal asset".into(),
        ));
    }

    let mut tapd = state.tapd.clone();
    let outcome = tapd
        .mint_asset(
            req.name.trim(),
            req.amount,
            req.meta.as_deref().unwrap_or(""),
            collectible,
            req.fee_rate_sat_vb.unwrap_or(0),
        )
        .await
        .map_err(GatewayError::Upstream)?;

    state.registry.add_pending_mint(
        &outcome.batch_key,
        &outcome.batch_txid,
        &pubkey,
        req.name.trim(),
        req.amount as i64,
        now_secs() as i64,
    )?;

    Ok(Json(MintResponse {
        batch_key: outcome.batch_key,
        batch_txid: outcome.batch_txid,
        status: "pending".into(),
    }))
}

#[derive(Debug, Deserialize)]
struct MintStatusQuery {
    batch_key: String,
}

#[derive(Debug, Serialize)]
struct MintStatusResponse {
    /// `pending` while the mint is unconfirmed, `minted` once resolved.
    status: String,
    batch_txid: String,
    asset_id: Option<String>,
}

/// Report a mint's status, running reconciliation first so it flips to `minted`
/// as soon as the asset confirms. Only the mint's owner may query it.
async fn mint_status(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<MintStatusQuery>,
) -> GatewayResult<Json<MintStatusResponse>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;

    let mut tapd = state.tapd.clone();
    if let Err(e) = reconcile_mints(&mut tapd, &state.registry).await {
        log::warn!("reconcile during mint_status: {e}");
    }

    // Still pending?
    if let Some(p) = state.registry.pending_mint(&q.batch_key)? {
        if p.owner_pubkey != pubkey {
            return Err(GatewayError::Forbidden("not your mint".into()));
        }
        return Ok(Json(MintStatusResponse {
            status: "pending".into(),
            batch_txid: p.batch_txid,
            asset_id: None,
        }));
    }

    // Resolved: the ownership row carries the batch key.
    match state.registry.asset_by_batch_key(&q.batch_key)? {
        Some(asset_id) => {
            if state.registry.owner_of(&asset_id)?.as_deref() != Some(pubkey.as_str()) {
                return Err(GatewayError::Forbidden("not your mint".into()));
            }
            Ok(Json(MintStatusResponse {
                status: "minted".into(),
                batch_txid: String::new(),
                asset_id: Some(asset_id),
            }))
        }
        None => Err(GatewayError::BadRequest("unknown batch_key".into())),
    }
}
