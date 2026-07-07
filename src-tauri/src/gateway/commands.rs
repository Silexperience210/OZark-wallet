//! Tauri commands for the gateway path.
//!
//! These mirror the direct-tapd Taproot commands but route through the gateway
//! onion with NIP-98 auth, so the wallet never needs the tapd macaroon. They
//! coexist with the direct-tapd commands; the UI can be switched over per user.

use crate::gateway::client::{GatewayClient, GatewayConfig};
use crate::WalletState;
use nostr::prelude::ToBech32;
use serde_json::{json, Value};
use std::path::PathBuf;
use tauri::{command, AppHandle, State};

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(WalletState::data_dir(app)?.join("gateway-config.json"))
}

/// The compile-time default gateway URL (the operator's custodial node), if baked
/// in via `OZARK_DEFAULT_GATEWAY_URL`. `None` when no default is embedded.
fn default_gateway_url() -> Option<String> {
    option_env!("OZARK_DEFAULT_GATEWAY_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn read_config(app: &AppHandle) -> Result<Option<GatewayConfig>, String> {
    let path = config_path(app)?;
    if path.exists() {
        let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let cfg: GatewayConfig = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        if !cfg.base_url.trim().is_empty() {
            return Ok(Some(cfg));
        }
    }
    // No user-saved config -> fall back to the baked default node (custodial method)
    // so the Vault screen connects out of the box. The user can still override it.
    Ok(default_gateway_url().map(|base_url| GatewayConfig { base_url }))
}

/// Save the gateway onion base URL (e.g. `http://<onion>.onion`).
#[command]
pub fn save_gateway_config(app_handle: AppHandle, base_url: String) -> Result<(), String> {
    let path = config_path(&app_handle)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let cfg = GatewayConfig {
        base_url: base_url.trim().to_string(),
    };
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Load the saved gateway config, if any.
#[command]
pub fn load_gateway_config(app_handle: AppHandle) -> Result<Option<GatewayConfig>, String> {
    read_config(&app_handle)
}

/// The wallet's Nostr public key — the identity every gateway request is signed
/// with. Returns `{ hex, npub }`. Set the **hex** as the gateway's
/// `OZARK_GATEWAY_ADMIN_PUBKEY` to authorize this wallet for the operator routes.
#[command]
pub fn gateway_pubkey(state: State<'_, WalletState>) -> Result<Value, String> {
    let guard = state.nostr.lock().map_err(|e| e.to_string())?;
    let keys = guard.as_ref().ok_or("wallet is locked")?;
    let pk = keys.public_key();
    Ok(json!({
        "hex": pk.to_hex(),
        "npub": pk.to_bech32().map_err(|e| e.to_string())?,
    }))
}

/// Build a gateway client from the saved URL, the wallet's Nostr keys, and Tor.
async fn client(state: &State<'_, WalletState>, app: &AppHandle) -> Result<GatewayClient, String> {
    let cfg = read_config(app)?.ok_or("gateway URL is not configured")?;
    if cfg.base_url.trim().is_empty() {
        return Err("gateway URL is not configured".into());
    }
    let keys = {
        let guard = state.nostr.lock().map_err(|e| e.to_string())?;
        guard.as_ref().ok_or("wallet is locked")?.clone()
    };
    let tor = state.tor.lock().await.clone();
    Ok(GatewayClient::new(cfg.base_url, keys, tor))
}

#[command]
pub async fn gateway_list_assets(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
) -> Result<Value, String> {
    client(&state, &app_handle).await?.get("/v1/assets").await
}

#[command]
pub async fn gateway_balance(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
) -> Result<Value, String> {
    let path = format!("/v1/balance?asset_id={}", asset_id.trim());
    client(&state, &app_handle).await?.get(&path).await
}

/// The caller's transaction history (owner-scoped ledger events), newest first.
#[command]
pub async fn gateway_history(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    limit: Option<u32>,
) -> Result<Value, String> {
    let path = format!("/v1/history?limit={}", limit.unwrap_or(50));
    client(&state, &app_handle).await?.get(&path).await
}

/// Public metadata (name/ticker blob, decimals) for one asset.
#[command]
pub async fn gateway_asset_meta(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
) -> Result<Value, String> {
    let path = format!("/v1/asset/meta?asset_id={}", asset_id.trim());
    client(&state, &app_handle).await?.get(&path).await
}

/// Gateway node info (tapd version + network) — for a status panel.
#[command]
pub async fn gateway_info(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
) -> Result<Value, String> {
    client(&state, &app_handle).await?.get("/v1/info").await
}

/// Decode a Lightning asset invoice (read-only): asset units + sat equivalent.
#[command]
pub async fn gateway_ln_decode(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    pay_req: String,
    asset_id: String,
) -> Result<Value, String> {
    let path = format!(
        "/v1/ln/decode?pay_req={}&asset_id={}",
        pay_req.trim(),
        asset_id.trim()
    );
    client(&state, &app_handle).await?.get(&path).await
}

/// The node's accepted RFQ quote counts (Lightning-asset routing health signal).
#[command]
pub async fn gateway_ln_rfq_quotes(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
) -> Result<Value, String> {
    client(&state, &app_handle)
        .await?
        .get("/v1/ln/rfq-quotes")
        .await
}

/// Pay a Lightning asset invoice: debits the caller's asset balance (reserved,
/// refunded on failure) and settles over an asset channel.
#[command]
pub async fn gateway_ln_pay(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    pay_req: String,
    asset_id: String,
    peer_pubkey: Option<String>,
) -> Result<Value, String> {
    let body = json!({
        "pay_req": pay_req,
        "asset_id": asset_id,
        "peer_pubkey": peer_pubkey,
    });
    client(&state, &app_handle)
        .await?
        .post("/v1/ln/pay", body)
        .await
}

/// Create a Lightning asset invoice to receive `asset_amount` units of `asset_id`.
/// Returns the BOLT11 `payment_request` (+ `r_hash`); the caller is credited once
/// the invoice settles (auto-credit needs the gateway's lnd macaroon).
#[command]
pub async fn gateway_ln_receive(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
    asset_amount: u64,
    peer_pubkey: Option<String>,
    memo: Option<String>,
) -> Result<Value, String> {
    let body = json!({
        "asset_id": asset_id,
        "asset_amount": asset_amount,
        "peer_pubkey": peer_pubkey,
        "memo": memo,
    });
    client(&state, &app_handle)
        .await?
        .post("/v1/ln/receive", body)
        .await
}

#[command]
#[allow(clippy::too_many_arguments)]
pub async fn gateway_mint(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    name: String,
    amount: u64,
    meta: Option<String>,
    collectible: Option<bool>,
    grouped: Option<bool>,
    fee_rate_sat_vb: Option<u32>,
) -> Result<Value, String> {
    let body = json!({
        "name": name,
        "amount": amount,
        "meta": meta,
        "collectible": collectible,
        "grouped": grouped,
        "fee_rate_sat_vb": fee_rate_sat_vb,
    });
    client(&state, &app_handle)
        .await?
        .post("/v1/mint", body)
        .await
}

#[command]
pub async fn gateway_mint_status(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    batch_key: String,
) -> Result<Value, String> {
    let path = format!("/v1/mint/status?batch_key={}", batch_key.trim());
    client(&state, &app_handle).await?.get(&path).await
}

#[command]
pub async fn gateway_receive(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
    amount: u64,
) -> Result<Value, String> {
    let body = json!({ "asset_id": asset_id, "amount": amount });
    client(&state, &app_handle)
        .await?
        .post("/v1/receive", body)
        .await
}

#[command]
pub async fn gateway_send(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    addr: String,
    fee_rate_sat_vb: Option<u32>,
) -> Result<Value, String> {
    let body = json!({ "addr": addr, "fee_rate_sat_vb": fee_rate_sat_vb });
    client(&state, &app_handle)
        .await?
        .post("/v1/send", body)
        .await
}

#[command]
pub async fn gateway_burn(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
    amount: u64,
) -> Result<Value, String> {
    let body = json!({ "asset_id": asset_id, "amount": amount });
    client(&state, &app_handle)
        .await?
        .post("/v1/burn", body)
        .await
}

#[command]
pub async fn gateway_transfer(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
    to_pubkey: String,
    amount: u64,
) -> Result<Value, String> {
    let body = json!({ "asset_id": asset_id, "to_pubkey": to_pubkey, "amount": amount });
    client(&state, &app_handle)
        .await?
        .post("/v1/transfer", body)
        .await
}

// ---- Custodial sats balance (funds on-chain operation fees) ----

/// The caller's custodial sats balance, in sats. Used to cover the operator fee on
/// chargeable on-chain ops (mint/send) when the node charges fees.
#[command]
pub async fn gateway_sats_balance(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
) -> Result<Value, String> {
    client(&state, &app_handle)
        .await?
        .get("/v1/sats/balance")
        .await
}

/// Create a Lightning invoice to top up the caller's sats balance by `amount_sats`.
/// Returns `{ payment_request, r_hash }`; the balance is credited once the invoice
/// settles (reconciled by the gateway via LookupInvoice).
#[command]
pub async fn gateway_sats_deposit(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    amount_sats: u64,
) -> Result<Value, String> {
    let body = json!({ "amount_sats": amount_sats });
    client(&state, &app_handle)
        .await?
        .post("/v1/sats/deposit", body)
        .await
}

/// Quote the sats fee for a chargeable op (`"mint"` or `"send"`) before performing
/// it. Returns `{ network_sats, margin_sats, total_sats }`. Charged nothing when the
/// node runs with fees off (the quote is still an estimate for display).
#[command]
pub async fn gateway_fee_quote(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    op: String,
    fee_rate_sat_vb: Option<u32>,
) -> Result<Value, String> {
    let mut path = format!("/v1/fee/quote?op={}", op.trim());
    if let Some(rate) = fee_rate_sat_vb {
        path.push_str(&format!("&fee_rate_sat_vb={rate}"));
    }
    client(&state, &app_handle).await?.get(&path).await
}

// ---- Operator (admin) — requires this wallet to be the node's operator (either
// the gateway's OZARK_GATEWAY_ADMIN_PUBKEY, or claimed via gateway_admin_claim).

/// Operator claim (trust-on-first-use): make this wallet the node's operator. Only
/// works when the gateway has `OZARK_GATEWAY_ALLOW_ADMIN_CLAIM` on and no operator
/// exists yet — a one-tap setup with no pubkey to copy.
#[command]
pub async fn gateway_admin_claim(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
) -> Result<Value, String> {
    client(&state, &app_handle)
        .await?
        .post("/v1/admin/claim", json!({}))
        .await
}

/// Operator: list the node's channels (asset channels included).
#[command]
pub async fn gateway_admin_channels(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
) -> Result<Value, String> {
    client(&state, &app_handle)
        .await?
        .get("/v1/admin/channels")
        .await
}

/// Operator: open an asset channel funded from the node's assets to a connected peer.
#[command]
pub async fn gateway_admin_channel_open(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    asset_id: String,
    asset_amount: u64,
    peer_pubkey: String,
    fee_rate_sat_vb: Option<u32>,
) -> Result<Value, String> {
    let body = json!({
        "asset_id": asset_id,
        "asset_amount": asset_amount,
        "peer_pubkey": peer_pubkey,
        "fee_rate_sat_vb": fee_rate_sat_vb,
    });
    client(&state, &app_handle)
        .await?
        .post("/v1/admin/channel/open", body)
        .await
}

/// Operator: connect to a Lightning peer (prerequisite for opening a channel).
#[command]
pub async fn gateway_admin_peer_connect(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    pubkey: String,
    host: String,
) -> Result<Value, String> {
    let body = json!({ "pubkey": pubkey, "host": host });
    client(&state, &app_handle)
        .await?
        .post("/v1/admin/peer/connect", body)
        .await
}
