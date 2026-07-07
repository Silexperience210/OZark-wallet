mod ark;
mod backup;
mod commands;
mod gateway;
mod kdf;
// `pub` so the not-yet-wired bonding-curve math counts as public API and does
// not trip clippy's `dead_code` under `-D warnings`. Made private once the desk
// commands consume it.
pub mod market;
mod onchain;
mod tapd_defaults;
mod taproot;
mod tor;
mod wallet;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ark::ArkService;
use onchain::SharedWallet;
use taproot::TaprootClient;
use tauri::Manager;
use tor::TorService;

/// Status of a background initialization task (Ark service start, tapd connect).
/// Surfaced to the frontend via `get_background_init_status` so a silent failure
/// during unlock is observable instead of fire-and-forget.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(tag = "state", content = "error", rename_all = "lowercase")]
pub enum TaskState {
    #[default]
    Idle,
    Pending,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BackgroundInit {
    pub ark: TaskState,
    pub tapd: TaskState,
}

pub type BgTask = tauri::async_runtime::JoinHandle<()>;

pub struct WalletState {
    pub onchain: Arc<Mutex<Option<SharedWallet>>>,
    pub onchain_db_path: Arc<Mutex<Option<PathBuf>>>,
    pub ark: Arc<Mutex<Option<ArkService>>>,
    pub taproot: Arc<tokio::sync::Mutex<Option<TaprootClient>>>,
    pub tor: Arc<tokio::sync::Mutex<TorService>>,
    /// Bonding-curve marketplace desk (reserves, ledger, trade log), loaded from
    /// the on-disk snapshot at startup and saved through on every trade.
    pub desk: Arc<Mutex<market::Desk>>,
    /// Marketplace Nostr identity (NIP-06), derived from the seed at unlock.
    /// `None` while the wallet is locked.
    pub nostr: Arc<Mutex<Option<nostr::Keys>>>,
    /// Remote settlement orders held by the desk, keyed by order id.
    pub orders: Arc<Mutex<std::collections::HashMap<String, market::settle::Order>>>,
    /// Observable status of the background unlock tasks.
    pub bg_init: Arc<Mutex<BackgroundInit>>,
    /// Handles to the spawned background tasks so they can be aborted on delete.
    pub bg_tasks: Arc<Mutex<Vec<BgTask>>>,
}

impl WalletState {
    pub fn new(data_dir: PathBuf) -> Self {
        let desk = market::store::load_or_default(&data_dir);
        let orders = market::store::load_orders(&data_dir);
        Self {
            onchain: Arc::new(Mutex::new(None)),
            onchain_db_path: Arc::new(Mutex::new(None)),
            ark: Arc::new(Mutex::new(None)),
            taproot: Arc::new(tokio::sync::Mutex::new(None)),
            tor: Arc::new(tokio::sync::Mutex::new(TorService::new(data_dir))),
            desk: Arc::new(Mutex::new(desk)),
            nostr: Arc::new(Mutex::new(None)),
            orders: Arc::new(Mutex::new(orders)),
            bg_init: Arc::new(Mutex::new(BackgroundInit::default())),
            bg_tasks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn data_dir(app_handle: &tauri::AppHandle) -> Result<PathBuf, String> {
        app_handle
            .path()
            .app_local_data_dir()
            .map_err(|e| e.to_string())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = WalletState::data_dir(app.handle())?;
            app.manage(WalletState::new(data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::generate_seed,
            commands::create_new_wallet,
            commands::import_wallet,
            commands::unlock_wallet_command,
            commands::wallet_exists,
            commands::get_wallet_status,
            commands::reveal_mnemonic,
            commands::change_wallet_password,
            commands::delete_wallet_command,
            commands::get_background_init_status,
            commands::validate_mnemonic_command,
            commands::get_new_address,
            commands::get_balance,
            commands::sync_wallet_command,
            commands::send_onchain,
            commands::get_ark_address_command,
            commands::get_arkade_address_command,
            commands::sync_ark_wallet_command,
            commands::get_ark_balance_command,
            commands::pay_lightning_invoice,
            commands::decode_lightning_invoice,
            commands::create_bolt11_invoice,
            commands::claim_lightning_receives,
            commands::send_ark_payment,
            commands::get_board_funding_address,
            commands::connect_tapd,
            commands::connect_default_tapd,
            commands::get_tapd_status,
            commands::disconnect_tapd,
            commands::list_taproot_assets,
            commands::mint_taproot_asset,
            commands::new_taproot_address,
            commands::send_taproot_asset,
            commands::export_taproot_proofs,
            commands::verify_taproot_proof,
            commands::list_taproot_balances,
            commands::list_taproot_transfers,
            commands::list_taproot_batches,
            commands::cancel_taproot_batch,
            commands::list_taproot_burns,
            commands::taproot_addr_receives,
            commands::fetch_taproot_asset_meta,
            commands::get_taproot_info,
            commands::decode_taproot_addr,
            commands::burn_taproot_asset,
            commands::get_universe_stats,
            commands::list_universe_roots,
            commands::sync_universe,
            commands::list_federation_servers,
            commands::add_federation_server,
            commands::delete_federation_server,
            commands::decode_asset_invoice,
            commands::list_rfq_quotes,
            commands::create_asset_invoice,
            commands::fund_asset_channel,
            commands::pay_asset_invoice,
            commands::encrypt_backup,
            commands::decrypt_backup,
            commands::load_ark_config_command,
            commands::save_ark_config_command,
            commands::refresh_ark_vtxos_command,
            commands::offboard_all_command,
            commands::send_ark_onchain_command,
            commands::start_ark_exit_command,
            commands::sync_ark_exits_command,
            commands::get_ark_exit_status_command,
            commands::drain_ark_exits_command,
            commands::get_onchain_history_command,
            commands::get_ark_history_command,
            commands::list_ark_vtxos_command,
            commands::start_tor,
            commands::stop_tor,
            commands::get_tor_status,
            commands::load_tor_config,
            commands::save_tor_config,
            commands::get_tapd_defaults,
            market::commands::market_create,
            market::commands::market_list,
            market::commands::market_get,
            market::commands::market_price_history,
            market::commands::market_quote_buy,
            market::commands::market_quote_sell,
            market::commands::market_buy,
            market::commands::market_sell,
            market::commands::market_position,
            market::commands::market_set_paused,
            market::commands::market_withdraw_asset,
            market::commands::get_nostr_identity,
            market::commands::market_publish,
            market::commands::market_discover,
            market::commands::market_remote_history,
            market::commands::market_remote_buy,
            market::commands::market_check_responses,
            market::commands::market_pay_and_prove,
            market::commands::market_remote_sell,
            gateway::commands::save_gateway_config,
            gateway::commands::load_gateway_config,
            gateway::commands::gateway_list_assets,
            gateway::commands::gateway_balance,
            gateway::commands::gateway_history,
            gateway::commands::gateway_asset_meta,
            gateway::commands::gateway_info,
            gateway::commands::gateway_ln_decode,
            gateway::commands::gateway_ln_rfq_quotes,
            gateway::commands::gateway_pubkey,
            gateway::commands::gateway_ln_pay,
            gateway::commands::gateway_ln_receive,
            gateway::commands::gateway_mint,
            gateway::commands::gateway_mint_status,
            gateway::commands::gateway_receive,
            gateway::commands::gateway_send,
            gateway::commands::gateway_burn,
            gateway::commands::gateway_transfer,
            gateway::commands::gateway_admin_claim,
            gateway::commands::gateway_admin_channels,
            gateway::commands::gateway_admin_channel_open,
            gateway::commands::gateway_admin_peer_connect,
        ]);

    #[cfg(mobile)]
    {
        builder = builder
            .plugin(tauri_plugin_barcode_scanner::init())
            .plugin(tauri_plugin_nfc::init());
    }

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
