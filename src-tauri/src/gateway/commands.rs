//! Tauri commands for the gateway path.
//!
//! These mirror the direct-tapd Taproot commands but route through the gateway
//! onion with NIP-98 auth, so the wallet never needs the tapd macaroon. They
//! coexist with the direct-tapd commands; the UI can be switched over per user.

use crate::gateway::client::{GatewayClient, GatewayConfig};
use crate::WalletState;
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

#[command]
#[allow(clippy::too_many_arguments)]
pub async fn gateway_mint(
    state: State<'_, WalletState>,
    app_handle: AppHandle,
    name: String,
    amount: u64,
    meta: Option<String>,
    collectible: Option<bool>,
    fee_rate_sat_vb: Option<u32>,
) -> Result<Value, String> {
    let body = json!({
        "name": name,
        "amount": amount,
        "meta": meta,
        "collectible": collectible,
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
