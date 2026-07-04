use std::sync::Arc;

use tauri::{command, AppHandle, State};
use url::Url;
use zeroize::Zeroizing;

use crate::ark;
use crate::backup;
use crate::onchain;
use crate::tapd_defaults::TapdDefaults;
use crate::taproot::{self, TAPD_MACAROON_KEY};
use crate::tor::TorConfig;
use crate::wallet::{self, VaultError};
use crate::WalletState;
use bitcoin::Network;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ArkConfigDto {
    pub server_address: String,
    pub esplora_address: Option<String>,
    pub server_access_token: Option<String>,
    pub network: String,
}

fn parse_network(s: &str) -> Result<Network, String> {
    match s.to_ascii_lowercase().as_str() {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "testnet" => Ok(Network::Testnet),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        _ => Err(format!("unsupported network: {s}")),
    }
}

#[derive(serde::Serialize)]
pub struct WalletStatus {
    pub exists: bool,
    pub unlocked: bool,
}

#[command]
pub fn generate_seed(word_count: usize) -> Result<String, String> {
    wallet::generate_mnemonic(word_count)
        .map(|m| m.to_string())
        .map_err(|e| e.to_string())
}

const ARK_TOKEN_KEY: &str = "ark_server_access_token";

async fn initialize_wallet_state(
    app_handle: &AppHandle,
    state: &State<'_, WalletState>,
    mnemonic: &str,
    password: &str,
) -> Result<(), String> {
    let mut ark_config = ark::load_ark_config(app_handle)?;
    let network = parse_network(&ark_config.network)?;

    let data_dir = WalletState::data_dir(app_handle)?;
    let onchain_db_path = data_dir.join("ozark-onchain.db");

    // Load the ASP access token from the encrypted vault, if one was saved.
    match wallet::load_secret(app_handle, password, ARK_TOKEN_KEY) {
        Ok(token) => ark_config.server_access_token = Some(token),
        Err(VaultError::NotInitialized) | Err(VaultError::InvalidPassword) => {
            // No token stored yet; keep the config as-is (likely None).
        }
        Err(e) => return Err(e.to_string()),
    }

    let bdk_wallet = onchain::create_wallet(mnemonic, None, network, &onchain_db_path)
        .map_err(|e| e.to_string())?;
    {
        let mut guard = state.onchain.lock().map_err(|e| e.to_string())?;
        *guard = Some(Arc::new(tokio::sync::Mutex::new(bdk_wallet)));
    }
    {
        let mut guard = state.onchain_db_path.lock().map_err(|e| e.to_string())?;
        *guard = Some(onchain_db_path);
    }

    // Derive the marketplace Nostr identity from the same seed (NIP-06). Failure
    // is non-fatal — it only disables the remote marketplace, not the wallet.
    match crate::market::identity::keys_from_mnemonic(mnemonic) {
        Ok(keys) => {
            if let Ok(mut guard) = state.nostr.lock() {
                *guard = Some(keys);
            }
        }
        Err(e) => log::error!("nostr identity derivation failed: {e}"),
    }

    // Abort any background tasks left over from a previous unlock and reset status.
    if let Ok(mut tasks) = state.bg_tasks.lock() {
        for t in tasks.drain(..) {
            t.abort();
        }
    }
    if let Ok(mut init) = state.bg_init.lock() {
        init.ark = crate::TaskState::Pending;
        init.tapd = crate::TaskState::Pending;
    }

    // Start ARK service in the background so a network failure does not block unlock.
    let data_dir = WalletState::data_dir(app_handle)?;
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let ark_db = data_dir.join("ozark-wallet.db");
    let ark_state = state.ark.clone();
    let ark_init = state.bg_init.clone();
    let mnemonic_for_ark = mnemonic.to_string();
    let ark_handle = tauri::async_runtime::spawn(async move {
        match ark::ArkService::start(mnemonic_for_ark, network, ark_db, ark_config).await {
            Ok(service) => {
                if let Ok(mut guard) = ark_state.lock() {
                    *guard = Some(service);
                }
                if let Ok(mut init) = ark_init.lock() {
                    init.ark = crate::TaskState::Ready;
                }
            }
            Err(e) => {
                log::error!("ark service failed to start: {e}");
                if let Ok(mut init) = ark_init.lock() {
                    init.ark = crate::TaskState::Failed(e);
                }
            }
        }
    });

    // Auto-connect tapd in the background (saved config, else the embedded default
    // node). Spawned so a slow Tor onion connect never blocks wallet unlock.
    let app_for_tapd = app_handle.clone();
    let tor_arc = state.tor.clone();
    let taproot_arc = state.taproot.clone();
    let tapd_init = state.bg_init.clone();
    let password_for_tapd = password.to_string();
    let tapd_handle = tauri::async_runtime::spawn(async move {
        match taproot::reconnect_tapd_bg(app_for_tapd, tor_arc, taproot_arc, password_for_tapd)
            .await
        {
            Ok(true) => {
                if let Ok(mut init) = tapd_init.lock() {
                    init.tapd = crate::TaskState::Ready;
                }
            }
            Ok(false) => {
                // No saved config and no defaults — OK, user will connect manually
                if let Ok(mut init) = tapd_init.lock() {
                    init.tapd = crate::TaskState::Idle;
                }
            }
            Err(e) => {
                log::error!("tapd auto-connect failed: {e}");
                if let Ok(mut init) = tapd_init.lock() {
                    init.tapd = crate::TaskState::Failed(e);
                }
            }
        }
    });

    // Supervisor: keep tapd connected mid-session (health-check + auto-reconnect),
    // and retry the initial connect if the first onion attempt was flaky.
    let sup_app = app_handle.clone();
    let sup_tor = state.tor.clone();
    let sup_taproot = state.taproot.clone();
    let sup_password = password.to_string();
    let sup_handle = tauri::async_runtime::spawn(async move {
        taproot::supervise_tapd(sup_app, sup_tor, sup_taproot, sup_password).await;
    });

    // Keep the handles so they are supervised (and can be aborted on delete) rather
    // than fully detached.
    if let Ok(mut tasks) = state.bg_tasks.lock() {
        tasks.push(ark_handle);
        tasks.push(tapd_handle);
        tasks.push(sup_handle);
    }

    Ok(())
}

/// Report the status of the background unlock tasks (Ark service + tapd connect)
/// so the UI can surface a failure instead of it being silently swallowed.
#[command]
pub fn get_background_init_status(
    state: State<'_, WalletState>,
) -> Result<crate::BackgroundInit, String> {
    state
        .bg_init
        .lock()
        .map(|g| g.clone())
        .map_err(|e| e.to_string())
}

#[command]
pub async fn create_new_wallet(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    password: String,
    word_count: usize,
) -> Result<String, String> {
    // Wipe the password from memory when this command returns.
    let password = Zeroizing::new(password);
    let phrase = wallet::generate_wallet(&app_handle, password.as_str(), word_count)
        .map_err(|e| e.to_string())?;
    initialize_wallet_state(&app_handle, &state, &phrase, password.as_str()).await?;
    Ok(phrase)
}

#[command]
pub async fn import_wallet(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    password: String,
    mnemonic: String,
) -> Result<String, String> {
    let password = Zeroizing::new(password);
    wallet::create_wallet(&app_handle, password.as_str(), mnemonic.as_str())
        .map_err(|e| e.to_string())?;
    initialize_wallet_state(&app_handle, &state, &mnemonic, password.as_str()).await?;
    Ok(mnemonic)
}

#[command]
pub async fn unlock_wallet_command(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    password: String,
) -> Result<bool, String> {
    let password = Zeroizing::new(password);
    let mnemonic = Zeroizing::new(
        wallet::unlock_and_get_mnemonic(&app_handle, password.as_str())
            .map_err(|e| e.to_string())?,
    );
    initialize_wallet_state(&app_handle, &state, &mnemonic, password.as_str()).await?;

    Ok(true)
}

#[command]
pub fn wallet_exists(app_handle: AppHandle) -> Result<bool, String> {
    Ok(wallet::has_wallet(&app_handle))
}

#[command]
pub async fn get_wallet_status(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
) -> Result<WalletStatus, String> {
    let exists = wallet::has_wallet(&app_handle);
    let onchain_unlocked = state.onchain.lock().map_err(|e| e.to_string())?.is_some();
    let ark_unlocked = state.ark.lock().map_err(|e| e.to_string())?.is_some();
    let taproot_unlocked = state.taproot.lock().await.is_some();
    Ok(WalletStatus {
        exists,
        unlocked: exists && (onchain_unlocked || ark_unlocked || taproot_unlocked),
    })
}

#[command]
pub fn reveal_mnemonic(app_handle: AppHandle, password: String) -> Result<String, String> {
    let password = Zeroizing::new(password);
    wallet::get_mnemonic(&app_handle, password.as_str()).map_err(|e| e.to_string())
}

#[command]
pub fn change_wallet_password(
    app_handle: AppHandle,
    old_password: String,
    new_password: String,
) -> Result<(), String> {
    let old_password = Zeroizing::new(old_password);
    let new_password = Zeroizing::new(new_password);
    // Preserve encrypted secrets that are not part of the seed snapshot.
    let token = wallet::load_secret(&app_handle, old_password.as_str(), ARK_TOKEN_KEY)
        .ok()
        .map(Zeroizing::new);
    let macaroon = wallet::load_secret(&app_handle, old_password.as_str(), TAPD_MACAROON_KEY)
        .ok()
        .map(Zeroizing::new);

    wallet::change_password(&app_handle, old_password.as_str(), new_password.as_str())
        .map_err(|e| e.to_string())?;

    if let Some(t) = token {
        wallet::store_secret(&app_handle, new_password.as_str(), ARK_TOKEN_KEY, &t)
            .map_err(|e| e.to_string())?;
    }
    if let Some(m) = macaroon {
        wallet::store_secret(&app_handle, new_password.as_str(), TAPD_MACAROON_KEY, &m)
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[command]
pub async fn delete_wallet_command(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
) -> Result<(), String> {
    // 1. Stop background tasks so nothing rewrites files while we purge.
    if let Ok(mut tasks) = state.bg_tasks.lock() {
        for t in tasks.drain(..) {
            t.abort();
        }
    }

    // 2. Drop the encrypted vault (snapshot + salt + leftover bak/tmp files).
    wallet::delete_wallet(&app_handle).map_err(|e| e.to_string())?;

    // 3. Clear in-memory wallet state so the UI cannot keep spending from a deleted
    //    wallet. Dropping the tapd client also tears down its gRPC streams (Drop).
    if let Ok(mut guard) = state.onchain.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = state.ark.lock() {
        *guard = None;
    }
    {
        let mut guard = state.taproot.lock().await;
        *guard = None;
    }
    if let Ok(mut guard) = state.onchain_db_path.lock() {
        *guard = None;
    }
    if let Ok(mut init) = state.bg_init.lock() {
        *init = crate::BackgroundInit::default();
    }

    // 4. Remove every on-disk data file this wallet created so no plaintext config,
    //    SQLite database, or Tor cache survives the reset.
    let data_dir = WalletState::data_dir(&app_handle)?;
    purge_wallet_files(&data_dir);

    Ok(())
}

/// Remove all wallet data files under `data_dir`. Best-effort: missing files are
/// ignored. Centralizes the deletion list so no data store is forgotten.
fn purge_wallet_files(data_dir: &std::path::Path) {
    // SQLite databases (with their journal/WAL/SHM sidecars).
    for db in ["ozark-onchain.db", "ozark-wallet.db"] {
        let base = data_dir.join(db);
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                base.clone()
            } else {
                let mut s = base.clone().into_os_string();
                s.push(suffix);
                std::path::PathBuf::from(s)
            };
            if p.exists() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    // Plaintext JSON configuration files.
    for cfg in ["ark-config.json", "tor-config.json", "tapd-config.json"] {
        let p = data_dir.join(cfg);
        if p.exists() {
            let _ = std::fs::remove_file(&p);
        }
    }

    // Embedded Tor state/cache directories.
    for dir in ["tor-state", "tor-cache"] {
        let p = data_dir.join(dir);
        if p.exists() {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

#[command]
pub fn validate_mnemonic_command(mnemonic: String) -> Result<bool, String> {
    match wallet::validate_mnemonic(mnemonic.as_str()) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[command]
pub async fn get_new_address(state: State<'_, WalletState>) -> Result<String, String> {
    let wallet = {
        let guard = state.onchain.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    let db_path = {
        let guard = state.onchain_db_path.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    onchain::get_new_address(&wallet, &db_path)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn get_balance(state: State<'_, WalletState>) -> Result<u64, String> {
    let wallet = {
        let guard = state.onchain.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    let balance = onchain::get_balance(&wallet)
        .await
        .map_err(|e| e.to_string())?;
    Ok(balance.total().to_sat())
}

#[command]
pub async fn sync_wallet_command(state: State<'_, WalletState>) -> Result<(), String> {
    let wallet = {
        let guard = state.onchain.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    let db_path = {
        let guard = state.onchain_db_path.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    onchain::sync_wallet(&wallet, &db_path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[command]
pub async fn send_onchain(
    state: State<'_, WalletState>,
    address: String,
    amount_sats: u64,
    fee_rate: u64,
) -> Result<String, String> {
    let wallet = {
        let guard = state.onchain.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    let db_path = {
        let guard = state.onchain_db_path.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    onchain::send_to_address(&wallet, &db_path, &address, amount_sats, fee_rate)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn get_ark_address_command(state: State<'_, WalletState>) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.new_address().await
}

#[command]
pub async fn get_arkade_address_command(state: State<'_, WalletState>) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.new_arkade_address().await
}

#[command]
pub async fn sync_ark_wallet_command(state: State<'_, WalletState>) -> Result<(), String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.sync().await
}

#[command]
pub async fn get_ark_balance_command(state: State<'_, WalletState>) -> Result<u64, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.balance().await
}

#[command]
pub async fn pay_lightning_invoice(
    state: State<'_, WalletState>,
    invoice: String,
    amount_sats: Option<u64>,
) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.pay_lightning_invoice(invoice, amount_sats).await
}

#[derive(serde::Serialize)]
pub struct DecodedInvoice {
    pub amount_sats: Option<u64>,
    pub is_expired: bool,
    pub expiry_seconds: u64,
    pub payment_hash: String,
}

/// Decode a BOLT11 invoice without spending, so the UI (and deep-link handler)
/// can show and validate the amount/expiry before the user pays.
#[command]
pub fn decode_lightning_invoice(invoice: String) -> Result<DecodedInvoice, String> {
    use bark::lightning_invoice::Bolt11Invoice;
    let inv = invoice
        .trim()
        .parse::<Bolt11Invoice>()
        .map_err(|e| format!("invalid lightning invoice: {e}"))?;
    Ok(DecodedInvoice {
        amount_sats: inv.amount_milli_satoshis().map(|m| m / 1000),
        is_expired: inv.is_expired(),
        expiry_seconds: inv.expiry_time().as_secs(),
        payment_hash: inv.payment_hash().to_string(),
    })
}

#[command]
pub async fn create_bolt11_invoice(
    state: State<'_, WalletState>,
    amount_sats: u64,
    description: String,
) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service
        .create_bolt11_invoice(amount_sats, Some(description).filter(|d| !d.is_empty()))
        .await
}

#[command]
pub async fn claim_lightning_receives(state: State<'_, WalletState>) -> Result<(), String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.claim_lightning_receives().await
}

#[command]
pub async fn send_ark_payment(
    state: State<'_, WalletState>,
    address: String,
    amount_sats: u64,
) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.send_ark_payment(address, amount_sats).await
}

#[command]
pub async fn get_board_funding_address(state: State<'_, WalletState>) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.board_funding_address().await
}

#[command]
pub async fn connect_tapd(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
    password: String,
    host: String,
    cert_pem: String,
    macaroon_hex: String,
    use_tor: bool,
) -> Result<bool, String> {
    if macaroon_hex.trim().is_empty() {
        return Err("tapd macaroon is required".into());
    }

    log::info!(
        "connect_tapd called; host={host} use_tor={use_tor} onion={}",
        host.trim().ends_with(".onion")
    );

    let config = taproot::TapdConfig {
        host: host.clone(),
        cert_pem: cert_pem.clone(),
        macaroon_hex: macaroon_hex.clone(),
    };

    let tor_service = if use_tor || host.trim().ends_with(".onion") {
        let tor = state.tor.lock().await.clone();
        tor.start().await?;
        Some(tor)
    } else {
        None
    };

    let client = match taproot::TaprootClient::connect(config, tor_service, use_tor).await {
        Ok(c) => c,
        Err(e) => {
            log::error!("connect_tapd failed: {e}");
            return Err(e.to_string());
        }
    };

    // Persist host/cert as plaintext; macaroon goes to Stronghold.
    let config_to_save = taproot::TapdConfig {
        host,
        cert_pem,
        macaroon_hex: String::new(),
    };
    taproot::save_tapd_config(&app_handle, &config_to_save).map_err(|e| e.to_string())?;
    wallet::store_secret(
        &app_handle,
        password.as_str(),
        TAPD_MACAROON_KEY,
        macaroon_hex.as_str(),
    )
    .map_err(|e| e.to_string())?;

    client.spawn_event_streams(app_handle.clone());
    {
        let mut guard = state.taproot.lock().await;
        *guard = Some(client);
    }
    Ok(true)
}

/// Connect to the embedded default Umbrel tapd node without a password (its
/// macaroon is baked into the app). Idempotent: returns immediately if tapd is
/// already connected. Used by the UI to auto-connect on open.
#[command]
pub async fn connect_default_tapd(
    app_handle: AppHandle,
    state: State<'_, WalletState>,
) -> Result<bool, String> {
    if state.taproot.lock().await.is_some() {
        return Ok(true);
    }

    let defaults = TapdDefaults::load();
    if defaults.is_empty() {
        return Err("no default tapd node is configured".into());
    }

    // Robustly detect .onion even when the host carries a scheme and/or a port
    // (e.g. "https://abc.onion:10029"): parse it the same way `connect()` does
    // and inspect only the host component. A naive `ends_with(".onion")` on the
    // raw string is false for a ported host, which would leave Tor disabled
    // (TorConfig defaults to enabled=false) and fail with "Tor is required for
    // .onion tapd hosts".
    let is_onion = {
        let h = defaults.host.trim();
        let normalized = if h.starts_with("http://") || h.starts_with("https://") {
            h.to_string()
        } else {
            format!("https://{h}")
        };
        url::Url::parse(&normalized)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.ends_with(".onion")))
            .unwrap_or(false)
    };
    let config = taproot::TapdConfig {
        host: defaults.host,
        cert_pem: defaults.cert_pem,
        macaroon_hex: defaults.macaroon_hex,
    };

    let tor_config = TorConfig::load(&app_handle).unwrap_or_default();
    let use_tor = is_onion || tor_config.force_tor || tor_config.enabled;

    let tor_service = if use_tor {
        let tor = state.tor.lock().await.clone();
        tor.start().await?;
        Some(tor)
    } else {
        None
    };

    let client = taproot::TaprootClient::connect(config, tor_service, use_tor)
        .await
        .map_err(|e| e.to_string())?;

    client.spawn_event_streams(app_handle.clone());
    *state.taproot.lock().await = Some(client);
    Ok(true)
}

/// Whether a tapd client is currently connected.
#[command]
pub async fn get_tapd_status(state: State<'_, WalletState>) -> Result<bool, String> {
    Ok(state.taproot.lock().await.is_some())
}

/// Drop the current tapd connection so the user can connect to a different node.
#[command]
pub async fn disconnect_tapd(state: State<'_, WalletState>) -> Result<(), String> {
    *state.taproot.lock().await = None;
    Ok(())
}

#[command]
pub async fn list_taproot_balances(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::AssetBalanceSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.list_balances().await.map_err(|e| e.to_string())
}

#[command]
pub async fn list_taproot_transfers(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::TransferSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.list_transfers().await.map_err(|e| e.to_string())
}

#[command]
pub async fn list_taproot_batches(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::BatchSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.list_batches().await.map_err(|e| e.to_string())
}

#[command]
pub async fn cancel_taproot_batch(state: State<'_, WalletState>) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.cancel_batch().await.map_err(|e| e.to_string())
}

#[command]
pub async fn list_taproot_burns(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::BurnSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.list_burns().await.map_err(|e| e.to_string())
}

#[command]
pub async fn taproot_addr_receives(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::AddrReceiveSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.addr_receives().await.map_err(|e| e.to_string())
}

#[command]
pub async fn fetch_taproot_asset_meta(
    state: State<'_, WalletState>,
    asset_id: String,
) -> Result<taproot::AssetMetaSummary, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .fetch_asset_meta(&asset_id)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn get_taproot_info(
    state: State<'_, WalletState>,
) -> Result<taproot::NodeInfoSummary, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.get_info().await.map_err(|e| e.to_string())
}

#[command]
pub async fn decode_taproot_addr(
    state: State<'_, WalletState>,
    address: String,
) -> Result<taproot::DecodedAddrSummary, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .decode_addr(&address)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn burn_taproot_asset(
    state: State<'_, WalletState>,
    asset_id: String,
    amount: u64,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .burn_asset(&asset_id, amount)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn get_universe_stats(
    state: State<'_, WalletState>,
) -> Result<taproot::UniverseStatsSummary, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.universe_stats().await.map_err(|e| e.to_string())
}

#[command]
pub async fn list_universe_roots(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::UniverseRootSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.universe_roots().await.map_err(|e| e.to_string())
}

#[command]
pub async fn sync_universe(state: State<'_, WalletState>, host: String) -> Result<usize, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.universe_sync(&host).await.map_err(|e| e.to_string())
}

#[command]
pub async fn list_federation_servers(state: State<'_, WalletState>) -> Result<Vec<String>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .list_federation_servers()
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn add_federation_server(
    state: State<'_, WalletState>,
    host: String,
) -> Result<(), String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .add_federation_server(&host)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn delete_federation_server(
    state: State<'_, WalletState>,
    host: String,
) -> Result<(), String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .delete_federation_server(&host)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn decode_asset_invoice(
    state: State<'_, WalletState>,
    pay_req: String,
    asset_id: String,
) -> Result<taproot::DecodedAssetInvoice, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .decode_asset_invoice(&pay_req, &asset_id)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn list_rfq_quotes(
    state: State<'_, WalletState>,
) -> Result<taproot::RfqQuotesSummary, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.list_rfq_quotes().await.map_err(|e| e.to_string())
}

#[command]
pub async fn create_asset_invoice(
    state: State<'_, WalletState>,
    asset_id: String,
    asset_amount: u64,
    peer_pubkey: String,
    memo: String,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .create_asset_invoice(&asset_id, asset_amount, &peer_pubkey, &memo)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn fund_asset_channel(
    state: State<'_, WalletState>,
    asset_id: String,
    asset_amount: u64,
    peer_pubkey: String,
    fee_rate_sat_vb: u32,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .fund_asset_channel(&asset_id, asset_amount, &peer_pubkey, fee_rate_sat_vb)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn pay_asset_invoice(
    state: State<'_, WalletState>,
    pay_req: String,
    asset_id: String,
    peer_pubkey: String,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .pay_asset_invoice(&pay_req, &asset_id, &peer_pubkey)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn list_taproot_assets(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::AssetSummary>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.list_assets().await.map_err(|e| e.to_string())
}

#[command]
pub async fn mint_taproot_asset(
    state: State<'_, WalletState>,
    name: String,
    amount: u64,
    metadata: String,
    collectible: bool,
    new_group: bool,
    fee_rate_sat_vb: u32,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .mint_asset(
            &name,
            amount,
            &metadata,
            collectible,
            new_group,
            fee_rate_sat_vb,
        )
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn new_taproot_address(
    state: State<'_, WalletState>,
    asset_id: String,
    amount: u64,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .new_address(&asset_id, amount)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn send_taproot_asset(
    state: State<'_, WalletState>,
    address: String,
    fee_rate_sat_vb: u32,
) -> Result<String, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .send_asset(&address, fee_rate_sat_vb)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn export_taproot_proofs(
    state: State<'_, WalletState>,
) -> Result<Vec<taproot::ProofBackup>, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client.export_proofs().await.map_err(|e| e.to_string())
}

#[command]
pub async fn verify_taproot_proof(
    state: State<'_, WalletState>,
    proof_base64: String,
) -> Result<bool, String> {
    let mut guard = state.taproot.lock().await;
    let client = guard.as_mut().ok_or("tapd not connected")?;
    client
        .verify_proof(&proof_base64)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub fn encrypt_backup(plaintext: String, password: String) -> Result<String, String> {
    backup::encrypt(&plaintext, &password).map_err(|e| e.to_string())
}

#[command]
pub fn decrypt_backup(ciphertext: String, password: String) -> Result<String, String> {
    backup::decrypt(&ciphertext, &password).map_err(|e| e.to_string())
}

#[command]
pub fn load_ark_config_command(app_handle: AppHandle) -> Result<ArkConfigDto, String> {
    let cfg = ark::load_ark_config(&app_handle)?;
    // The access token is stored encrypted in Stronghold, not in this plaintext file.
    Ok(ArkConfigDto {
        server_address: cfg.server_address,
        esplora_address: cfg.esplora_address,
        server_access_token: None,
        network: cfg.network,
    })
}

fn validate_ark_config(config: &ArkConfigDto) -> Result<(), String> {
    let network = parse_network(&config.network)?;

    if config.server_address.trim().is_empty() {
        return Err("ASP server address is required".into());
    }

    let server_url =
        Url::parse(&config.server_address).map_err(|_| "invalid ASP server URL".to_string())?;
    let esplora_ok = config
        .esplora_address
        .as_ref()
        .map(|u| Url::parse(u).map_err(|_| "invalid Esplora URL".to_string()))
        .transpose()?;

    // Mainnet ASP and Esplora endpoints must use HTTPS.
    if network == Network::Bitcoin {
        if server_url.scheme() != "https" {
            return Err("mainnet ASP server must use HTTPS".into());
        }
        if let Some(url) = esplora_ok {
            if url.scheme() != "https" {
                return Err("mainnet Esplora endpoint must use HTTPS".into());
            }
        }
    }

    Ok(())
}

#[command]
pub fn save_ark_config_command(
    app_handle: AppHandle,
    password: String,
    config: ArkConfigDto,
) -> Result<(), String> {
    // Validate network and URLs before persisting anything.
    validate_ark_config(&config)?;

    // Persist the non-sensitive part of the config without the token.
    let cfg = ark::ArkConfig {
        server_address: config.server_address,
        esplora_address: config.esplora_address,
        server_access_token: None,
        network: config.network,
    };
    ark::save_ark_config(&app_handle, &cfg)?;

    // Store the access token encrypted in Stronghold if one was provided.
    if let Some(token) = config.server_access_token.filter(|t| !t.is_empty()) {
        wallet::store_secret(&app_handle, password.as_str(), ARK_TOKEN_KEY, &token)
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[command]
pub async fn refresh_ark_vtxos_command(state: State<'_, WalletState>) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.refresh_vtxos().await
}

#[command]
pub async fn offboard_all_command(
    state: State<'_, WalletState>,
    address: String,
) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.offboard_all(address).await
}

#[command]
pub async fn send_ark_onchain_command(
    state: State<'_, WalletState>,
    address: String,
    amount_sats: u64,
) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.send_onchain(address, amount_sats).await
}

#[command]
pub async fn start_ark_exit_command(state: State<'_, WalletState>) -> Result<(), String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.start_exit().await
}

#[command]
pub async fn sync_ark_exits_command(state: State<'_, WalletState>) -> Result<(), String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.sync_exits().await
}

#[command]
pub async fn get_ark_exit_status_command(
    state: State<'_, WalletState>,
) -> Result<ark::service::ExitStatusSummary, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.exit_status().await
}

#[command]
pub async fn drain_ark_exits_command(
    state: State<'_, WalletState>,
    address: String,
) -> Result<String, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.drain_exits(address).await
}

#[command]
pub async fn get_onchain_history_command(
    state: State<'_, WalletState>,
) -> Result<Vec<onchain::OnchainTxSummary>, String> {
    let wallet = {
        let guard = state.onchain.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    onchain::get_transaction_history(&wallet)
        .await
        .map_err(|e| e.to_string())
}

#[command]
pub async fn get_ark_history_command(
    state: State<'_, WalletState>,
) -> Result<Vec<ark::service::ArkMovementSummary>, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.history().await
}

#[command]
pub async fn list_ark_vtxos_command(
    state: State<'_, WalletState>,
) -> Result<Vec<ark::service::VtxoSummary>, String> {
    let service = {
        let guard = state.ark.lock().map_err(|e| e.to_string())?;
        guard.as_ref().cloned().ok_or("Wallet not unlocked")?
    };
    service.list_vtxos().await
}

#[command]
pub async fn start_tor(state: State<'_, WalletState>) -> Result<String, String> {
    let tor = state.tor.lock().await.clone();
    tor.start().await?;
    Ok(format!("{:?}", tor.state().await))
}

#[command]
pub async fn stop_tor(state: State<'_, WalletState>) -> Result<String, String> {
    let tor = state.tor.lock().await.clone();
    tor.stop().await;
    Ok("stopped".into())
}

#[command]
pub async fn get_tor_status(state: State<'_, WalletState>) -> Result<String, String> {
    let tor = state.tor.lock().await.clone();
    Ok(format!("{:?}", tor.state().await))
}

#[command]
pub fn load_tor_config(app_handle: AppHandle) -> Result<TorConfig, String> {
    TorConfig::load(&app_handle)
}

#[command]
pub fn save_tor_config(app_handle: AppHandle, config: TorConfig) -> Result<(), String> {
    config.save(&app_handle)
}

#[command]
pub fn get_tapd_defaults() -> Result<TapdDefaults, String> {
    Ok(TapdDefaults::load())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_network() {
        assert_eq!(parse_network("bitcoin").unwrap(), Network::Bitcoin);
        assert_eq!(parse_network("mainnet").unwrap(), Network::Bitcoin);
        assert_eq!(parse_network("testnet").unwrap(), Network::Testnet);
        assert_eq!(parse_network("signet").unwrap(), Network::Signet);
        assert_eq!(parse_network("regtest").unwrap(), Network::Regtest);
        assert!(parse_network("invalid").is_err());
    }

    #[test]
    fn test_validate_ark_config_requires_https_on_mainnet() {
        let config = ArkConfigDto {
            server_address: "http://ark.example.com".into(),
            esplora_address: None,
            server_access_token: None,
            network: "bitcoin".into(),
        };
        assert!(validate_ark_config(&config).is_err());

        let config_ok = ArkConfigDto {
            server_address: "https://ark.second.tech".into(),
            esplora_address: Some("https://mempool.second.tech/api".into()),
            server_access_token: None,
            network: "bitcoin".into(),
        };
        assert!(validate_ark_config(&config_ok).is_ok());
    }

    #[test]
    fn test_validate_ark_config_allows_http_on_regtest() {
        let config = ArkConfigDto {
            server_address: "http://localhost:8080".into(),
            esplora_address: Some("http://localhost:3002".into()),
            server_access_token: None,
            network: "regtest".into(),
        };
        assert!(validate_ark_config(&config).is_ok());
    }
}
