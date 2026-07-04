//! Tauri commands exposing the bonding-curve marketplace to the frontend.
//!
//! The desk lives in [`WalletState::desk`]; mutating commands lock it, apply the
//! change, then persist a fresh snapshot. Timestamps are taken from the system
//! clock here so the pure engine in [`super::desk`] stays clock-free.
//!
//! Scope note: these commands drive the **accounting/pricing engine** and its
//! custodial ledger. Real fund movement (mint the Taproot asset on create, pay
//! the Lightning invoice on buy, `send_asset` on withdraw) is the tapd wiring
//! that lands in the next step — `token_id` here is an already-minted asset id.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{command, AppHandle, State};

use super::desk::{
    BuyPreview, Market, MarketSpec, MarketStatus, SellPreview, Side, Trade, Visibility,
};
use super::store;
use crate::WalletState;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lightweight projection of a market for list/detail views — omits the full
/// ledger and trade log the panel doesn't need.
#[derive(Serialize)]
pub struct MarketView {
    pub token_id: String,
    pub ticker: String,
    pub name: String,
    pub creator: String,
    pub visibility: Visibility,
    pub status: MarketStatus,
    pub supply: u64,
    pub reserve_sats: u64,
    pub withdrawn: u64,
    pub spot_price_msat: u64,
    pub progress_bp: u16,
    pub creator_fee_bp: u16,
    pub creator_fees_sats: u64,
    pub trade_count: usize,
    pub created_at: u64,
}

impl MarketView {
    fn of(m: &Market) -> Self {
        Self {
            token_id: m.token_id.clone(),
            ticker: m.ticker.clone(),
            name: m.name.clone(),
            creator: m.creator.clone(),
            visibility: m.visibility,
            status: m.status,
            supply: m.supply,
            reserve_sats: m.reserve_sats,
            withdrawn: m.withdrawn,
            spot_price_msat: m.spot_price_msat().unwrap_or(0),
            progress_bp: m.progress_bp(),
            creator_fee_bp: m.creator_fee_bp,
            creator_fees_sats: m.creator_fees_sats,
            trade_count: m.trades.len(),
            created_at: m.created_at,
        }
    }
}

/// One point on the price chart, derived from a trade.
#[derive(Serialize)]
pub struct PricePoint {
    pub ts: u64,
    pub price_msat: u64,
    pub side: Side,
    pub tokens: u64,
    pub supply_after: u64,
}

/// Register (list) a new market for an already-minted asset. `spec.seed_sats`
/// controls the optional creator seed (0 = fair launch).
#[command]
pub fn market_create(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    spec: MarketSpec,
) -> Result<(), String> {
    let dir = WalletState::data_dir(&app_handle)?;
    let mut desk = state.desk.lock().map_err(|e| e.to_string())?;
    desk.create_market(spec, now_secs())
        .map_err(|e| e.to_string())?;
    store::save(&dir, &desk)
}

/// All publicly listed markets (the marketplace feed).
#[command]
pub fn market_list(state: State<'_, WalletState>) -> Result<Vec<MarketView>, String> {
    let desk = state.desk.lock().map_err(|e| e.to_string())?;
    Ok(desk
        .public_markets()
        .into_iter()
        .map(MarketView::of)
        .collect())
}

/// A single market by token id.
#[command]
pub fn market_get(state: State<'_, WalletState>, token_id: String) -> Result<MarketView, String> {
    let desk = state.desk.lock().map_err(|e| e.to_string())?;
    let m = desk.market(&token_id).map_err(|e| e.to_string())?;
    Ok(MarketView::of(m))
}

/// The trade log projected to price points for charting.
#[command]
pub fn market_price_history(
    state: State<'_, WalletState>,
    token_id: String,
) -> Result<Vec<PricePoint>, String> {
    let desk = state.desk.lock().map_err(|e| e.to_string())?;
    let m = desk.market(&token_id).map_err(|e| e.to_string())?;
    Ok(m.trades
        .iter()
        .map(|t| PricePoint {
            ts: t.ts,
            price_msat: t.price_msat,
            side: t.side,
            tokens: t.tokens,
            supply_after: t.supply_after,
        })
        .collect())
}

/// Preview how many tokens `budget_sats` buys (fee included), without executing.
#[command]
pub fn market_quote_buy(
    state: State<'_, WalletState>,
    token_id: String,
    budget_sats: u64,
) -> Result<BuyPreview, String> {
    let desk = state.desk.lock().map_err(|e| e.to_string())?;
    desk.market(&token_id)
        .map_err(|e| e.to_string())?
        .preview_buy(budget_sats)
        .map_err(|e| e.to_string())
}

/// Preview the payout for selling `amount` tokens, without executing.
#[command]
pub fn market_quote_sell(
    state: State<'_, WalletState>,
    token_id: String,
    user: String,
    amount: u64,
) -> Result<SellPreview, String> {
    let desk = state.desk.lock().map_err(|e| e.to_string())?;
    desk.market(&token_id)
        .map_err(|e| e.to_string())?
        .preview_sell(&user, amount)
        .map_err(|e| e.to_string())
}

/// Execute a buy on the curve for `user`.
#[command]
pub fn market_buy(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    token_id: String,
    user: String,
    budget_sats: u64,
) -> Result<Trade, String> {
    let dir = WalletState::data_dir(&app_handle)?;
    let mut desk = state.desk.lock().map_err(|e| e.to_string())?;
    let trade = desk
        .buy(&token_id, &user, budget_sats, now_secs())
        .map_err(|e| e.to_string())?;
    store::save(&dir, &desk)?;
    Ok(trade)
}

/// Execute a sell on the curve for `user`.
#[command]
pub fn market_sell(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    token_id: String,
    user: String,
    amount: u64,
) -> Result<Trade, String> {
    let dir = WalletState::data_dir(&app_handle)?;
    let mut desk = state.desk.lock().map_err(|e| e.to_string())?;
    let trade = desk
        .sell(&token_id, &user, amount, now_secs())
        .map_err(|e| e.to_string())?;
    store::save(&dir, &desk)?;
    Ok(trade)
}

/// A user's token balance held via the desk.
#[command]
pub fn market_position(
    state: State<'_, WalletState>,
    token_id: String,
    user: String,
) -> Result<u64, String> {
    let desk = state.desk.lock().map_err(|e| e.to_string())?;
    let m = desk.market(&token_id).map_err(|e| e.to_string())?;
    Ok(m.balances.get(&user).copied().unwrap_or(0))
}

/// Creator control: pause or resume trading. A migrated market cannot be paused.
#[command]
pub fn market_set_paused(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    token_id: String,
    paused: bool,
) -> Result<(), String> {
    let dir = WalletState::data_dir(&app_handle)?;
    let mut desk = state.desk.lock().map_err(|e| e.to_string())?;
    {
        let m = desk
            .markets
            .get_mut(&token_id)
            .ok_or_else(|| "market not found".to_string())?;
        m.status = match (m.status, paused) {
            (MarketStatus::Migrated, _) => return Err("market already migrated".to_string()),
            (_, true) => MarketStatus::Paused,
            (_, false) => MarketStatus::Trading,
        };
    }
    store::save(&dir, &desk)
}

/// Withdraw `amount` tokens of a market's asset on-chain to `address` (a Taproot
/// Asset address that already encodes the asset and amount). The asset moves via
/// tapd `send_asset`; the custodial ledger is debited only **after** the send
/// succeeds. The tokens stay in circulation (counted as `withdrawn`), so supply
/// and reserve are untouched. Returns the anchor txid.
#[command]
pub async fn market_withdraw_asset(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    token_id: String,
    user: String,
    amount: u64,
    address: String,
    fee_rate_sat_vb: u32,
) -> Result<String, String> {
    if amount == 0 {
        return Err("amount must be > 0".to_string());
    }
    // Pre-check the custodial balance, then drop the (std) desk lock before any
    // await — never hold a std Mutex across an await point.
    {
        let desk = state.desk.lock().map_err(|e| e.to_string())?;
        let m = desk.market(&token_id).map_err(|e| e.to_string())?;
        if m.balances.get(&user).copied().unwrap_or(0) < amount {
            return Err("insufficient token balance".to_string());
        }
    }
    // Move the asset on-chain via tapd.
    let txid = {
        let mut guard = state.taproot.lock().await;
        let client = guard.as_mut().ok_or("tapd not connected")?;
        client
            .send_asset(&address, fee_rate_sat_vb)
            .await
            .map_err(|e| e.to_string())?
    };
    // Debit the ledger + record the withdrawn supply, then persist.
    let dir = WalletState::data_dir(&app_handle)?;
    {
        let mut desk = state.desk.lock().map_err(|e| e.to_string())?;
        desk.withdraw(&token_id, &user, amount)
            .map_err(|e| e.to_string())?;
        store::save(&dir, &desk)?;
    }
    Ok(txid)
}
