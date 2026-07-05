//! Client for the OZark gateway.
//!
//! The wallet talks to the gateway over the node's **Tor onion** (via the embedded
//! arti client) instead of hitting tapd directly. Every request is authenticated
//! with a **NIP-98** event signed by the wallet's Nostr key (the same key derived
//! from the seed), so the gateway can enforce per-user isolation without the wallet
//! ever holding the tapd macaroon.
//!
//! The onion connection is plain HTTP: Tor provides confidentiality and the v3
//! onion address authenticates the endpoint, so no TLS is layered on top.

use crate::tor::TorService;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bitcoin::hashes::{sha256, Hash};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use nostr::prelude::{EventBuilder, JsonUtil, Keys, Kind, Tag};
use serde_json::Value;
use std::time::Duration;

/// NIP-98 HTTP Auth event kind.
const NIP98_KIND: u16 = 27235;

/// Persisted gateway settings (the onion base URL is not secret — bakeable).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GatewayConfig {
    /// e.g. `http://<onion>.onion`.
    pub base_url: String,
}

pub struct GatewayClient {
    base_url: String,
    keys: Keys,
    tor: TorService,
}

impl GatewayClient {
    pub fn new(base_url: String, keys: Keys, tor: TorService) -> Self {
        Self {
            base_url,
            keys,
            tor,
        }
    }

    /// Authenticated GET; `path` includes any query string.
    pub async fn get(&self, path: &str) -> Result<Value, String> {
        self.request("GET", path, None).await
    }

    /// Authenticated POST with a JSON body (bound to the signature via `payload`).
    pub async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.request("POST", path, Some(body)).await
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let base = self.base_url.trim_end_matches('/');
        if base.is_empty() {
            return Err("gateway URL is not configured".into());
        }
        let url = format!("{base}{path}");
        let parsed = url::Url::parse(base).map_err(|e| format!("bad gateway URL: {e}"))?;
        let host = parsed
            .host_str()
            .ok_or("gateway URL has no host")?
            .to_string();
        let port = parsed.port().unwrap_or(80);
        let host_header = match parsed.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        };

        let body_bytes = match &body {
            Some(v) => serde_json::to_vec(v).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        let auth = build_nip98(&self.keys, method, &url, &body_bytes)?;

        // Open a Tor stream to the onion and speak HTTP/1.1 over it.
        let stream = self.tor.connect(&host, port).await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| format!("gateway handshake: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                log::debug!("gateway connection closed: {e}");
            }
        });

        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, host_header)
            .header(hyper::header::AUTHORIZATION, auth);
        if body.is_some() {
            builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        let req = builder
            .body(Full::new(Bytes::from(body_bytes)))
            .map_err(|e| format!("build gateway request: {e}"))?;

        let resp = tokio::time::timeout(Duration::from_secs(60), sender.send_request(req))
            .await
            .map_err(|_| "gateway request timed out".to_string())?
            .map_err(|e| format!("gateway request: {e}"))?;

        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("read gateway response: {e}"))?
            .to_bytes();

        if !status.is_success() {
            // Surface the gateway's `{ "error": … }` message when present.
            let msg = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
            return Err(format!("gateway {}: {msg}", status.as_u16()));
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("parse gateway response: {e}"))
    }
}

/// Build the `Authorization: Nostr <base64(event)>` header for one request.
fn build_nip98(keys: &Keys, method: &str, url: &str, body: &[u8]) -> Result<String, String> {
    let mut tags = vec![
        Tag::parse(["u".to_string(), url.to_string()]).map_err(|e| e.to_string())?,
        Tag::parse(["method".to_string(), method.to_string()]).map_err(|e| e.to_string())?,
    ];
    if !body.is_empty() {
        // Forward-hex sha256, matching the gateway's `hex::encode(Sha256::digest(..))`.
        let hash = hex::encode(sha256::Hash::hash(body).as_byte_array());
        tags.push(Tag::parse(["payload".to_string(), hash]).map_err(|e| e.to_string())?);
    }
    let event = EventBuilder::new(Kind::from(NIP98_KIND), "")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|e| e.to_string())?;
    Ok(format!("Nostr {}", B64.encode(event.as_json())))
}
