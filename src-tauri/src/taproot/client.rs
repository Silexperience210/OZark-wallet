use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::tor::{ArtiConnector, TorService};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use tauri::{AppHandle, Emitter, Manager};
use tonic::{
    service::{interceptor::InterceptedService, Interceptor},
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
    Request, Status,
};
use zeroize::Zeroizing;

pub const TAPD_MACAROON_KEY: &str = "tapd_macaroon";

#[allow(clippy::all, dead_code)]
pub mod taprpc {
    tonic::include_proto!("taprpc");
}

#[allow(clippy::all, dead_code)]
pub mod mintrpc {
    tonic::include_proto!("mintrpc");
}

#[allow(clippy::all, dead_code)]
pub mod universerpc {
    tonic::include_proto!("universerpc");
}

#[allow(clippy::all, dead_code)]
pub mod lnrpc {
    tonic::include_proto!("lnrpc");
}

#[allow(clippy::all, dead_code)]
pub mod routerrpc {
    tonic::include_proto!("routerrpc");
}

#[allow(clippy::all, dead_code)]
pub mod rfqrpc {
    tonic::include_proto!("rfqrpc");
}

#[allow(clippy::all, dead_code)]
pub mod priceoraclerpc {
    tonic::include_proto!("priceoraclerpc");
}

#[allow(clippy::all, dead_code)]
pub mod tapchannelrpc {
    tonic::include_proto!("tapchannelrpc");
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TapdConfig {
    pub host: String,
    pub cert_pem: String,
    pub macaroon_hex: String,
}

#[derive(Clone)]
struct MacaroonInterceptor {
    // Zeroized on drop so the admin macaroon does not linger in memory after the
    // client is dropped (disconnect_tapd / wallet delete).
    macaroon: Zeroizing<String>,
    // Shared across all gRPC clients of one connection so the rate limit is global.
    limiter: Arc<TokenBucket>,
}

impl Interceptor for MacaroonInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        // Single choke point: every RPC (unary and streaming) passes through here,
        // so rate limiting one buggy poll loop protects the whole tapd surface.
        if !self.limiter.try_acquire() {
            return Err(Status::resource_exhausted(
                "tapd request rate limit exceeded; please slow down",
            ));
        }
        let value = self
            .macaroon
            .as_str()
            .parse()
            .map_err(|_| Status::invalid_argument("invalid macaroon"))?;
        req.metadata_mut().insert("macaroon", value);
        Ok(req)
    }
}

/// Token-bucket rate limiter. Non-blocking and synchronous so it can be called
/// from the tonic interceptor. Refills continuously up to `capacity` tokens.
struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: std::sync::Mutex<(f64, Instant)>,
}

impl TokenBucket {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            state: std::sync::Mutex::new((capacity, Instant::now())),
        }
    }

    /// Take one token if available. Returns false when the bucket is empty.
    fn try_acquire(&self) -> bool {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (mut tokens, last) = *guard;
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_secs_f64();
        tokens = (tokens + elapsed * self.refill_per_sec).min(self.capacity);
        if tokens >= 1.0 {
            tokens -= 1.0;
            *guard = (tokens, now);
            true
        } else {
            *guard = (tokens, now);
            false
        }
    }
}

/// Holds the JoinHandles of live event-subscription tasks so they are aborted when
/// the `TaprootClient` is dropped (disconnect_tapd / wallet delete), instead of
/// leaking — which would keep the gRPC streams (and the macaroon) alive.
#[derive(Default)]
struct EventStreamHandles(std::sync::Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>);

impl Drop for EventStreamHandles {
    fn drop(&mut self) {
        if let Ok(mut handles) = self.0.lock() {
            for h in handles.drain(..) {
                h.abort();
            }
        }
    }
}

type AssetsClient = taprpc::taproot_assets_client::TaprootAssetsClient<
    InterceptedService<Channel, MacaroonInterceptor>,
>;
type MintClient =
    mintrpc::mint_client::MintClient<InterceptedService<Channel, MacaroonInterceptor>>;
type UniverseClient =
    universerpc::universe_client::UniverseClient<InterceptedService<Channel, MacaroonInterceptor>>;
type TapChannelClient = tapchannelrpc::taproot_asset_channels_client::TaprootAssetChannelsClient<
    InterceptedService<Channel, MacaroonInterceptor>,
>;
type RfqClient = rfqrpc::rfq_client::RfqClient<InterceptedService<Channel, MacaroonInterceptor>>;

#[derive(Clone)]
pub struct TaprootClient {
    assets: AssetsClient,
    mint: MintClient,
    universe: UniverseClient,
    tapchannel: TapChannelClient,
    rfq: RfqClient,
    /// Aborts the live event-subscription tasks when the last clone is dropped.
    events: Arc<EventStreamHandles>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AssetSummary {
    pub asset_id: String,
    pub name: String,
    pub amount: u64,
    pub asset_type: String,
    pub decimal_display: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProofBackup {
    pub asset_id: String,
    pub name: String,
    pub amount: u64,
    pub proof_base64: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AssetBalanceSummary {
    pub asset_id: String,
    pub name: String,
    pub balance: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TransferSummary {
    pub timestamp: i64,
    pub anchor_txid: String,
    pub height_hint: u32,
    pub inputs: usize,
    pub outputs: usize,
    pub total_out: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BatchSummary {
    pub batch_key: String,
    pub batch_txid: String,
    pub state: String,
    pub assets: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AddrReceiveSummary {
    pub timestamp: u64,
    pub addr: String,
    pub status: String,
    pub outpoint: String,
    pub utxo_amt_sat: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AssetMetaSummary {
    pub data: String,
    pub meta_type: String,
    pub meta_hash: String,
    pub decimal_display: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeInfoSummary {
    pub version: String,
    pub lnd_version: String,
    pub network: String,
    pub lnd_pubkey: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DecodedAddrSummary {
    pub encoded: String,
    pub asset_id: String,
    pub asset_type: String,
    pub amount: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BurnSummary {
    pub asset_id: String,
    pub amount: u64,
    pub anchor_txid: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UniverseStatsSummary {
    pub runtime_id: i64,
    pub num_assets: i64,
    pub num_groups: i64,
    pub num_syncs: i64,
    pub num_proofs: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UniverseRootSummary {
    pub asset_id: String,
    pub asset_name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DecodedAssetInvoice {
    pub asset_amount: u64,
    pub sat_amount: i64,
    pub description: String,
    pub destination: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RfqQuotesSummary {
    pub buy_quotes: usize,
    pub sell_quotes: usize,
}

impl TaprootClient {
    pub async fn connect(
        config: TapdConfig,
        tor: Option<TorService>,
        force_tor: bool,
    ) -> Result<Self, String> {
        let host = config.host.trim();
        if host.is_empty() {
            return Err("tapd host is empty".into());
        }

        // Ensure the host has a scheme so we can decide whether to use TLS.
        let normalized = if host.starts_with("http://") || host.starts_with("https://") {
            host.to_string()
        } else {
            format!("https://{host}")
        };

        let parsed =
            url::Url::parse(&normalized).map_err(|e| format!("invalid tapd host URL: {e}"))?;
        let domain = parsed
            .host_str()
            .ok_or_else(|| "tapd host has no domain".to_string())?
            .to_string();

        let is_onion = domain.ends_with(".onion");
        let use_tor = is_onion || force_tor;
        let normalized = parsed.to_string();
        log::info!("taproot connect normalized={normalized} onion={is_onion} use_tor={use_tor}");

        let endpoint = Endpoint::from_shared(normalized.clone())
            .map_err(|e| format!("invalid tapd host: {e}"))?;

        // Build TLS config for HTTPS endpoints. For .onion services reached over Tor we keep
        // HTTPS (the gRPC server expects TLS) and verify against the embedded certificate.
        // The certificate is self-signed for localhost/UNIX, so we use "localhost" as the
        // verification domain; Tor already provides onion service authentication.
        let tls_config = if parsed.scheme() == "https" {
            let domain = if is_onion { "localhost" } else { &domain };
            if config.cert_pem.trim().is_empty() {
                Some(
                    ClientTlsConfig::new()
                        .with_native_roots()
                        .domain_name(domain),
                )
            } else {
                let cert = Certificate::from_pem(config.cert_pem.as_bytes());
                Some(
                    ClientTlsConfig::new()
                        .ca_certificate(cert)
                        .domain_name(domain),
                )
            }
        } else {
            None
        };

        let channel = if use_tor {
            let tor = tor.ok_or_else(|| {
                if is_onion {
                    "Tor is required for .onion tapd hosts".to_string()
                } else {
                    "Tor is enabled but no Tor service was provided".to_string()
                }
            })?;

            // TLS is handled by the connector (with cert pinning) because the litd/tapd
            // cert is self-signed with CA:TRUE and rustls rejects it as a leaf
            // (CaUsedAsEndEntity). Use an http:// URI so tonic does not add its own TLS.
            let server_name = if is_onion {
                "localhost".to_string()
            } else {
                domain.clone()
            };
            let tls = build_tls_client_config(&config.cert_pem, is_onion)?;
            let tor_uri = normalized.replacen("https://", "http://", 1);
            let endpoint =
                Endpoint::from_shared(tor_uri).map_err(|e| format!("invalid tapd host: {e}"))?;
            let connector = ArtiConnector::new(tor, Arc::new(tls), server_name);
            let connect = endpoint.connect_with_connector(connector);
            match tokio::time::timeout(Duration::from_secs(120), connect).await {
                Ok(res) => res.map_err(|e| {
                    let msg = format!("tapd connect via tor: {}", err_chain(&e));
                    log::error!("{msg}");
                    msg
                })?,
                Err(_) => {
                    return Err("tapd connect via tor: timed out after 120s (Tor onion connection too slow; try connecting again)".into());
                }
            }
        } else {
            let endpoint = if let Some(tls_config) = tls_config {
                endpoint
                    .tls_config(tls_config)
                    .map_err(|e| format!("tls config: {e}"))?
            } else {
                endpoint
            };

            match tokio::time::timeout(Duration::from_secs(30), endpoint.connect()).await {
                Ok(res) => res.map_err(|e| format!("tapd connect: {}", err_chain(&e)))?,
                Err(_) => {
                    return Err("tapd connect: timed out after 30s".into());
                }
            }
        };

        // Allow short UI polling bursts (20) but throttle runaway loops to ~10 req/s.
        let limiter = Arc::new(TokenBucket::new(20.0, 10.0));
        let interceptor = MacaroonInterceptor {
            macaroon: Zeroizing::new(config.macaroon_hex),
            limiter,
        };

        let assets = taprpc::taproot_assets_client::TaprootAssetsClient::with_interceptor(
            channel.clone(),
            interceptor.clone(),
        );
        let mint = mintrpc::mint_client::MintClient::with_interceptor(
            channel.clone(),
            interceptor.clone(),
        );
        let universe = universerpc::universe_client::UniverseClient::with_interceptor(
            channel.clone(),
            interceptor.clone(),
        );
        let tapchannel =
            tapchannelrpc::taproot_asset_channels_client::TaprootAssetChannelsClient::with_interceptor(
                channel.clone(),
                interceptor.clone(),
            );
        let rfq = rfqrpc::rfq_client::RfqClient::with_interceptor(channel, interceptor);

        Ok(Self {
            assets,
            mint,
            universe,
            tapchannel,
            rfq,
            events: Arc::new(EventStreamHandles::default()),
        })
    }

    pub async fn list_assets(&mut self) -> Result<Vec<AssetSummary>, String> {
        let req = taprpc::ListAssetRequest {
            with_witness: false,
            include_spent: false,
            include_leased: false,
            ..Default::default()
        };
        let resp = self
            .assets
            .list_assets(req)
            .await
            .map_err(|e| format!("list_assets: {e}"))?;
        let assets = resp
            .into_inner()
            .assets
            .into_iter()
            .filter_map(|a| {
                let genesis = a.asset_genesis.as_ref()?;
                let asset_id = hex::encode(&genesis.asset_id);
                let name = genesis.name.clone();
                let amount = a.amount;
                let decimal_display = a
                    .decimal_display
                    .as_ref()
                    .map(|d| d.decimal_display)
                    .unwrap_or(0);
                let asset_type = format!(
                    "{:?}",
                    taprpc::AssetType::try_from(genesis.asset_type)
                        .unwrap_or(taprpc::AssetType::Normal)
                );
                Some(AssetSummary {
                    asset_id,
                    name,
                    amount,
                    asset_type,
                    decimal_display,
                })
            })
            .collect();
        Ok(assets)
    }

    pub async fn mint_asset(
        &mut self,
        name: &str,
        amount: u64,
        metadata: &str,
        collectible: bool,
        new_group: bool,
        fee_rate_sat_vb: u32,
    ) -> Result<String, String> {
        let meta = taprpc::AssetMeta {
            data: metadata.as_bytes().to_vec(),
            r#type: taprpc::AssetMetaType::MetaTypeOpaque as i32,
            meta_hash: vec![],
        };
        // Collectibles are single-unit by definition.
        let amount = if collectible { 1 } else { amount };
        let asset_type = if collectible {
            taprpc::AssetType::Collectible
        } else {
            taprpc::AssetType::Normal
        };
        let asset = mintrpc::MintAsset {
            asset_version: taprpc::AssetVersion::V0 as i32,
            asset_type: asset_type as i32,
            name: name.to_string(),
            asset_meta: Some(meta),
            amount,
            new_grouped_asset: new_group,
            grouped_asset: false,
            group_key: vec![],
            group_anchor: String::new(),
            ..Default::default()
        };
        let req = mintrpc::MintAssetRequest {
            asset: Some(asset),
            short_response: false,
        };
        self.mint
            .mint_asset(req)
            .await
            .map_err(|e| format!("mint_asset: {e}"))?;

        // tapd fee_rate is sat/kw; ~250 sat/kw per sat/vB. 0 = let tapd choose.
        let finalize_req = mintrpc::FinalizeBatchRequest {
            fee_rate: fee_rate_sat_vb.saturating_mul(250),
            ..Default::default()
        };
        let resp = self
            .mint
            .finalize_batch(finalize_req)
            .await
            .map_err(|e| format!("finalize_batch: {e}"))?;

        let batch_key = resp
            .into_inner()
            .batch
            .map(|b| hex::encode(b.batch_key))
            .unwrap_or_else(|| "finalized".into());
        Ok(batch_key)
    }

    pub async fn new_address(&mut self, asset_id: &str, amount: u64) -> Result<String, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        let req = taprpc::NewAddrRequest {
            asset_id,
            amt: amount,
            ..Default::default()
        };
        let resp = self
            .assets
            .new_addr(req)
            .await
            .map_err(|e| format!("new_addr: {e}"))?;
        Ok(resp.into_inner().encoded)
    }

    pub async fn send_asset(
        &mut self,
        address: &str,
        fee_rate_sat_vb: u32,
    ) -> Result<String, String> {
        let req = taprpc::SendAssetRequest {
            tap_addrs: vec![address.to_string()],
            fee_rate: fee_rate_sat_vb.saturating_mul(250),
            ..Default::default()
        };
        let resp = self
            .assets
            .send_asset(req)
            .await
            .map_err(|e| format!("send_asset: {e}"))?;
        let transfer = resp.into_inner().transfer.ok_or("no transfer returned")?;
        Ok(hex::encode(transfer.anchor_tx_hash))
    }

    pub async fn export_proofs(&mut self) -> Result<Vec<ProofBackup>, String> {
        let req = taprpc::ListAssetRequest {
            with_witness: false,
            include_spent: false,
            include_leased: false,
            ..Default::default()
        };
        let resp = self
            .assets
            .list_assets(req)
            .await
            .map_err(|e| format!("list_assets: {e}"))?;

        let mut backups = Vec::new();
        for asset in resp.into_inner().assets {
            let genesis = asset
                .asset_genesis
                .as_ref()
                .ok_or("asset missing genesis")?;
            let anchor = asset.chain_anchor.as_ref().ok_or("asset missing anchor")?;
            let outpoint = parse_outpoint(&anchor.anchor_outpoint)?;

            let export_req = taprpc::ExportProofRequest {
                asset_id: genesis.asset_id.clone(),
                script_key: asset.script_key.clone(),
                outpoint: Some(outpoint),
            };
            let proof_resp = self
                .assets
                .export_proof(export_req)
                .await
                .map_err(|e| format!("export_proof: {e}"))?;
            let proof = proof_resp.into_inner();

            backups.push(ProofBackup {
                asset_id: hex::encode(&genesis.asset_id),
                name: genesis.name.clone(),
                amount: asset.amount,
                proof_base64: BASE64.encode(&proof.raw_proof_file),
            });
        }
        Ok(backups)
    }

    pub async fn verify_proof(&mut self, proof_base64: &str) -> Result<bool, String> {
        let raw_proof_file = BASE64
            .decode(proof_base64)
            .map_err(|e| format!("invalid base64: {e}"))?;
        let req = taprpc::ProofFile {
            raw_proof_file,
            genesis_point: String::new(),
        };
        let resp = self
            .assets
            .verify_proof(req)
            .await
            .map_err(|e| format!("verify_proof: {e}"))?;
        Ok(resp.into_inner().valid)
    }

    pub async fn list_balances(&mut self) -> Result<Vec<AssetBalanceSummary>, String> {
        let req = taprpc::ListBalancesRequest {
            group_by: Some(taprpc::list_balances_request::GroupBy::AssetId(true)),
            ..Default::default()
        };
        let resp = self
            .assets
            .list_balances(req)
            .await
            .map_err(|e| format!("list_balances: {e}"))?;
        let balances = resp
            .into_inner()
            .asset_balances
            .into_values()
            .map(|b| {
                let (asset_id, name) = b
                    .asset_genesis
                    .as_ref()
                    .map(|g| (hex::encode(&g.asset_id), g.name.clone()))
                    .unwrap_or_default();
                AssetBalanceSummary {
                    asset_id,
                    name,
                    balance: b.balance,
                }
            })
            .collect();
        Ok(balances)
    }

    pub async fn list_transfers(&mut self) -> Result<Vec<TransferSummary>, String> {
        let resp = self
            .assets
            .list_transfers(taprpc::ListTransfersRequest::default())
            .await
            .map_err(|e| format!("list_transfers: {e}"))?;
        let transfers = resp
            .into_inner()
            .transfers
            .into_iter()
            .map(|tr| {
                let mut txid = tr.anchor_tx_hash.clone();
                txid.reverse();
                let total_out: u64 = tr.outputs.iter().map(|o| o.amount).sum();
                TransferSummary {
                    timestamp: tr.transfer_timestamp,
                    anchor_txid: hex::encode(txid),
                    height_hint: tr.anchor_tx_height_hint,
                    inputs: tr.inputs.len(),
                    outputs: tr.outputs.len(),
                    total_out,
                }
            })
            .collect();
        Ok(transfers)
    }

    pub async fn list_batches(&mut self) -> Result<Vec<BatchSummary>, String> {
        let resp = self
            .mint
            .list_batches(mintrpc::ListBatchRequest::default())
            .await
            .map_err(|e| format!("list_batches: {e}"))?;
        let batches = resp
            .into_inner()
            .batches
            .into_iter()
            .filter_map(|vb| {
                let b = vb.batch?;
                let state = format!(
                    "{:?}",
                    mintrpc::BatchState::try_from(b.state).unwrap_or(mintrpc::BatchState::Unknown)
                );
                Some(BatchSummary {
                    batch_key: hex::encode(&b.batch_key),
                    batch_txid: b.batch_txid.clone(),
                    state,
                    assets: vb.unsealed_assets.len(),
                })
            })
            .collect();
        Ok(batches)
    }

    pub async fn cancel_batch(&mut self) -> Result<String, String> {
        let resp = self
            .mint
            .cancel_batch(mintrpc::CancelBatchRequest::default())
            .await
            .map_err(|e| format!("cancel_batch: {e}"))?;
        Ok(hex::encode(resp.into_inner().batch_key))
    }

    pub async fn list_burns(&mut self) -> Result<Vec<BurnSummary>, String> {
        let resp = self
            .assets
            .list_burns(taprpc::ListBurnsRequest::default())
            .await
            .map_err(|e| format!("list_burns: {e}"))?;
        let burns = resp
            .into_inner()
            .burns
            .into_iter()
            .map(|b| {
                let mut txid = b.anchor_txid;
                txid.reverse();
                BurnSummary {
                    asset_id: hex::encode(b.asset_id),
                    amount: b.amount,
                    anchor_txid: hex::encode(txid),
                }
            })
            .collect();
        Ok(burns)
    }

    pub async fn addr_receives(&mut self) -> Result<Vec<AddrReceiveSummary>, String> {
        let resp = self
            .assets
            .addr_receives(taprpc::AddrReceivesRequest::default())
            .await
            .map_err(|e| format!("addr_receives: {e}"))?;
        let events = resp
            .into_inner()
            .events
            .into_iter()
            .map(|e| {
                let addr = e
                    .addr
                    .as_ref()
                    .map(|a| a.encoded.clone())
                    .unwrap_or_default();
                let status = format!(
                    "{:?}",
                    taprpc::AddrEventStatus::try_from(e.status)
                        .unwrap_or(taprpc::AddrEventStatus::TransactionDetected)
                );
                AddrReceiveSummary {
                    timestamp: e.creation_time_unix_seconds,
                    addr,
                    status,
                    outpoint: e.outpoint,
                    utxo_amt_sat: e.utxo_amt_sat,
                }
            })
            .collect();
        Ok(events)
    }

    pub async fn fetch_asset_meta(&mut self, asset_id: &str) -> Result<AssetMetaSummary, String> {
        let req = taprpc::FetchAssetMetaRequest {
            asset: Some(taprpc::fetch_asset_meta_request::Asset::AssetIdStr(
                asset_id.to_string(),
            )),
        };
        let resp = self
            .assets
            .fetch_asset_meta(req)
            .await
            .map_err(|e| format!("fetch_asset_meta: {e}"))?;
        let m = resp.into_inner();
        let meta_type = format!(
            "{:?}",
            taprpc::AssetMetaType::try_from(m.r#type)
                .unwrap_or(taprpc::AssetMetaType::MetaTypeOpaque)
        );
        Ok(AssetMetaSummary {
            data: String::from_utf8_lossy(&m.data).to_string(),
            meta_type,
            meta_hash: hex::encode(m.meta_hash),
            decimal_display: m.decimal_display,
        })
    }

    pub async fn get_info(&mut self) -> Result<NodeInfoSummary, String> {
        let resp = self
            .assets
            .get_info(taprpc::GetInfoRequest {})
            .await
            .map_err(|e| format!("get_info: {e}"))?;
        let i = resp.into_inner();
        Ok(NodeInfoSummary {
            version: i.version,
            lnd_version: i.lnd_version,
            network: i.network,
            lnd_pubkey: hex::encode(i.lnd_identity_pubkey),
        })
    }

    pub async fn decode_addr(&mut self, addr: &str) -> Result<DecodedAddrSummary, String> {
        let req = taprpc::DecodeAddrRequest {
            addr: addr.to_string(),
        };
        let resp = self
            .assets
            .decode_addr(req)
            .await
            .map_err(|e| format!("decode_addr: {e}"))?;
        let a = resp.into_inner();
        let asset_type = format!(
            "{:?}",
            taprpc::AssetType::try_from(a.asset_type).unwrap_or(taprpc::AssetType::Normal)
        );
        Ok(DecodedAddrSummary {
            encoded: a.encoded,
            asset_id: hex::encode(a.asset_id),
            asset_type,
            amount: a.amount,
        })
    }

    pub async fn burn_asset(&mut self, asset_id: &str, amount: u64) -> Result<String, String> {
        let req = taprpc::BurnAssetRequest {
            amount_to_burn: amount,
            confirmation_text: "assets will be destroyed".to_string(),
            asset: Some(taprpc::burn_asset_request::Asset::AssetIdStr(
                asset_id.to_string(),
            )),
            ..Default::default()
        };
        let resp = self
            .assets
            .burn_asset(req)
            .await
            .map_err(|e| format!("burn_asset: {e}"))?;
        let txid = resp
            .into_inner()
            .burn_transfer
            .map(|t| {
                let mut h = t.anchor_tx_hash;
                h.reverse();
                hex::encode(h)
            })
            .unwrap_or_default();
        Ok(txid)
    }

    /// Spawn background tasks that stream tapd receive/send/mint events and emit a
    /// Tauri `tapd-event` on each, so the UI can refresh live. Clones the gRPC
    /// clients (cheap channel clones); tasks end when a stream closes or errors.
    pub fn spawn_event_streams(&self, app: AppHandle) {
        let mut handles = self.events.0.lock().unwrap_or_else(|p| p.into_inner());

        let mut recv = self.assets.clone();
        let app_recv = app.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            if let Ok(stream) = recv
                .subscribe_receive_events(taprpc::SubscribeReceiveEventsRequest::default())
                .await
            {
                let mut s = stream.into_inner();
                while let Ok(Some(_)) = s.message().await {
                    let _ = app_recv.emit("tapd-event", "receive");
                }
            }
        }));

        let mut send = self.assets.clone();
        let app_send = app.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            if let Ok(stream) = send
                .subscribe_send_events(taprpc::SubscribeSendEventsRequest::default())
                .await
            {
                let mut s = stream.into_inner();
                while let Ok(Some(_)) = s.message().await {
                    let _ = app_send.emit("tapd-event", "send");
                }
            }
        }));

        let mut mint = self.mint.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            if let Ok(stream) = mint
                .subscribe_mint_events(mintrpc::SubscribeMintEventsRequest::default())
                .await
            {
                let mut s = stream.into_inner();
                while let Ok(Some(_)) = s.message().await {
                    let _ = app.emit("tapd-event", "mint");
                }
            }
        }));
    }

    pub async fn universe_stats(&mut self) -> Result<UniverseStatsSummary, String> {
        let info = self
            .universe
            .info(universerpc::InfoRequest {})
            .await
            .map_err(|e| format!("universe info: {e}"))?
            .into_inner();
        let stats = self
            .universe
            .universe_stats(universerpc::StatsRequest {})
            .await
            .map_err(|e| format!("universe stats: {e}"))?
            .into_inner();
        Ok(UniverseStatsSummary {
            runtime_id: info.runtime_id,
            num_assets: stats.num_total_assets,
            num_groups: stats.num_total_groups,
            num_syncs: stats.num_total_syncs,
            num_proofs: stats.num_total_proofs,
        })
    }

    pub async fn universe_roots(&mut self) -> Result<Vec<UniverseRootSummary>, String> {
        let req = universerpc::AssetRootRequest {
            with_amounts_by_id: false,
            offset: 0,
            limit: 100,
            ..Default::default()
        };
        let resp = self
            .universe
            .asset_roots(req)
            .await
            .map_err(|e| format!("asset_roots: {e}"))?
            .into_inner();
        let roots = resp
            .universe_roots
            .into_iter()
            .map(|(asset_id, v)| UniverseRootSummary {
                asset_id,
                asset_name: v.asset_name,
            })
            .collect();
        Ok(roots)
    }

    pub async fn universe_sync(&mut self, host: &str) -> Result<usize, String> {
        let req = universerpc::SyncRequest {
            universe_host: host.to_string(),
            sync_mode: universerpc::UniverseSyncMode::SyncIssuanceOnly as i32,
            sync_targets: vec![],
        };
        let resp = self
            .universe
            .sync_universe(req)
            .await
            .map_err(|e| format!("sync_universe: {e}"))?
            .into_inner();
        Ok(resp.synced_universes.len())
    }

    /// List the universe federation servers this node auto-syncs with.
    pub async fn list_federation_servers(&mut self) -> Result<Vec<String>, String> {
        let resp = self
            .universe
            .list_federation_servers(universerpc::ListFederationServersRequest {})
            .await
            .map_err(|e| format!("list federation: {e}"))?
            .into_inner();
        Ok(resp.servers.into_iter().map(|s| s.host).collect())
    }

    /// Add a universe server to the federation (node then periodically syncs it).
    pub async fn add_federation_server(&mut self, host: &str) -> Result<(), String> {
        self.universe
            .add_federation_server(universerpc::AddFederationServerRequest {
                servers: vec![universerpc::UniverseFederationServer {
                    host: host.to_string(),
                    id: 0,
                }],
            })
            .await
            .map_err(|e| format!("add federation: {e}"))?;
        Ok(())
    }

    /// Remove a universe server from the federation.
    pub async fn delete_federation_server(&mut self, host: &str) -> Result<(), String> {
        self.universe
            .delete_federation_server(universerpc::DeleteFederationServerRequest {
                servers: vec![universerpc::UniverseFederationServer {
                    host: host.to_string(),
                    id: 0,
                }],
            })
            .await
            .map_err(|e| format!("delete federation: {e}"))?;
        Ok(())
    }

    // ---- Lightning assets (litd: tapchannel + rfq) ----

    pub async fn decode_asset_invoice(
        &mut self,
        pay_req: &str,
        asset_id: &str,
    ) -> Result<DecodedAssetInvoice, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        let req = tapchannelrpc::AssetPayReq {
            asset_id,
            pay_req_string: pay_req.to_string(),
            ..Default::default()
        };
        let resp = self
            .tapchannel
            .decode_asset_pay_req(req)
            .await
            .map_err(|e| format!("decode_asset_pay_req: {e}"))?
            .into_inner();
        let (sat_amount, description, destination) = resp
            .pay_req
            .map(|p| (p.num_satoshis, p.description, p.destination))
            .unwrap_or_default();
        Ok(DecodedAssetInvoice {
            asset_amount: resp.asset_amount,
            sat_amount,
            description,
            destination,
        })
    }

    pub async fn list_rfq_quotes(&mut self) -> Result<RfqQuotesSummary, String> {
        let resp = self
            .rfq
            .query_peer_accepted_quotes(rfqrpc::QueryPeerAcceptedQuotesRequest::default())
            .await
            .map_err(|e| format!("query_peer_accepted_quotes: {e}"))?
            .into_inner();
        Ok(RfqQuotesSummary {
            buy_quotes: resp.buy_quotes.len(),
            sell_quotes: resp.sell_quotes.len(),
        })
    }

    pub async fn create_asset_invoice(
        &mut self,
        asset_id: &str,
        asset_amount: u64,
        peer_pubkey: &str,
        memo: &str,
    ) -> Result<String, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        // Empty peer => let litd auto-select the asset channel (single-channel case),
        // mirroring pay_asset_invoice. Avoids forcing the user to paste a peer pubkey.
        let peer_pubkey = if peer_pubkey.trim().is_empty() {
            vec![]
        } else {
            hex::decode(peer_pubkey).map_err(|e| format!("invalid peer pubkey: {e}"))?
        };
        let invoice = lnrpc::Invoice {
            memo: memo.to_string(),
            ..Default::default()
        };
        let req = tapchannelrpc::AddInvoiceRequest {
            asset_id,
            asset_amount,
            peer_pubkey,
            invoice_request: Some(invoice),
            ..Default::default()
        };
        let resp = self
            .tapchannel
            .add_invoice(req)
            .await
            .map_err(|e| format!("add_invoice: {e}"))?
            .into_inner();
        Ok(resp
            .invoice_result
            .map(|r| r.payment_request)
            .unwrap_or_default())
    }

    pub async fn fund_asset_channel(
        &mut self,
        asset_id: &str,
        asset_amount: u64,
        peer_pubkey: &str,
        fee_rate_sat_vb: u32,
    ) -> Result<String, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        let peer_pubkey =
            hex::decode(peer_pubkey).map_err(|e| format!("invalid peer pubkey: {e}"))?;
        let req = tapchannelrpc::FundChannelRequest {
            asset_amount,
            asset_id,
            peer_pubkey,
            fee_rate_sat_per_vbyte: fee_rate_sat_vb,
            ..Default::default()
        };
        let resp = self
            .tapchannel
            .fund_channel(req)
            .await
            .map_err(|e| format!("fund_channel: {e}"))?
            .into_inner();
        Ok(format!("{}:{}", resp.txid, resp.output_index))
    }

    pub async fn pay_asset_invoice(
        &mut self,
        pay_req: &str,
        asset_id: &str,
        peer_pubkey: &str,
    ) -> Result<String, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        let peer_pubkey = if peer_pubkey.trim().is_empty() {
            vec![]
        } else {
            hex::decode(peer_pubkey).map_err(|e| format!("invalid peer pubkey: {e}"))?
        };
        let inner = routerrpc::SendPaymentRequest {
            payment_request: pay_req.to_string(),
            timeout_seconds: 60,
            ..Default::default()
        };
        let req = tapchannelrpc::SendPaymentRequest {
            asset_id,
            peer_pubkey,
            payment_request: Some(inner),
            ..Default::default()
        };
        let mut stream = self
            .tapchannel
            .send_payment(req)
            .await
            .map_err(|e| format!("send_payment: {e}"))?
            .into_inner();
        let succeeded = lnrpc::payment::PaymentStatus::Succeeded as i32;
        let failed = lnrpc::payment::PaymentStatus::Failed as i32;
        let mut status = "pending".to_string();
        while let Ok(Some(resp)) = stream.message().await {
            if let Some(tapchannelrpc::send_payment_response::Result::PaymentResult(p)) =
                resp.result
            {
                status = format!(
                    "{:?}",
                    lnrpc::payment::PaymentStatus::try_from(p.status)
                        .unwrap_or(lnrpc::payment::PaymentStatus::Unknown)
                );
                if p.status == succeeded || p.status == failed {
                    break;
                }
            }
        }
        Ok(status)
    }
}

/// Format an error plus its full `source()` chain on one line, so surfaced
/// messages reveal the real cause behind tonic's generic "transport error".
fn err_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(" -> ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

/// A rustls verifier that pins exactly one certificate (by DER bytes). Needed
/// because litd/tapd self-signed certs are marked CA:TRUE and rustls' webpki
/// verifier rejects such a cert when presented as the server leaf. We are
/// connecting to a fixed .onion with a known cert, so pinning is the right model.
#[derive(Debug)]
struct PinnedCertVerifier {
    /// `Some(der)` pins the exact certificate; `None` accepts any certificate —
    /// used for .onion, where the v3 onion address already authenticates the
    /// endpoint (Tor rendezvous) so pinning is redundant and fragile.
    pinned: Option<Vec<u8>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match &self.pinned {
            // Onion: Tor already authenticates the endpoint; accept any cert.
            None => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            Some(p) if end_entity.as_ref() == p.as_slice() => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::General(
                "tapd server certificate does not match the pinned certificate".into(),
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Decode a single PEM certificate into DER bytes.
fn parse_cert_der(pem: &str) -> Result<Vec<u8>, String> {
    if pem.trim().is_empty() {
        return Err("tapd certificate is required to connect over Tor".into());
    }
    let b64: String = pem
        .lines()
        .filter(|l| !l.contains("CERTIFICATE"))
        .map(|l| l.trim())
        .collect();
    BASE64
        .decode(b64)
        .map_err(|e| format!("decode tapd certificate: {e}"))
}

/// Build a rustls client config that pins the given tapd certificate and offers
/// ALPN h2 (gRPC). Uses the ring provider to match arti-client.
fn build_tls_client_config(cert_pem: &str, is_onion: bool) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // For .onion the v3 onion address itself authenticates the endpoint (Tor
    // rendezvous), so pinning the self-signed cert is redundant AND fragile — it
    // would break every user the moment litd regenerates its cert (update/expiry).
    // Accept any cert over Tor (still TLS-encrypted) and rely on Tor for
    // authentication; pin on clearnet where there is no such guarantee.
    let pinned = if is_onion {
        None
    } else {
        Some(parse_cert_der(cert_pem)?)
    };
    let verifier = Arc::new(PinnedCertVerifier {
        pinned,
        provider: provider.clone(),
    });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("rustls protocol versions: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(config)
}

fn parse_outpoint(s: &str) -> Result<taprpc::OutPoint, String> {
    let (txid_hex, vout_str) = s
        .split_once(':')
        .ok_or_else(|| format!("invalid outpoint: {s}"))?;
    let mut txid = hex::decode(txid_hex).map_err(|e| format!("invalid txid: {e}"))?;
    if txid.len() != 32 {
        return Err(format!("invalid txid length: {}", txid.len()));
    }
    // RPC expects internal little-endian byte order.
    txid.reverse();
    let output_index = vout_str
        .parse::<u32>()
        .map_err(|e| format!("invalid vout: {e}"))?;
    Ok(taprpc::OutPoint { txid, output_index })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_outpoint_ok() {
        let txid_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let out = parse_outpoint(&format!("{txid_hex}:1")).unwrap();
        assert_eq!(out.output_index, 1);
        let mut expected = hex::decode(txid_hex).unwrap();
        expected.reverse();
        assert_eq!(out.txid, expected);
    }

    #[test]
    fn test_parse_outpoint_invalid_format() {
        assert!(parse_outpoint("nocolon").is_err());
    }

    #[test]
    fn test_parse_outpoint_invalid_hex() {
        assert!(parse_outpoint("nothex:0").is_err());
    }

    #[test]
    fn test_parse_outpoint_invalid_length() {
        assert!(parse_outpoint("deadbeef:0").is_err());
    }
}

fn config_path(app_handle: &AppHandle) -> Result<PathBuf, String> {
    app_handle
        .path()
        .app_local_data_dir()
        .map(|p| p.join("tapd-config.json"))
        .map_err(|e| e.to_string())
}

pub fn save_tapd_config(app_handle: &AppHandle, config: &TapdConfig) -> Result<(), String> {
    let path = config_path(app_handle)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Never write the macaroon to plaintext JSON; it is stored in Stronghold.
    let config_to_save = TapdConfig {
        macaroon_hex: String::new(),
        ..config.clone()
    };
    let json = serde_json::to_string_pretty(&config_to_save).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn load_tapd_config(app_handle: &AppHandle) -> Result<Option<TapdConfig>, String> {
    let path = config_path(app_handle)?;
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let config = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    Ok(Some(config))
}

/// Auto-connect tapd at unlock, in the background. Uses the user's saved config
/// (macaroon decrypted from Stronghold with the unlock password) if present, and
/// otherwise falls back to the embedded default Umbrel node. Takes owned handles
/// so it can be spawned without blocking unlock on the slow Tor onion connect.
pub async fn reconnect_tapd_bg(
    app_handle: AppHandle,
    tor: std::sync::Arc<tokio::sync::Mutex<crate::tor::TorService>>,
    taproot: std::sync::Arc<tokio::sync::Mutex<Option<TaprootClient>>>,
    password: String,
) -> Result<bool, String> {
    // Don't override an already-established connection.
    if taproot.lock().await.is_some() {
        return Ok(true);
    }

    let config = match load_tapd_config(&app_handle)? {
        Some(mut c) => {
            c.macaroon_hex = crate::wallet::load_secret(&app_handle, &password, TAPD_MACAROON_KEY)
                .map_err(|e| e.to_string())?;
            c
        }
        None => {
            let d = crate::tapd_defaults::TapdDefaults::load();
            if d.is_empty() {
                return Ok(false);
            }
            TapdConfig {
                host: d.host,
                cert_pem: d.cert_pem,
                macaroon_hex: d.macaroon_hex,
            }
        }
    };

    // Robust .onion detection: the host may carry a scheme and/or the :10029 port,
    // which a naive `ends_with(".onion")` misses → Tor never starts → connect fails.
    // This is what made auto-connect-on-unlock silently do nothing for the default
    // node. Parse it like `connect()` does and test only the host component.
    let is_onion = {
        let h = config.host.trim();
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
    let tor_config = crate::tor::TorConfig::load(&app_handle).unwrap_or_default();
    // Always use Tor for .onion hosts (consistent with connect_tapd).
    let use_tor = is_onion || tor_config.force_tor || tor_config.enabled;

    let tor_service = if use_tor {
        let t = tor.lock().await.clone();
        t.start().await?;
        Some(t)
    } else {
        None
    };

    let client = TaprootClient::connect(config, tor_service, use_tor).await?;
    client.spawn_event_streams(app_handle.clone());
    *taproot.lock().await = Some(client);
    Ok(true)
}

/// Background supervisor: every 30s, health-check the tapd connection with a cheap
/// `get_info` RPC and transparently reconnect if the Tor circuit / gRPC stream has
/// dropped. Makes the wallet self-heal mid-session instead of appearing
/// disconnected until the user reopens the screen; also retries the initial
/// connect if the first (often flaky) onion attempt failed. Spawned once at unlock.
pub async fn supervise_tapd(
    app_handle: AppHandle,
    tor: std::sync::Arc<tokio::sync::Mutex<crate::tor::TorService>>,
    taproot: std::sync::Arc<tokio::sync::Mutex<Option<TaprootClient>>>,
    password: String,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        // Snapshot (cheap gRPC channel clone) so the health-check RPC doesn't hold
        // the lock and block UI-triggered tapd calls.
        let snapshot = { taproot.lock().await.clone() };
        let mut healthy = false;
        if let Some(mut client) = snapshot {
            healthy = client.get_info().await.is_ok();
            if !healthy {
                // Stream is dead — drop it so the reconnect below re-establishes.
                *taproot.lock().await = None;
                let _ = app_handle.emit("tapd-status", "reconnecting");
            }
        }

        if !healthy {
            if let Ok(true) = reconnect_tapd_bg(
                app_handle.clone(),
                tor.clone(),
                taproot.clone(),
                password.clone(),
            )
            .await
            {
                let _ = app_handle.emit("tapd-status", "connected");
            }
        }
    }
}
