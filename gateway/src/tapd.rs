//! Minimal tapd gRPC client for the gateway's read endpoints.
//!
//! Connects to a **local** litd/tapd over TLS with the certificate pinned (the
//! cert is self-signed with CA:TRUE, which rustls' webpki rejects as a leaf, so we
//! verify it by exact match instead — same approach as the wallet app). The
//! macaroon is read from a local file by the caller and injected on every request.
//! This is the only component in the system that holds the macaroon.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use http::Uri;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, TlsConnector};
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use zeroize::Zeroizing;

#[allow(clippy::all, dead_code)]
pub mod taprpc {
    tonic::include_proto!("taprpc");
}

#[allow(clippy::all, dead_code)]
pub mod universerpc {
    tonic::include_proto!("universerpc");
}

#[allow(clippy::all, dead_code)]
pub mod mintrpc {
    tonic::include_proto!("mintrpc");
}

// Lightning-asset stack (litd): tapchannel + rfq, plus their lnd proto deps.
// Declared as siblings so the generated cross-package `super::` references
// (e.g. tapchannelrpc -> lnrpc / rfqrpc / routerrpc / taprpc) resolve.
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

type AssetsClient =
    taprpc::taproot_assets_client::TaprootAssetsClient<InterceptedService<Channel, Macaroon>>;
type UniverseClient =
    universerpc::universe_client::UniverseClient<InterceptedService<Channel, Macaroon>>;
type MintClient = mintrpc::mint_client::MintClient<InterceptedService<Channel, Macaroon>>;
type TapChannelClient = tapchannelrpc::taproot_asset_channels_client::TaprootAssetChannelsClient<
    InterceptedService<Channel, Macaroon>,
>;
type RfqClient = rfqrpc::rfq_client::RfqClient<InterceptedService<Channel, Macaroon>>;
type LndClient = lnrpc::lightning_client::LightningClient<InterceptedService<Channel, Macaroon>>;

/// Injects the tapd macaroon into every gRPC request's metadata.
#[derive(Clone)]
pub struct Macaroon {
    hex: Zeroizing<String>,
}

impl Interceptor for Macaroon {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let value = self
            .hex
            .as_str()
            .parse()
            .map_err(|_| Status::invalid_argument("invalid macaroon"))?;
        req.metadata_mut().insert("macaroon", value);
        Ok(req)
    }
}

#[derive(Clone)]
pub struct TapdClient {
    assets: AssetsClient,
    universe: UniverseClient,
    mint: MintClient,
    tapchannel: TapChannelClient,
    rfq: RfqClient,
    /// lnd's own `Lightning` service, used only for `LookupInvoice` to detect
    /// LN-asset receive settlement. Authorized by a separate lnd macaroon when one
    /// is provided (the tapd macaroon does not cover lnd RPCs); see `connect`.
    lightning: LndClient,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetInfo {
    pub asset_id: String,
    pub name: String,
    pub amount: u64,
    pub asset_type: String,
    pub decimal_display: u32,
    /// Hex txid of the transaction anchoring this asset. For a freshly minted
    /// asset this equals its mint batch's txid — the link used to attribute
    /// ownership at reconciliation time.
    pub anchor_txid: String,
}

/// Outcome of a mint: the batch identifiers used to later resolve the asset id.
#[derive(Debug, Clone, Serialize)]
pub struct MintOutcome {
    pub batch_key: String,
    pub batch_txid: String,
}

/// A minting batch's identifiers (used to resolve a pending mint's txid).
#[derive(Debug, Clone)]
pub struct BatchRef {
    pub batch_key: String,
    pub batch_txid: String,
}

/// An incoming-transfer event for one of our receive addresses.
#[derive(Debug, Clone)]
pub struct AddrReceiveEvent {
    pub addr: String,
    /// True once the transfer is fully received & confirmed (safe to credit).
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UniverseStats {
    pub num_assets: i64,
    pub num_groups: i64,
    pub num_syncs: i64,
    pub num_proofs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UniverseRoot {
    pub asset_id: String,
    pub asset_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedAddr {
    pub encoded: String,
    pub asset_id: String,
    pub asset_type: String,
    pub amount: u64,
}

/// Public metadata for one asset (safe to expose: it is the asset's own genesis
/// metadata, not any user's holdings).
#[derive(Debug, Clone, Serialize)]
pub struct AssetMeta {
    /// UTF-8 lossy view of the raw meta blob (issuer-provided).
    pub data: String,
    pub meta_type: String,
    pub meta_hash: String,
    pub decimal_display: u32,
}

/// Non-sensitive node information (version + network), for a status panel.
#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub version: String,
    pub lnd_version: String,
    pub network: String,
}

/// A decoded Lightning asset invoice (RFQ-priced): the asset units it settles and
/// the sat equivalent, plus the human description.
#[derive(Debug, Clone, Serialize)]
pub struct DecodedAssetInvoice {
    pub asset_amount: u64,
    pub sat_amount: i64,
    pub description: String,
    pub destination: String,
}

/// Count of the node's currently-accepted RFQ quotes (buy/sell), a cheap health
/// signal for whether asset-channel routing is available.
#[derive(Debug, Clone, Serialize)]
pub struct RfqQuotes {
    pub buy_quotes: usize,
    pub sell_quotes: usize,
}

/// A freshly-created Lightning **asset** invoice: the BOLT11 payment request to
/// hand to the payer, plus the hex payment hash used to detect settlement later.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedAssetInvoice {
    pub payment_request: String,
    pub r_hash: String,
}

impl TapdClient {
    /// Connect to a local tapd. `host` is `host:port`; `cert_pem` is tapd's TLS
    /// certificate (PEM); `macaroon_hex` authorizes the tapd/litd gRPC calls.
    /// `lnd_macaroon_hex`, when present, authorizes the separate lnd `Lightning`
    /// service (`LookupInvoice`) used to detect LN-asset receive settlement — the
    /// tapd macaroon does not cover lnd RPCs. When absent, LN receive still issues
    /// invoices but settlement can't be observed, so auto-credit is disabled.
    pub async fn connect(
        host: &str,
        cert_pem: &str,
        macaroon_hex: &str,
        lnd_macaroon_hex: Option<&str>,
    ) -> Result<Self, String> {
        let host = host.trim();
        if host.is_empty() {
            return Err("tapd host is empty".into());
        }
        // TLS is handled by our connector (cert pinning) so the endpoint speaks
        // http:// — tonic must not add its own TLS on top.
        let endpoint = Endpoint::from_shared(format!("http://{host}"))
            .map_err(|e| format!("invalid tapd host: {e}"))?;

        let tls = Arc::new(build_pinned_tls(cert_pem)?);
        let connector = LocalTlsConnector {
            tls,
            // The pinned verifier ignores the server name; "localhost" is a
            // placeholder that must still parse as a valid SNI.
            server_name: "localhost".to_string(),
        };
        let channel = endpoint
            .connect_with_connector(connector)
            .await
            .map_err(|e| format!("tapd connect: {e}"))?;

        let interceptor = Macaroon {
            hex: Zeroizing::new(macaroon_hex.to_string()),
        };
        let assets = taprpc::taproot_assets_client::TaprootAssetsClient::with_interceptor(
            channel.clone(),
            interceptor.clone(),
        );
        let universe = universerpc::universe_client::UniverseClient::with_interceptor(
            channel.clone(),
            interceptor.clone(),
        );
        let mint = mintrpc::mint_client::MintClient::with_interceptor(
            channel.clone(),
            interceptor.clone(),
        );
        let tapchannel =
            tapchannelrpc::taproot_asset_channels_client::TaprootAssetChannelsClient::with_interceptor(
                channel.clone(),
                interceptor.clone(),
            );
        // lnd's Lightning service needs an lnd macaroon; fall back to the tapd
        // macaroon when none is provided (LookupInvoice then fails 'permission
        // denied', which the reconciler logs — invoices still issue, auto-credit
        // is simply off).
        let lnd_interceptor = Macaroon {
            hex: Zeroizing::new(lnd_macaroon_hex.unwrap_or(macaroon_hex).to_string()),
        };
        let lightning = lnrpc::lightning_client::LightningClient::with_interceptor(
            channel.clone(),
            lnd_interceptor,
        );
        let rfq = rfqrpc::rfq_client::RfqClient::with_interceptor(channel, interceptor);
        Ok(Self {
            assets,
            universe,
            mint,
            tapchannel,
            rfq,
            lightning,
        })
    }

    /// All assets known to tapd. The gateway filters this by the ownership
    /// registry before returning anything to a caller.
    pub async fn list_assets(&mut self) -> Result<Vec<AssetInfo>, String> {
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
            .map_err(|e| format!("list_assets: {e}"))?
            .into_inner();
        let out = resp
            .assets
            .into_iter()
            .filter_map(|a| {
                let g = a.asset_genesis.as_ref()?;
                let asset_type = format!(
                    "{:?}",
                    taprpc::AssetType::try_from(g.asset_type).unwrap_or(taprpc::AssetType::Normal)
                );
                Some(AssetInfo {
                    asset_id: hex::encode(&g.asset_id),
                    name: g.name.clone(),
                    amount: a.amount,
                    asset_type,
                    decimal_display: a
                        .decimal_display
                        .as_ref()
                        .map(|d| d.decimal_display)
                        .unwrap_or(0),
                    // AnchorInfo exposes the anchor as `anchor_outpoint` ("txid:vout").
                    // For a freshly minted asset the anchor is the mint tx, so this
                    // txid equals the mint batch's txid — the reconciliation link.
                    anchor_txid: a
                        .chain_anchor
                        .as_ref()
                        .and_then(|c| c.anchor_outpoint.split(':').next())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect();
        Ok(out)
    }

    pub async fn universe_stats(&mut self) -> Result<UniverseStats, String> {
        let stats = self
            .universe
            .universe_stats(universerpc::StatsRequest {})
            .await
            .map_err(|e| format!("universe stats: {e}"))?
            .into_inner();
        Ok(UniverseStats {
            num_assets: stats.num_total_assets,
            num_groups: stats.num_total_groups,
            num_syncs: stats.num_total_syncs,
            num_proofs: stats.num_total_proofs,
        })
    }

    pub async fn universe_roots(&mut self) -> Result<Vec<UniverseRoot>, String> {
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
            .map(|(asset_id, v)| UniverseRoot {
                asset_id,
                asset_name: v.asset_name,
            })
            .collect();
        Ok(roots)
    }

    pub async fn decode_addr(&mut self, addr: &str) -> Result<DecodedAddr, String> {
        let req = taprpc::DecodeAddrRequest {
            addr: addr.to_string(),
        };
        let a = self
            .assets
            .decode_addr(req)
            .await
            .map_err(|e| format!("decode_addr: {e}"))?
            .into_inner();
        let asset_type = format!(
            "{:?}",
            taprpc::AssetType::try_from(a.asset_type).unwrap_or(taprpc::AssetType::Normal)
        );
        Ok(DecodedAddr {
            encoded: a.encoded,
            asset_id: hex::encode(a.asset_id),
            asset_type,
            amount: a.amount,
        })
    }

    /// Public metadata for a single asset, keyed by its hex asset id.
    pub async fn fetch_asset_meta(&mut self, asset_id: &str) -> Result<AssetMeta, String> {
        let req = taprpc::FetchAssetMetaRequest {
            asset: Some(taprpc::fetch_asset_meta_request::Asset::AssetIdStr(
                asset_id.to_string(),
            )),
        };
        let m = self
            .assets
            .fetch_asset_meta(req)
            .await
            .map_err(|e| format!("fetch_asset_meta: {e}"))?
            .into_inner();
        let meta_type = format!(
            "{:?}",
            taprpc::AssetMetaType::try_from(m.r#type)
                .unwrap_or(taprpc::AssetMetaType::MetaTypeOpaque)
        );
        Ok(AssetMeta {
            data: String::from_utf8_lossy(&m.data).to_string(),
            meta_type,
            meta_hash: hex::encode(m.meta_hash),
            decimal_display: m.decimal_display,
        })
    }

    /// Node version + network. Non-sensitive; for a status panel.
    pub async fn get_info(&mut self) -> Result<NodeInfo, String> {
        let i = self
            .assets
            .get_info(taprpc::GetInfoRequest {})
            .await
            .map_err(|e| format!("get_info: {e}"))?
            .into_inner();
        Ok(NodeInfo {
            version: i.version,
            lnd_version: i.lnd_version,
            network: i.network,
        })
    }

    /// Decode a Lightning **asset** invoice: how many asset units it settles (given
    /// the asset id) and the sat equivalent. Read-only — no ledger effect.
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

    /// Pay a Lightning **asset** invoice over an asset channel, spending `asset_id`.
    /// Streams the payment and returns the final status string (e.g. "Succeeded",
    /// "Failed"). The **caller** must have already reserved the asset amount in the
    /// ledger — this only drives the tapd/litd payment.
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

    /// Create a Lightning **asset** invoice: request `asset_amount` units of
    /// `asset_id`, priced to sats by negotiating an RFQ quote with a peer. Returns
    /// the BOLT11 payment request to hand to the payer and the hex payment hash used
    /// to detect settlement later. Requires an open asset channel + a peer that
    /// quotes the asset. `peer_pubkey` may be empty to let litd pick a peer; `memo`
    /// is the invoice description.
    pub async fn create_asset_invoice(
        &mut self,
        asset_id: &str,
        asset_amount: u64,
        peer_pubkey: &str,
        memo: &str,
    ) -> Result<CreatedAssetInvoice, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        let peer_pubkey = if peer_pubkey.trim().is_empty() {
            vec![]
        } else {
            hex::decode(peer_pubkey).map_err(|e| format!("invalid peer pubkey: {e}"))?
        };
        // The lnd invoice fields (value/value_msat) are overwritten by the asset
        // amount after the RFQ negotiation; we only carry the memo through.
        let invoice_request = lnrpc::Invoice {
            memo: memo.to_string(),
            ..Default::default()
        };
        let req = tapchannelrpc::AddInvoiceRequest {
            asset_id,
            asset_amount,
            peer_pubkey,
            invoice_request: Some(invoice_request),
            ..Default::default()
        };
        let resp = self
            .tapchannel
            .add_invoice(req)
            .await
            .map_err(|e| format!("add_invoice: {e}"))?
            .into_inner();
        let inv = resp
            .invoice_result
            .ok_or("add_invoice returned no invoice")?;
        Ok(CreatedAssetInvoice {
            payment_request: inv.payment_request,
            r_hash: hex::encode(inv.r_hash),
        })
    }

    /// Whether the invoice with hex payment hash `r_hash` has **settled**. Uses
    /// lnd's `LookupInvoice` (needs an lnd macaroon — see `connect`). A pending,
    /// canceled, or not-yet-known invoice returns `false`.
    pub async fn lookup_invoice_settled(&mut self, r_hash: &str) -> Result<bool, String> {
        let r_hash = hex::decode(r_hash).map_err(|e| format!("invalid r_hash: {e}"))?;
        let inv = self
            .lightning
            .lookup_invoice(lnrpc::PaymentHash {
                r_hash,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("lookup_invoice: {e}"))?
            .into_inner();
        Ok(inv.state == lnrpc::invoice::InvoiceState::Settled as i32)
    }

    /// The node's currently-accepted RFQ quote counts (buy/sell). Read-only.
    pub async fn list_rfq_quotes(&mut self) -> Result<RfqQuotes, String> {
        let resp = self
            .rfq
            .query_peer_accepted_quotes(rfqrpc::QueryPeerAcceptedQuotesRequest::default())
            .await
            .map_err(|e| format!("query_peer_accepted_quotes: {e}"))?
            .into_inner();
        Ok(RfqQuotes {
            buy_quotes: resp.buy_quotes.len(),
            sell_quotes: resp.sell_quotes.len(),
        })
    }

    /// Mint a single asset and finalize its batch (broadcast). Returns the batch
    /// identifiers; the asset id only exists once the genesis confirms on-chain, so
    /// ownership is attributed later by matching an asset's `anchor_txid` to
    /// `batch_txid` (see the reconcile service).
    pub async fn mint_asset(
        &mut self,
        name: &str,
        amount: u64,
        metadata: &str,
        collectible: bool,
        grouped: bool,
        fee_rate_sat_vb: u32,
    ) -> Result<MintOutcome, String> {
        let meta = taprpc::AssetMeta {
            data: metadata.as_bytes().to_vec(),
            r#type: taprpc::AssetMetaType::MetaTypeOpaque as i32,
            meta_hash: vec![],
        };
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
            // Emit a group key so more of this asset can be issued later (a
            // reissuable group). Off => a fixed one-off supply.
            new_grouped_asset: grouped,
            ..Default::default()
        };
        self.mint
            .mint_asset(mintrpc::MintAssetRequest {
                asset: Some(asset),
                short_response: true,
            })
            .await
            .map_err(|e| format!("mint_asset: {e}"))?;

        // tapd fee_rate is sat/kw; ~250 sat/kw per sat/vB. 0 lets tapd choose.
        let batch = self
            .mint
            .finalize_batch(mintrpc::FinalizeBatchRequest {
                fee_rate: fee_rate_sat_vb.saturating_mul(250),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("finalize_batch: {e}"))?
            .into_inner()
            .batch;
        let batch = batch.ok_or_else(|| "finalize_batch returned no batch".to_string())?;
        Ok(MintOutcome {
            batch_key: hex::encode(batch.batch_key),
            batch_txid: batch.batch_txid,
        })
    }

    /// Send an asset to a Taproot Asset address (on-chain). The address encodes the
    /// asset id and amount, so the caller's ownership is checked by decoding it
    /// first (see the route). Returns the anchor txid.
    pub async fn send_asset(&mut self, addr: &str, fee_rate_sat_vb: u32) -> Result<String, String> {
        let req = taprpc::SendAssetRequest {
            tap_addrs: vec![addr.to_string()],
            fee_rate: fee_rate_sat_vb.saturating_mul(250),
            ..Default::default()
        };
        let transfer = self
            .assets
            .send_asset(req)
            .await
            .map_err(|e| format!("send_asset: {e}"))?
            .into_inner()
            .transfer
            .ok_or("send_asset: no transfer returned")?;
        Ok(hex::encode(transfer.anchor_tx_hash))
    }

    /// Burn (destroy) `amount` units of an asset. Returns the anchor txid.
    pub async fn burn_asset(&mut self, asset_id: &str, amount: u64) -> Result<String, String> {
        let req = taprpc::BurnAssetRequest {
            amount_to_burn: amount,
            confirmation_text: "assets will be destroyed".to_string(),
            asset: Some(taprpc::burn_asset_request::Asset::AssetIdStr(
                asset_id.to_string(),
            )),
            ..Default::default()
        };
        let txid = self
            .assets
            .burn_asset(req)
            .await
            .map_err(|e| format!("burn_asset: {e}"))?
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

    /// Generate a Taproot Asset address to receive `amount` of `asset_id`. The
    /// gateway remembers which pubkey this address is for, and credits them once an
    /// incoming transfer to it confirms (see reconcile_receives).
    pub async fn new_address(&mut self, asset_id: &str, amount: u64) -> Result<String, String> {
        let asset_id = hex::decode(asset_id).map_err(|e| format!("invalid asset id: {e}"))?;
        let resp = self
            .assets
            .new_addr(taprpc::NewAddrRequest {
                asset_id,
                amt: amount,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("new_addr: {e}"))?
            .into_inner();
        Ok(resp.encoded)
    }

    /// Incoming-transfer events for our receive addresses, each flagged completed
    /// (fully received & confirmed) or not.
    pub async fn addr_receives(&mut self) -> Result<Vec<AddrReceiveEvent>, String> {
        let resp = self
            .assets
            .addr_receives(taprpc::AddrReceivesRequest::default())
            .await
            .map_err(|e| format!("addr_receives: {e}"))?
            .into_inner();
        let out = resp
            .events
            .into_iter()
            .map(|e| AddrReceiveEvent {
                addr: e
                    .addr
                    .as_ref()
                    .map(|a| a.encoded.clone())
                    .unwrap_or_default(),
                completed: e.status == taprpc::AddrEventStatus::Completed as i32,
            })
            .collect();
        Ok(out)
    }

    /// List minting batches, mapping each batch key to its (possibly empty) txid.
    /// Used to fill in a pending mint's txid before matching it to an asset.
    pub async fn list_batches(&mut self) -> Result<Vec<BatchRef>, String> {
        let resp = self
            .mint
            .list_batches(mintrpc::ListBatchRequest::default())
            .await
            .map_err(|e| format!("list_batches: {e}"))?
            .into_inner();
        let out = resp
            .batches
            .into_iter()
            .filter_map(|vb| {
                let b = vb.batch?;
                Some(BatchRef {
                    batch_key: hex::encode(b.batch_key),
                    batch_txid: b.batch_txid,
                })
            })
            .collect();
        Ok(out)
    }
}

/// A Tower connector that opens a plain TCP stream to the local tapd and wraps it
/// in TLS with the pinned certificate. Mirrors the app's Tor connector but over
/// loopback TCP (the gateway runs on the same box as tapd).
#[derive(Clone)]
struct LocalTlsConnector {
    tls: Arc<rustls::ClientConfig>,
    server_name: String,
}

impl tower_service::Service<Uri> for LocalTlsConnector {
    type Response = TokioIo<TlsStream<TcpStream>>;
    type Error = String;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls = self.tls.clone();
        let server_name = self.server_name.clone();
        Box::pin(async move {
            let host = uri.host().ok_or_else(|| "uri has no host".to_string())?;
            let port = uri.port_u16().unwrap_or(443);
            let stream = TcpStream::connect((host, port))
                .await
                .map_err(|e| format!("tcp connect: {e}"))?;
            let sni = rustls::pki_types::ServerName::try_from(server_name)
                .map_err(|e| format!("invalid TLS server name: {e}"))?;
            let tls_stream = TlsConnector::from(tls)
                .connect(sni, stream)
                .await
                .map_err(|e| format!("tls handshake: {e}"))?;
            Ok(TokioIo::new(tls_stream))
        })
    }
}

/// A rustls verifier that accepts exactly one pinned certificate (by DER bytes).
#[derive(Debug)]
struct PinnedCertVerifier {
    pinned: Vec<u8>,
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
        if end_entity.as_ref() == self.pinned.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "tapd server certificate does not match the pinned certificate".into(),
            ))
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

fn parse_cert_der(pem: &str) -> Result<Vec<u8>, String> {
    if pem.trim().is_empty() {
        return Err("tapd certificate is required".into());
    }
    let b64: String = pem
        .lines()
        .filter(|l| !l.contains("CERTIFICATE"))
        .map(|l| l.trim())
        .collect();
    B64.decode(b64)
        .map_err(|e| format!("decode tapd certificate: {e}"))
}

fn build_pinned_tls(cert_pem: &str) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCertVerifier {
        pinned: parse_cert_der(cert_pem)?,
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
