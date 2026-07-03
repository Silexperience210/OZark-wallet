use std::path::PathBuf;
use std::sync::Arc;

use bark::lightning_invoice::Bolt11Invoice;
use bark::lock_manager::memory::MemoryLockManager;
use bark::persist::sqlite::SqliteClient;
use bark::persist::BarkPersister;
use bark::{Config, Wallet as BarkWallet};
use bitcoin::{Amount, Network};
use tokio::sync::{mpsc, oneshot};

use crate::ark::arkade_address::encode_arkade_address;
use crate::ark::config::ArkConfig;
use crate::wallet::seed::validate_mnemonic;

#[derive(Debug)]
enum Request {
    NewAddress {
        respond: oneshot::Sender<Result<String, String>>,
    },
    NewArkadeAddress {
        respond: oneshot::Sender<Result<String, String>>,
    },
    Sync {
        respond: oneshot::Sender<Result<(), String>>,
    },
    Balance {
        respond: oneshot::Sender<Result<u64, String>>,
    },
    PayLightningInvoice {
        invoice: String,
        amount_sats: Option<u64>,
        respond: oneshot::Sender<Result<String, String>>,
    },
    CreateBolt11Invoice {
        amount_sats: u64,
        description: Option<String>,
        respond: oneshot::Sender<Result<String, String>>,
    },
    ClaimLightningReceives {
        respond: oneshot::Sender<Result<(), String>>,
    },
    SendArkPayment {
        address: String,
        amount_sats: u64,
        respond: oneshot::Sender<Result<String, String>>,
    },
    BoardFundingAddress {
        respond: oneshot::Sender<Result<String, String>>,
    },
    History {
        respond: oneshot::Sender<Result<Vec<ArkMovementSummary>, String>>,
    },
    ListVtxos {
        respond: oneshot::Sender<Result<Vec<VtxoSummary>, String>>,
    },
    RefreshVtxos {
        respond: oneshot::Sender<Result<String, String>>,
    },
    OffboardAll {
        address: String,
        respond: oneshot::Sender<Result<String, String>>,
    },
    SendOnchain {
        address: String,
        amount_sats: u64,
        respond: oneshot::Sender<Result<String, String>>,
    },
    StartExit {
        respond: oneshot::Sender<Result<(), String>>,
    },
    SyncExits {
        respond: oneshot::Sender<Result<(), String>>,
    },
    ExitStatus {
        respond: oneshot::Sender<Result<ExitStatusSummary, String>>,
    },
    DrainExits {
        address: String,
        respond: oneshot::Sender<Result<String, String>>,
    },
}

/// Serializable summary of a single exit VTXO.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExitVtxoSummary {
    pub vtxo_id: String,
    pub amount_sats: u64,
    pub state: String,
    pub claimable: bool,
    pub pending: bool,
}

/// Serializable summary of a single spendable VTXO with its expiry height, so the
/// UI can warn before a VTXO expires (Ark VTXOs must be refreshed before expiry,
/// otherwise the funds require a unilateral on-chain exit).
#[derive(Debug, Clone, serde::Serialize)]
pub struct VtxoSummary {
    pub vtxo_id: String,
    pub amount_sats: u64,
    pub expiry_height: u32,
}

/// Serializable summary of a single Ark movement.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArkMovementSummary {
    pub id: u32,
    pub subsystem: String,
    pub kind: String,
    pub status: String,
    pub amount_sats: i64,
    pub fee_sats: u64,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub description: String,
}

/// Serializable summary of the current exit state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExitStatusSummary {
    pub has_pending: bool,
    pub pending_sats: Option<u64>,
    pub total_sats: u64,
    pub claimable_sats: u64,
    pub exits: Vec<ExitVtxoSummary>,
}

/// Handle to an Ark wallet running on a dedicated single-threaded runtime.
#[derive(Clone)]
pub struct ArkService {
    tx: mpsc::UnboundedSender<Request>,
}

impl ArkService {
    /// Start a dedicated thread that owns the Bark wallet.
    /// The wallet is created inside the thread so it never needs to cross thread boundaries.
    pub async fn start(
        mnemonic: String,
        network: Network,
        db_path: PathBuf,
        ark_config: ArkConfig,
    ) -> Result<Self, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
        let (init_tx, init_rx) = oneshot::channel::<Result<(), String>>();

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = init_tx.send(Err(format!("runtime build failed: {e}")));
                    return;
                }
            };

            rt.block_on(async {
                let wallet = async {
                    let mnemonic = validate_mnemonic(&mnemonic)
                        .map_err(|e| format!("invalid mnemonic: {e}"))?;

                    let mut config = Config::network_default(network);
                    config.server_address = ark_config.server_address;
                    config.esplora_address = ark_config.esplora_address;
                    config.server_access_token = ark_config.server_access_token;

                    let db =
                        Arc::new(SqliteClient::open(&db_path).map_err(|e| format!("sqlite: {e}"))?);

                    let lock_manager = Box::new(MemoryLockManager::new());

                    // If the wallet was already initialized, open it; otherwise create it.
                    let existing = db
                        .read_properties()
                        .await
                        .map_err(|e| format!("read properties: {e}"))?;

                    if existing.is_some() {
                        BarkWallet::open_with_exits(&mnemonic, db, config, lock_manager)
                            .await
                            .map_err(|e| format!("ark open failed: {e}"))
                    } else {
                        BarkWallet::create_with_exits(
                            &mnemonic,
                            network,
                            config,
                            db,
                            lock_manager,
                            false,
                        )
                        .await
                        .map_err(|e| format!("ark create failed: {e}"))
                    }
                }
                .await;

                let wallet = match wallet {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                let _ = init_tx.send(Ok(()));

                let wallet = tokio::sync::Mutex::new(wallet);

                while let Some(req) = rx.recv().await {
                    match req {
                        Request::NewAddress { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let addr = wallet.new_address().await.map_err(|e| e.to_string())?;
                                Ok::<_, String>(addr.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::NewArkadeAddress { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let ark_info =
                                    wallet.require_ark_info().await.map_err(|e| e.to_string())?;
                                let server_pubkey = ark_info.server_pubkey;
                                let exit_delta = ark_info.vtxo_exit_delta;

                                let (keypair, _) = wallet
                                    .derive_store_next_keypair()
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let user_pubkey = keypair.public_key();

                                let policy = bark::ark::VtxoPolicy::new_pubkey(user_pubkey);
                                let pubkey_policy = match policy {
                                    bark::ark::VtxoPolicy::Pubkey(p) => p,
                                    _ => {
                                        return Err("unexpected VTXO policy variant".to_string());
                                    }
                                };
                                let taproot_info = pubkey_policy.taproot(server_pubkey, exit_delta);
                                let output_key = taproot_info.output_key().serialize();
                                let server_xonly = server_pubkey.x_only_public_key().0.serialize();
                                let testnet = ark_info.network != bitcoin::Network::Bitcoin;

                                encode_arkade_address(testnet, server_xonly, output_key)
                                    .map_err(|e| e.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::Sync { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                wallet.sync().await;
                                Ok::<_, String>(())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::Balance { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let vtxos =
                                    wallet.spendable_vtxos().await.map_err(|e| e.to_string())?;
                                let sats: u64 = vtxos.iter().try_fold(0u64, |acc, v| {
                                    acc.checked_add(v.amount().to_sat())
                                        .ok_or_else(|| "ARK balance overflow".to_string())
                                })?;
                                Ok::<_, String>(sats)
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::PayLightningInvoice {
                            invoice,
                            amount_sats,
                            respond,
                        } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let invoice = invoice
                                    .parse::<Bolt11Invoice>()
                                    .map_err(|e| format!("invalid invoice: {e}"))?;
                                let amount = amount_sats
                                    .map(|a| {
                                        if a > Amount::MAX_MONEY.to_sat() {
                                            Err(format!("amount exceeds maximum: {a}"))
                                        } else {
                                            Ok(Amount::from_sat(a))
                                        }
                                    })
                                    .transpose()?;

                                // Verify the spendable Ark balance covers the invoice
                                // amount before attempting payment, so the user gets a
                                // clear error instead of a confusing failure deep in bark.
                                let required_sats = match amount {
                                    Some(a) => a.to_sat(),
                                    None => invoice
                                        .amount_milli_satoshis()
                                        .map(|msat| msat / 1000)
                                        .unwrap_or(0),
                                };
                                let spendable: u64 = wallet
                                    .spendable_vtxos()
                                    .await
                                    .map_err(|e| e.to_string())?
                                    .iter()
                                    .map(|v| v.amount().to_sat())
                                    .sum();
                                if required_sats > spendable {
                                    return Err(format!(
                                        "insufficient Ark balance: {spendable} sat available, \
                                         invoice needs at least {required_sats} sat (plus routing fees)"
                                    ));
                                }

                                let paid = wallet
                                    .pay_lightning_invoice(invoice, amount, true)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(paid.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::CreateBolt11Invoice {
                            amount_sats,
                            description,
                            respond,
                        } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                if amount_sats > Amount::MAX_MONEY.to_sat() {
                                    return Err(format!("amount exceeds maximum: {amount_sats}"));
                                }
                                let amount = Amount::from_sat(amount_sats);
                                let invoice = wallet
                                    .bolt11_invoice(amount, description)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(invoice.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::ClaimLightningReceives { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                wallet
                                    .try_claim_all_lightning_receives(false)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::SendArkPayment {
                            address,
                            amount_sats,
                            respond,
                        } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let address = address
                                    .parse::<bark::ark::Address>()
                                    .map_err(|e| format!("invalid ark address: {e}"))?;
                                if amount_sats > Amount::MAX_MONEY.to_sat() {
                                    return Err(format!("amount exceeds maximum: {amount_sats}"));
                                }
                                let amount = Amount::from_sat(amount_sats);
                                let vtxos = wallet
                                    .send_arkoor_payment(&address, amount)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(format!("sent ({} vtxo(s))", vtxos.len()))
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::BoardFundingAddress { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let (kp, _) = wallet
                                    .derive_store_next_keypair()
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let (addr, _) = wallet
                                    .board_funding_address(&kp)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(addr.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::History { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let movements =
                                    wallet.history().await.map_err(|e| e.to_string())?;
                                let summaries = movements
                                    .into_iter()
                                    .map(|m| {
                                        let description = m
                                            .lightning_invoice()
                                            .map(|i| i.to_string())
                                            .or_else(|| {
                                                m.sent_to
                                                    .first()
                                                    .or_else(|| m.received_on.first())
                                                    .map(|d| format!("{:?}", d.destination))
                                            })
                                            .unwrap_or_default();
                                        ArkMovementSummary {
                                            id: m.id.0,
                                            subsystem: m.subsystem.name,
                                            kind: m.subsystem.kind,
                                            status: m.status.to_string(),
                                            amount_sats: m.effective_balance.to_sat(),
                                            fee_sats: m.offchain_fee.to_sat(),
                                            created_at: m.time.created_at.timestamp(),
                                            completed_at: m
                                                .time
                                                .completed_at
                                                .map(|t| t.timestamp()),
                                            description,
                                        }
                                    })
                                    .collect();
                                Ok::<_, String>(summaries)
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::RefreshVtxos { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let vtxos =
                                    wallet.spendable_vtxos().await.map_err(|e| e.to_string())?;
                                let status = wallet
                                    .refresh_vtxos(&vtxos)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let msg = match status {
                                    Some(s) => format!("refresh initiated: {s:?}"),
                                    None => "no vtxos needed refresh".into(),
                                };
                                Ok::<_, String>(msg)
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::ListVtxos { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let vtxos = wallet
                                    .spendable_vtxos()
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let summaries = vtxos
                                    .iter()
                                    .map(|v| VtxoSummary {
                                        vtxo_id: v.id().to_string(),
                                        amount_sats: v.amount().to_sat(),
                                        expiry_height: v.expiry_height(),
                                    })
                                    .collect::<Vec<_>>();
                                Ok::<_, String>(summaries)
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::OffboardAll { address, respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let address = address
                                    .parse::<bitcoin::Address<_>>()
                                    .map_err(|e| format!("invalid bitcoin address: {e}"))?
                                    .require_network(network)
                                    .map_err(|e| format!("address network mismatch: {e}"))?;
                                let txid = wallet
                                    .offboard_all(address)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(txid.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::SendOnchain {
                            address,
                            amount_sats,
                            respond,
                        } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let address = address
                                    .parse::<bitcoin::Address<_>>()
                                    .map_err(|e| format!("invalid bitcoin address: {e}"))?
                                    .require_network(network)
                                    .map_err(|e| format!("address network mismatch: {e}"))?;
                                if amount_sats > Amount::MAX_MONEY.to_sat() {
                                    return Err(format!("amount exceeds maximum: {amount_sats}"));
                                }
                                let amount = Amount::from_sat(amount_sats);
                                let txid = wallet
                                    .send_onchain(address, amount)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(txid.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::StartExit { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                // Don't start a unilateral exit (which spends on-chain
                                // fees) when there are no funds to exit.
                                let spendable = wallet
                                    .spendable_vtxos()
                                    .await
                                    .map_err(|e| e.to_string())?;
                                if spendable.is_empty() {
                                    return Err(
                                        "no Ark funds to exit; nothing to do".to_string()
                                    );
                                }
                                wallet
                                    .exit_mgr()
                                    .start_exit_for_entire_wallet()
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::SyncExits { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let w = &*wallet;
                                wallet.exit_mgr().sync(w).await.map_err(|e| e.to_string())?;
                                Ok::<_, String>(())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::ExitStatus { respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let exits = wallet.exit_mgr().get_exit_vtxos().await;

                                let mut summaries = Vec::with_capacity(exits.len());
                                let mut total_sats: u64 = 0;
                                let mut claimable_sats: u64 = 0;
                                for exit in &exits {
                                    let sats = exit.amount().to_sat();
                                    total_sats = total_sats
                                        .checked_add(sats)
                                        .ok_or_else(|| "exit total overflow".to_string())?;
                                    if exit.is_claimable() {
                                        claimable_sats =
                                            claimable_sats.checked_add(sats).ok_or_else(|| {
                                                "claimable total overflow".to_string()
                                            })?;
                                    }
                                    summaries.push(ExitVtxoSummary {
                                        vtxo_id: exit.id().to_string(),
                                        amount_sats: sats,
                                        state: format!("{:?}", exit.state()),
                                        claimable: exit.is_claimable(),
                                        pending: exit.state().is_pending(),
                                    });
                                }

                                let pending_sats =
                                    wallet.exit_mgr().try_pending_total().map(|a| a.to_sat());

                                Ok::<_, String>(ExitStatusSummary {
                                    has_pending: wallet.exit_mgr().has_pending_exits().await,
                                    pending_sats,
                                    total_sats,
                                    claimable_sats,
                                    exits: summaries,
                                })
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                        Request::DrainExits { address, respond } => {
                            let res = async {
                                let wallet = wallet.lock().await;
                                let address = address
                                    .parse::<bitcoin::Address<_>>()
                                    .map_err(|e| format!("invalid bitcoin address: {e}"))?
                                    .require_network(network)
                                    .map_err(|e| format!("address network mismatch: {e}"))?;
                                let claimable = wallet.exit_mgr().list_claimable().await;
                                if claimable.is_empty() {
                                    return Err("no claimable exits".into());
                                }
                                let mut psbt = wallet
                                    .exit_mgr()
                                    .drain_exits(&claimable, &wallet, address, None)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                wallet
                                    .exit_mgr()
                                    .sign_exit_claim_inputs(&mut psbt, &wallet)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let tx =
                                    psbt.extract_tx().map_err(|e| format!("extract tx: {e}"))?;
                                let txid = tx.compute_txid();
                                wallet
                                    .chain()
                                    .broadcast_tx(&tx)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok::<_, String>(txid.to_string())
                            }
                            .await;
                            let _ = respond.send(res);
                        }
                    }
                }
            });
        });

        init_rx
            .await
            .map_err(|_| "ark service init cancelled")?
            .map(|_| Self { tx })
    }

    pub async fn new_address(&self) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::NewAddress { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn new_arkade_address(&self) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::NewArkadeAddress { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn sync(&self) -> Result<(), String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::Sync { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn balance(&self) -> Result<u64, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::Balance { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn pay_lightning_invoice(
        &self,
        invoice: String,
        amount_sats: Option<u64>,
    ) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::PayLightningInvoice {
                invoice,
                amount_sats,
                respond,
            })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn create_bolt11_invoice(
        &self,
        amount_sats: u64,
        description: Option<String>,
    ) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::CreateBolt11Invoice {
                amount_sats,
                description,
                respond,
            })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn claim_lightning_receives(&self) -> Result<(), String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::ClaimLightningReceives { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn send_ark_payment(
        &self,
        address: String,
        amount_sats: u64,
    ) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::SendArkPayment {
                address,
                amount_sats,
                respond,
            })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn board_funding_address(&self) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::BoardFundingAddress { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn history(&self) -> Result<Vec<ArkMovementSummary>, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::History { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn list_vtxos(&self) -> Result<Vec<VtxoSummary>, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::ListVtxos { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn refresh_vtxos(&self) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::RefreshVtxos { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn offboard_all(&self, address: String) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::OffboardAll { address, respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn send_onchain(&self, address: String, amount_sats: u64) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::SendOnchain {
                address,
                amount_sats,
                respond,
            })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn start_exit(&self) -> Result<(), String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::StartExit { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn sync_exits(&self) -> Result<(), String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::SyncExits { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn exit_status(&self) -> Result<ExitStatusSummary, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::ExitStatus { respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }

    pub async fn drain_exits(&self, address: String) -> Result<String, String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(Request::DrainExits { address, respond })
            .map_err(|_| "ark service stopped")?;
        rx.await.map_err(|_| "ark service dropped response")?
    }
}
