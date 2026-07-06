//! HTTP routes. Every endpoint (except `/health`) requires a valid NIP-98 auth
//! header. Reads are scoped to the caller's balances; mutating actions
//! (mint/receive/send/burn/transfer) check and move the caller's balance in the
//! custodial ledger, so no user can touch another's holdings.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{authenticate, now_secs};
use crate::error::{GatewayError, GatewayResult};
use crate::reconcile::reconcile_all;
use crate::registry::{event_kind, LedgerEvent};
use crate::state::AppState;
use crate::tapd::{
    AssetInfo, AssetMeta, DecodedAddr, DecodedAssetInvoice, NodeInfo, RfqQuotes, UniverseRoot,
    UniverseStats,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/assets", get(list_assets))
        .route("/v1/universe/stats", get(universe_stats))
        .route("/v1/universe/roots", get(universe_roots))
        .route("/v1/asset/meta", get(asset_meta))
        .route("/v1/info", get(node_info))
        .route("/v1/decode", get(decode))
        .route("/v1/balance", get(balance))
        .route("/v1/history", get(history))
        .route("/v1/ln/decode", get(ln_decode))
        .route("/v1/ln/rfq-quotes", get(ln_rfq_quotes))
        .route("/v1/mint", post(mint))
        .route("/v1/mint/status", get(mint_status))
        .route("/v1/receive", post(receive))
        .route("/v1/send", post(send))
        .route("/v1/burn", post(burn))
        .route("/v1/transfer", post(transfer))
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

/// One of the caller's holdings: their ledger balance plus tapd metadata.
#[derive(Debug, Serialize)]
struct HeldAsset {
    asset_id: String,
    name: String,
    /// The **caller's** balance (not the node's total holding).
    amount: u64,
    asset_type: String,
    decimal_display: u32,
}

/// The caller's holdings: their non-zero ledger balances, enriched with asset
/// metadata from tapd. Reconciliation runs first so confirmed mints/receives show
/// up without a separate status poll.
async fn list_assets(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<Vec<HeldAsset>>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;
    let mut tapd = state.tapd.clone();
    reconcile_all(&mut tapd, &state.registry).await;

    let holdings = state.registry.holdings(&pubkey)?;
    if holdings.is_empty() {
        return Ok(Json(vec![]));
    }
    let meta: HashMap<String, AssetInfo> = tapd
        .list_assets()
        .await
        .map_err(GatewayError::Upstream)?
        .into_iter()
        .map(|a| (a.asset_id.clone(), a))
        .collect();
    let out = holdings
        .into_iter()
        .map(|(asset_id, amount)| {
            let m = meta.get(&asset_id);
            HeldAsset {
                name: m.map(|m| m.name.clone()).unwrap_or_default(),
                asset_type: m.map(|m| m.asset_type.clone()).unwrap_or_default(),
                decimal_display: m.map(|m| m.decimal_display).unwrap_or(0),
                asset_id,
                amount,
            }
        })
        .collect();
    Ok(Json(out))
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
struct AssetMetaQuery {
    asset_id: String,
}

/// Public metadata for one asset (name/ticker blob, decimals). Not owner-scoped:
/// asset genesis metadata is public, and it lets the client render human-readable
/// amounts and names for any asset it can see.
async fn asset_meta(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<AssetMetaQuery>,
) -> GatewayResult<Json<AssetMeta>> {
    auth_get(&state, &headers, &method, &uri)?;
    let asset_id = q.asset_id.trim();
    if asset_id.is_empty() {
        return Err(GatewayError::BadRequest("asset_id is required".into()));
    }
    let mut tapd = state.tapd.clone();
    let meta = tapd
        .fetch_asset_meta(asset_id)
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(meta))
}

/// Node version + network (non-sensitive status info).
async fn node_info(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<NodeInfo>> {
    auth_get(&state, &headers, &method, &uri)?;
    let mut tapd = state.tapd.clone();
    let info = tapd.get_info().await.map_err(GatewayError::Upstream)?;
    Ok(Json(info))
}

#[derive(Debug, Deserialize)]
struct DecodeQuery {
    addr: String,
}

/// Decode a Taproot Asset address (read-only helper).
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
struct BalanceQuery {
    asset_id: String,
}

#[derive(Debug, Serialize)]
struct BalanceResponse {
    asset_id: String,
    amount: u64,
}

/// The caller's balance of one asset.
async fn balance(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<BalanceQuery>,
) -> GatewayResult<Json<BalanceResponse>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;
    let asset_id = q.asset_id.trim();
    if asset_id.is_empty() {
        return Err(GatewayError::BadRequest("asset_id is required".into()));
    }
    let amount = state.registry.balance_of(asset_id, &pubkey)?;
    Ok(Json(BalanceResponse {
        asset_id: asset_id.to_string(),
        amount,
    }))
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    limit: Option<u32>,
}

/// The caller's transaction history (mint/receive/send/burn/transfers), newest
/// first. Owner-scoped from the ledger — never the node-global transfer list, so
/// no other user's activity is exposed.
async fn history(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> GatewayResult<Json<Vec<LedgerEvent>>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let events = state.registry.history(&pubkey, limit)?;
    Ok(Json(events))
}

#[derive(Debug, Deserialize)]
struct LnDecodeQuery {
    pay_req: String,
    asset_id: String,
}

/// Decode a Lightning **asset** invoice (read-only): asset units + sat equivalent
/// for a given asset id. Not owner-scoped — decoding leaks nothing about balances.
async fn ln_decode(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<LnDecodeQuery>,
) -> GatewayResult<Json<DecodedAssetInvoice>> {
    auth_get(&state, &headers, &method, &uri)?;
    let pay_req = q.pay_req.trim();
    let asset_id = q.asset_id.trim();
    if pay_req.is_empty() || asset_id.is_empty() {
        return Err(GatewayError::BadRequest(
            "pay_req and asset_id are required".into(),
        ));
    }
    let mut tapd = state.tapd.clone();
    let decoded = tapd
        .decode_asset_invoice(pay_req, asset_id)
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(decoded))
}

/// The node's accepted RFQ quote counts (read-only health signal for whether
/// asset-channel routing is available).
async fn ln_rfq_quotes(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<RfqQuotes>> {
    auth_get(&state, &headers, &method, &uri)?;
    let mut tapd = state.tapd.clone();
    let quotes = tapd
        .list_rfq_quotes()
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(quotes))
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
    grouped: Option<bool>,
    #[serde(default)]
    fee_rate_sat_vb: Option<u32>,
}

#[derive(Debug, Serialize)]
struct MintResponse {
    batch_key: String,
    batch_txid: String,
    status: String,
}

/// Mint a new asset and record the caller as its owner. Minting is async, so the
/// caller is credited the full amount only once the genesis confirms (see
/// reconciliation). The body is bound to the NIP-98 signature.
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
    let amount = if collectible { 1 } else { req.amount };

    let mut tapd = state.tapd.clone();
    let outcome = tapd
        .mint_asset(
            req.name.trim(),
            amount,
            req.meta.as_deref().unwrap_or(""),
            collectible,
            req.grouped.unwrap_or(false),
            req.fee_rate_sat_vb.unwrap_or(0),
        )
        .await
        .map_err(GatewayError::Upstream)?;

    state.registry.add_pending_mint(
        &outcome.batch_key,
        &outcome.batch_txid,
        &pubkey,
        req.name.trim(),
        amount as i64,
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
    status: String,
    batch_txid: String,
    asset_id: Option<String>,
}

/// Report a mint's status, reconciling first so it flips to `minted` (and credits
/// the balance) as soon as the asset confirms. Owner-gated.
async fn mint_status(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<MintStatusQuery>,
) -> GatewayResult<Json<MintStatusResponse>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;

    let mut tapd = state.tapd.clone();
    reconcile_all(&mut tapd, &state.registry).await;

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

    match state.registry.mint_result(&q.batch_key)? {
        Some((asset_id, owner)) => {
            if owner != pubkey {
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

#[derive(Debug, Deserialize)]
struct ReceiveRequest {
    asset_id: String,
    amount: u64,
}

#[derive(Debug, Serialize)]
struct ReceiveResponse {
    /// The Taproot Asset address to receive to; the caller is credited once an
    /// incoming transfer to it confirms.
    addr: String,
}

/// Generate a receive address for the caller. On confirmation of an incoming
/// transfer, reconciliation credits the caller's balance.
async fn receive(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<ReceiveResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: ReceiveRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid receive body: {e}")))?;
    if req.asset_id.trim().is_empty() {
        return Err(GatewayError::BadRequest("asset_id is required".into()));
    }
    if req.amount == 0 {
        return Err(GatewayError::BadRequest("amount must be > 0".into()));
    }

    let mut tapd = state.tapd.clone();
    let addr = tapd
        .new_address(req.asset_id.trim(), req.amount)
        .await
        .map_err(GatewayError::Upstream)?;
    state.registry.add_pending_receive(
        &addr,
        req.asset_id.trim(),
        &pubkey,
        req.amount,
        now_secs() as i64,
    )?;
    Ok(Json(ReceiveResponse { addr }))
}

#[derive(Debug, Deserialize)]
struct SendRequest {
    /// Taproot Asset address to send to (encodes the asset id + amount).
    addr: String,
    #[serde(default)]
    fee_rate_sat_vb: Option<u32>,
}

#[derive(Debug, Serialize)]
struct TxResponse {
    txid: String,
}

/// Send an asset out to a Taproot Asset address. The asset id and amount are read
/// from the decoded address; the caller's balance is **debited first** (reserved)
/// and refunded if tapd rejects the send — so a failed send never loses funds and
/// an insufficient balance is rejected (403) before touching the node.
async fn send(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<TxResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: SendRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid send body: {e}")))?;
    if req.addr.trim().is_empty() {
        return Err(GatewayError::BadRequest("addr is required".into()));
    }

    let mut tapd = state.tapd.clone();
    let decoded = tapd
        .decode_addr(req.addr.trim())
        .await
        .map_err(GatewayError::Upstream)?;

    // Reserve the caller's balance; errors (incl. insufficient) reject before send.
    state
        .registry
        .debit(&decoded.asset_id, &pubkey, decoded.amount)?;

    match tapd
        .send_asset(req.addr.trim(), req.fee_rate_sat_vb.unwrap_or(0))
        .await
    {
        Ok(txid) => {
            // Best-effort history: the balance already moved; a failed record must
            // not fail the send.
            let _ = state.registry.record_event(
                &pubkey,
                &decoded.asset_id,
                event_kind::SEND,
                decoded.amount,
                Some(req.addr.trim()),
                Some(&txid),
                now_secs() as i64,
            );
            Ok(Json(TxResponse { txid }))
        }
        Err(e) => {
            // Refund the reservation so a failed send never loses funds.
            let _ = state
                .registry
                .credit(&decoded.asset_id, &pubkey, decoded.amount);
            Err(GatewayError::Upstream(e))
        }
    }
}

#[derive(Debug, Deserialize)]
struct BurnRequest {
    asset_id: String,
    amount: u64,
}

/// Burn (destroy) some of an asset the caller owns. Debits first, refunds on tapd
/// failure.
async fn burn(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<TxResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: BurnRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid burn body: {e}")))?;
    let asset_id = req.asset_id.trim();
    if asset_id.is_empty() {
        return Err(GatewayError::BadRequest("asset_id is required".into()));
    }
    if req.amount == 0 {
        return Err(GatewayError::BadRequest("amount must be > 0".into()));
    }

    state.registry.debit(asset_id, &pubkey, req.amount)?;

    let mut tapd = state.tapd.clone();
    match tapd.burn_asset(asset_id, req.amount).await {
        Ok(txid) => {
            let _ = state.registry.record_event(
                &pubkey,
                asset_id,
                event_kind::BURN,
                req.amount,
                None,
                Some(&txid),
                now_secs() as i64,
            );
            Ok(Json(TxResponse { txid }))
        }
        Err(e) => {
            let _ = state.registry.credit(asset_id, &pubkey, req.amount);
            Err(GatewayError::Upstream(e))
        }
    }
}

#[derive(Debug, Deserialize)]
struct TransferRequest {
    asset_id: String,
    to_pubkey: String,
    amount: u64,
}

#[derive(Debug, Serialize)]
struct TransferResponse {
    status: String,
}

/// Instant internal transfer between two gateway users: a pure ledger move (debit
/// caller, credit recipient), atomic, no on-chain transaction and no fee.
async fn transfer(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<TransferResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: TransferRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid transfer body: {e}")))?;
    let asset_id = req.asset_id.trim();
    let to = req.to_pubkey.trim();
    if asset_id.is_empty() {
        return Err(GatewayError::BadRequest("asset_id is required".into()));
    }
    if req.amount == 0 {
        return Err(GatewayError::BadRequest("amount must be > 0".into()));
    }
    if to.len() != 64 || !to.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(GatewayError::BadRequest(
            "to_pubkey must be a 64-hex Nostr pubkey".into(),
        ));
    }
    if to == pubkey {
        return Err(GatewayError::BadRequest(
            "cannot transfer to yourself".into(),
        ));
    }

    state.registry.transfer(asset_id, &pubkey, to, req.amount)?;
    Ok(Json(TransferResponse {
        status: "ok".into(),
    }))
}
