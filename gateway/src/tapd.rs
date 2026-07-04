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

type AssetsClient =
    taprpc::taproot_assets_client::TaprootAssetsClient<InterceptedService<Channel, Macaroon>>;
type UniverseClient =
    universerpc::universe_client::UniverseClient<InterceptedService<Channel, Macaroon>>;

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
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetInfo {
    pub asset_id: String,
    pub name: String,
    pub amount: u64,
    pub asset_type: String,
    pub decimal_display: u32,
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

impl TapdClient {
    /// Connect to a local tapd. `host` is `host:port`; `cert_pem` is tapd's TLS
    /// certificate (PEM); `macaroon_hex` authorizes the gRPC calls.
    pub async fn connect(host: &str, cert_pem: &str, macaroon_hex: &str) -> Result<Self, String> {
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
        let universe =
            universerpc::universe_client::UniverseClient::with_interceptor(channel, interceptor);
        Ok(Self { assets, universe })
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
