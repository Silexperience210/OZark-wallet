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
use crate::fees::FeeQuote;
use crate::reconcile::reconcile_all;
use crate::registry::{event_kind, LedgerEvent};
use crate::state::AppState;
use crate::tapd::{
    AssetInfo, AssetMeta, ChannelInfo, DecodedAddr, DecodedAssetInvoice, NodeInfo, RfqQuotes,
    UniverseRoot, UniverseStats,
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
        .route("/v1/sats/balance", get(sats_balance))
        .route("/v1/sats/deposit", post(sats_deposit))
        .route("/v1/fee/quote", get(fee_quote))
        .route("/v1/ln/decode", get(ln_decode))
        .route("/v1/ln/rfq-quotes", get(ln_rfq_quotes))
        .route("/v1/ln/pay", post(ln_pay))
        .route("/v1/ln/receive", post(ln_receive))
        .route("/v1/mint", post(mint))
        .route("/v1/mint/status", get(mint_status))
        .route("/v1/receive", post(receive))
        .route("/v1/send", post(send))
        .route("/v1/burn", post(burn))
        .route("/v1/transfer", post(transfer))
        .route("/v1/admin/claim", post(admin_claim))
        .route("/v1/admin/channels", get(admin_channels))
        .route("/v1/admin/channel/open", post(admin_channel_open))
        .route("/v1/admin/peer/connect", post(admin_peer_connect))
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
    Ok(authenticate(
        &state.auth,
        &state.security,
        headers,
        method,
        uri,
        &[],
    )?)
}

/// Authenticate a request whose body is signed via the NIP-98 `payload` tag.
fn auth_body(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    body: &[u8],
) -> GatewayResult<String> {
    Ok(authenticate(
        &state.auth,
        &state.security,
        headers,
        method,
        uri,
        body,
    )?)
}

/// The current operator pubkey: the env-configured one wins; else a prior claim
/// persisted in the registry. `None` => no operator yet (admin routes are 403).
fn effective_admin(state: &AppState) -> GatewayResult<Option<String>> {
    if let Some(a) = &state.auth.admin_pubkey {
        return Ok(Some(a.clone()));
    }
    Ok(state.registry.get_admin_pubkey()?.map(|s| s.to_lowercase()))
}

/// Authenticate an **operator** request: a valid NIP-98 signed by exactly the
/// current operator pubkey. 403 when there is no operator or the caller isn't it.
fn auth_admin(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    body: &[u8],
) -> GatewayResult<()> {
    let caller =
        authenticate(&state.auth, &state.security, headers, method, uri, body)?.to_lowercase();
    match effective_admin(state)? {
        Some(admin) if admin == caller => Ok(()),
        _ => Err(GatewayError::Forbidden("operator only".into())),
    }
}

/// Charge the operator fee (sats) for a chargeable on-chain op, when enabled.
/// Returns `Some((operator, amount))` if a fee was taken (so the caller can refund
/// on failure), or `None` when fees are off, no operator exists, or the payer is
/// the operator. A `Forbidden` (insufficient sats) propagates to the caller.
fn maybe_charge_fee(
    state: &AppState,
    payer: &str,
    op: &str,
    fee_rate_sat_vb: u32,
) -> GatewayResult<Option<(String, u64)>> {
    if !state.fees.charge {
        return Ok(None);
    }
    let Some(operator) = effective_admin(state)? else {
        return Ok(None);
    };
    if operator == payer {
        return Ok(None);
    }
    let total = state.fees.quote(op, fee_rate_sat_vb).total_sats;
    state.registry.charge_fee(payer, &operator, total)?;
    Ok(Some((operator, total)))
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

#[derive(Debug, Serialize)]
struct SatsBalanceResponse {
    amount: u64,
}

/// The caller's custodial sats balance (funds on-chain operation fees).
async fn sats_balance(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<SatsBalanceResponse>> {
    let pubkey = auth_get(&state, &headers, &method, &uri)?;
    let amount = state.registry.sats_balance_of(&pubkey)?;
    Ok(Json(SatsBalanceResponse { amount }))
}

#[derive(Debug, Deserialize)]
struct SatsDepositRequest {
    amount_sats: u64,
}

#[derive(Debug, Serialize)]
struct SatsDepositResponse {
    payment_request: String,
    r_hash: String,
}

/// Create a Lightning invoice to top up the caller's sats balance; credited on
/// settlement (reconciliation via LookupInvoice).
async fn sats_deposit(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<SatsDepositResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: SatsDepositRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid deposit body: {e}")))?;
    if req.amount_sats == 0 {
        return Err(GatewayError::BadRequest("amount_sats must be > 0".into()));
    }
    let mut tapd = state.tapd.clone();
    let created = tapd
        .create_sats_invoice(req.amount_sats, "OZark sats deposit")
        .await
        .map_err(GatewayError::Upstream)?;
    state.registry.add_pending_sats_deposit(
        &created.r_hash,
        &pubkey,
        req.amount_sats,
        now_secs() as i64,
    )?;
    Ok(Json(SatsDepositResponse {
        payment_request: created.payment_request,
        r_hash: created.r_hash,
    }))
}

#[derive(Debug, Deserialize)]
struct FeeQuoteQuery {
    op: String,
    #[serde(default)]
    fee_rate_sat_vb: Option<u32>,
}

/// Quote the sats fee for a chargeable op (`mint`/`send`) before performing it.
async fn fee_quote(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<FeeQuoteQuery>,
) -> GatewayResult<Json<FeeQuote>> {
    auth_get(&state, &headers, &method, &uri)?;
    Ok(Json(
        state
            .fees
            .quote(q.op.trim(), q.fee_rate_sat_vb.unwrap_or(0)),
    ))
}

#[derive(Debug, Deserialize)]
struct LnDecodeQuery {
    pay_req: String,
    #[serde(default)]
    asset_id: String,
    /// Price the invoice against a fungible **group** instead of one asset id.
    /// Mutually exclusive with `asset_id`; the resolved tranche is returned.
    #[serde(default)]
    group_key: String,
}

/// Decode a Lightning **asset** invoice (read-only): asset units + sat equivalent
/// for a given asset id (or fungible group key). Not owner-scoped — decoding leaks
/// nothing about balances.
async fn ln_decode(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<LnDecodeQuery>,
) -> GatewayResult<Json<DecodedAssetInvoice>> {
    auth_get(&state, &headers, &method, &uri)?;
    let pay_req = q.pay_req.trim();
    if pay_req.is_empty() || (q.asset_id.trim().is_empty() && q.group_key.trim().is_empty()) {
        return Err(GatewayError::BadRequest(
            "pay_req and one of asset_id / group_key are required".into(),
        ));
    }
    let mut tapd = state.tapd.clone();
    let decoded = tapd
        .decode_asset_invoice(pay_req, q.asset_id.trim(), q.group_key.trim())
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
struct LnPayRequest {
    pay_req: String,
    asset_id: String,
    #[serde(default)]
    peer_pubkey: Option<String>,
}

#[derive(Debug, Serialize)]
struct LnPayResponse {
    status: String,
    asset_amount: u64,
}

/// Pay a Lightning asset invoice, spending the caller's `asset_id` balance. The
/// amount is read from the decoded invoice; the caller is **debited first**
/// (reserved) and refunded unless the payment succeeds — mirroring the on-chain
/// send. Only a "Succeeded" result keeps the debit.
async fn ln_pay(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<LnPayResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: LnPayRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid ln pay body: {e}")))?;
    let pay_req = req.pay_req.trim();
    let asset_id = req.asset_id.trim();
    if pay_req.is_empty() || asset_id.is_empty() {
        return Err(GatewayError::BadRequest(
            "pay_req and asset_id are required".into(),
        ));
    }

    let mut tapd = state.tapd.clone();
    // Decode to learn how many asset units the invoice consumes. Paying keys the
    // ledger debit by a concrete asset_id, so group-key pay is not offered here
    // (it needs group-fungible ledger accounting — a separate change).
    let decoded = tapd
        .decode_asset_invoice(pay_req, asset_id, "")
        .await
        .map_err(GatewayError::Upstream)?;
    let amount = decoded.asset_amount;
    if amount == 0 {
        return Err(GatewayError::BadRequest(
            "invoice settles zero asset units".into(),
        ));
    }

    // Reserve the caller's balance AND record durable in-flight intent atomically
    // (403 if insufficient) — a crash mid-payment leaves a recoverable row instead
    // of a silent debit. The payment hash lets recovery track the outcome.
    let counterparty = (!decoded.destination.is_empty()).then_some(decoded.destination.as_str());
    let reference = (!decoded.payment_hash.is_empty()).then_some(decoded.payment_hash.as_str());
    let id = state.registry.debit_and_mark_in_flight(
        event_kind::LN_SEND,
        asset_id,
        &pubkey,
        amount,
        reference,
        counterparty,
        now_secs() as i64,
    )?;

    let peer = req.peer_pubkey.as_deref().unwrap_or("");
    match tapd.pay_asset_invoice(pay_req, asset_id, peer).await {
        Ok(status) if status == "Succeeded" => {
            let _ = state.registry.settle_in_flight(id, None);
            Ok(Json(LnPayResponse {
                status,
                asset_amount: amount,
            }))
        }
        Ok(status) => {
            // Not settled — refund the reservation.
            let _ = state.registry.refund_in_flight(id);
            Err(GatewayError::Upstream(format!(
                "payment not completed: {status}"
            )))
        }
        Err(e) => {
            let _ = state.registry.refund_in_flight(id);
            Err(GatewayError::Upstream(e))
        }
    }
}

#[derive(Debug, Deserialize)]
struct LnReceiveRequest {
    asset_id: String,
    asset_amount: u64,
    #[serde(default)]
    peer_pubkey: Option<String>,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Debug, Serialize)]
struct LnReceiveResponse {
    /// BOLT11 payment request to hand to the payer.
    payment_request: String,
    /// Hex payment hash; reconciliation credits the caller once it settles.
    r_hash: String,
    /// The accepted RFQ quote (negotiated asset⇄sat rate + expiry), when priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    quote: Option<crate::tapd::RfqQuote>,
}

/// Create a Lightning **asset** invoice for the caller. On settlement,
/// reconciliation (via lnd `LookupInvoice`) credits the caller's ledger balance —
/// mirroring on-chain receive. Requires an open asset channel + a quoting peer;
/// auto-credit additionally needs the gateway's lnd macaroon (see deploy docs).
async fn ln_receive(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<LnReceiveResponse>> {
    let pubkey = auth_body(&state, &headers, &method, &uri, &body)?;
    let req: LnReceiveRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid ln receive body: {e}")))?;
    let asset_id = req.asset_id.trim();
    if asset_id.is_empty() {
        return Err(GatewayError::BadRequest("asset_id is required".into()));
    }
    if req.asset_amount == 0 {
        return Err(GatewayError::BadRequest("asset_amount must be > 0".into()));
    }

    let mut tapd = state.tapd.clone();
    // Receive credits the ledger under a concrete asset_id, so it is asset-id
    // scoped (group-key receive needs group-fungible crediting — a separate
    // change). The accepted RFQ quote is surfaced for the user.
    let created = tapd
        .create_asset_invoice(
            asset_id,
            "",
            req.asset_amount,
            req.peer_pubkey.as_deref().unwrap_or(""),
            req.memo.as_deref().unwrap_or(""),
        )
        .await
        .map_err(GatewayError::Upstream)?;
    state.registry.add_pending_ln_receive(
        &created.r_hash,
        asset_id,
        &pubkey,
        req.asset_amount,
        now_secs() as i64,
    )?;
    Ok(Json(LnReceiveResponse {
        payment_request: created.payment_request,
        r_hash: created.r_hash,
        quote: created.quote,
    }))
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

    // Charge the operator fee (sats) up front; refund if the mint call fails.
    let fee_rate = req.fee_rate_sat_vb.unwrap_or(0);
    let charged = maybe_charge_fee(&state, &pubkey, "mint", fee_rate)?;

    let mut tapd = state.tapd.clone();
    let outcome = match tapd
        .mint_asset(
            req.name.trim(),
            amount,
            req.meta.as_deref().unwrap_or(""),
            collectible,
            req.grouped.unwrap_or(false),
            fee_rate,
        )
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Some((operator, sats)) = &charged {
                let _ = state.registry.refund_fee(&pubkey, operator, *sats);
            }
            return Err(GatewayError::Upstream(e));
        }
    };

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

    // Reserve balance + durable in-flight intent atomically (403 if insufficient),
    // so a crash mid-send leaves a recoverable row instead of a silent debit.
    let id = state.registry.debit_and_mark_in_flight(
        event_kind::SEND,
        &decoded.asset_id,
        &pubkey,
        decoded.amount,
        None,
        Some(req.addr.trim()),
        now_secs() as i64,
    )?;

    // Charge the operator fee (sats) after reserving the asset; if the fee fails
    // (insufficient sats), refund the asset reservation and reject.
    let fee_rate = req.fee_rate_sat_vb.unwrap_or(0);
    let charged = match maybe_charge_fee(&state, &pubkey, "send", fee_rate) {
        Ok(c) => c,
        Err(e) => {
            let _ = state.registry.refund_in_flight(id);
            return Err(e);
        }
    };

    match tapd.send_asset(req.addr.trim(), fee_rate).await {
        Ok(txid) => {
            // Settle records the SEND event, stamping the anchor txid as reference.
            let _ = state.registry.settle_in_flight(id, Some(txid.as_str()));
            Ok(Json(TxResponse { txid }))
        }
        Err(e) => {
            // Refund the reservation + the sats fee so a failed send loses nothing.
            let _ = state.registry.refund_in_flight(id);
            if let Some((operator, sats)) = &charged {
                let _ = state.registry.refund_fee(&pubkey, operator, *sats);
            }
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

// ---- Operator (admin) routes ------------------------------------------------
// Open/list asset channels + connect peers. These spend the node's OWN
// liquidity, so they are gated to the configured operator pubkey — never exposed
// to custodial users. This is what makes LN-asset routing possible in the first
// place (pay/receive only route once an asset channel with a quoting peer exists).

#[derive(Debug, Serialize)]
struct ClaimResponse {
    status: String,
    pubkey: String,
}

/// Trust-on-first-use operator claim: when `allow_admin_claim` is on and no
/// operator exists yet, the authenticated caller becomes the persisted operator.
/// One tap, no hex to copy. Idempotent for the same caller; 403 once claimed or if
/// claiming is disabled. Turn `OZARK_GATEWAY_ALLOW_ADMIN_CLAIM` off after setup.
async fn admin_claim(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<ClaimResponse>> {
    let caller = auth_body(&state, &headers, &method, &uri, &body)?.to_lowercase();
    if !state.auth.allow_admin_claim {
        return Err(GatewayError::Forbidden("admin claim is disabled".into()));
    }
    if let Some(existing) = effective_admin(&state)? {
        if existing == caller {
            return Ok(Json(ClaimResponse {
                status: "ok".into(),
                pubkey: caller,
            }));
        }
        return Err(GatewayError::Forbidden("operator already claimed".into()));
    }
    state.registry.set_admin_pubkey(&caller)?;
    log::info!("operator claimed by {caller}");
    Ok(Json(ClaimResponse {
        status: "ok".into(),
        pubkey: caller,
    }))
}

/// The node's channels (operator view). Owner-gated to the admin pubkey.
async fn admin_channels(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> GatewayResult<Json<Vec<ChannelInfo>>> {
    auth_admin(&state, &headers, &method, &uri, &[])?;
    let mut tapd = state.tapd.clone();
    let channels = tapd.list_channels().await.map_err(GatewayError::Upstream)?;
    Ok(Json(channels))
}

#[derive(Debug, Deserialize)]
struct ChannelOpenRequest {
    asset_id: String,
    asset_amount: u64,
    peer_pubkey: String,
    #[serde(default)]
    fee_rate_sat_vb: Option<u32>,
}

/// Open an asset channel to a (connected) peer, funded from the node's assets.
async fn admin_channel_open(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<TxResponse>> {
    auth_admin(&state, &headers, &method, &uri, &body)?;
    let req: ChannelOpenRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid channel open body: {e}")))?;
    let asset_id = req.asset_id.trim();
    let peer = req.peer_pubkey.trim();
    if asset_id.is_empty() || peer.is_empty() {
        return Err(GatewayError::BadRequest(
            "asset_id and peer_pubkey are required".into(),
        ));
    }
    if req.asset_amount == 0 {
        return Err(GatewayError::BadRequest("asset_amount must be > 0".into()));
    }
    let mut tapd = state.tapd.clone();
    let txid = tapd
        .fund_asset_channel(
            asset_id,
            req.asset_amount,
            peer,
            req.fee_rate_sat_vb.unwrap_or(0),
        )
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(TxResponse { txid }))
}

#[derive(Debug, Deserialize)]
struct PeerConnectRequest {
    pubkey: String,
    host: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: String,
}

/// Connect to a Lightning peer (prerequisite for opening a channel).
async fn admin_peer_connect(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> GatewayResult<Json<StatusResponse>> {
    auth_admin(&state, &headers, &method, &uri, &body)?;
    let req: PeerConnectRequest = serde_json::from_slice(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid peer connect body: {e}")))?;
    let pubkey = req.pubkey.trim();
    let host = req.host.trim();
    if pubkey.is_empty() || host.is_empty() {
        return Err(GatewayError::BadRequest(
            "pubkey and host are required".into(),
        ));
    }
    let mut tapd = state.tapd.clone();
    tapd.connect_peer(pubkey, host)
        .await
        .map_err(GatewayError::Upstream)?;
    Ok(Json(StatusResponse {
        status: "ok".into(),
    }))
}
