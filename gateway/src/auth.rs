//! NIP-98 (HTTP Auth) verification.
//!
//! Every request carries an `Authorization: Nostr <base64(event)>` header where the
//! event is a **kind 27235** Nostr event signed by the caller's NIP-06 key (the same
//! key the wallet derives from its seed). We verify the signature, that the event is
//! fresh (bounded clock skew, so a captured header can't be replayed later), and that
//! its `u`/`method`/`payload` tags match the actual request. The verified pubkey is
//! the identity every ownership check in the gateway keys off.
//!
//! Spec: <https://github.com/nostr-protocol/nips/blob/master/98.md>

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use nostr::prelude::{Event, JsonUtil, Kind};
use sha2::{Digest, Sha256};
use url::Url;

use crate::state::AuthConfig;

/// NIP-98 HTTP Auth event kind.
pub const NIP98_KIND: u16 = 27235;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing Authorization header")]
    MissingHeader,
    #[error("Authorization scheme must be 'Nostr'")]
    BadScheme,
    #[error("invalid base64 in Authorization header")]
    BadBase64,
    #[error("invalid nostr event: {0}")]
    BadEvent(String),
    #[error("invalid event signature")]
    BadSignature,
    #[error("wrong event kind (expected NIP-98 kind 27235)")]
    WrongKind,
    #[error("auth event is expired or from the future")]
    StaleTimestamp,
    #[error("missing '{0}' tag")]
    MissingTag(&'static str),
    #[error("request URL does not match the signed 'u' tag")]
    UrlMismatch,
    #[error("HTTP method does not match the signed 'method' tag")]
    MethodMismatch,
    #[error("request body does not match the signed 'payload' tag")]
    PayloadMismatch,
    #[error("auth event already used (replay)")]
    Replayed,
    #[error("rate limit exceeded")]
    RateLimited,
}

/// Result of a successful verification: the caller's identity plus the fields the
/// replay guard needs (the event id, and when it goes stale).
#[derive(Debug, PartialEq, Eq)]
pub struct VerifiedAuth {
    /// x-only pubkey, 64-hex — the account id all ownership checks use.
    pub pubkey_hex: String,
    /// The NIP-98 event id (64-hex) — spent once, for replay protection.
    pub event_id: String,
    /// The event's `created_at` (unix secs), for computing the replay expiry.
    pub created_at: u64,
}

/// Verify a NIP-98 Authorization header against the request it accompanies.
///
/// - `expected_url`: when `host_strict`, the full request URL to match the `u`
///   tag against; otherwise the request path+query (host-agnostic — see
///   [`AuthConfig::public_base_url`]).
/// - `body`: the raw request body, checked against the optional `payload` tag.
#[allow(clippy::too_many_arguments)]
pub fn verify_nip98(
    auth_header: &str,
    method: &str,
    expected_url: &str,
    host_strict: bool,
    body: &[u8],
    now_secs: u64,
    max_skew_secs: u64,
) -> Result<VerifiedAuth, AuthError> {
    // Scheme is case-insensitive per RFC 7235; the value is the base64 event.
    let token = auth_header
        .strip_prefix("Nostr ")
        .or_else(|| auth_header.strip_prefix("nostr "))
        .ok_or(AuthError::BadScheme)?
        .trim();

    let json = B64.decode(token).map_err(|_| AuthError::BadBase64)?;
    let event = Event::from_json(json).map_err(|e| AuthError::BadEvent(e.to_string()))?;

    // Cryptographically bind the request to the key: verifies id + signature.
    event.verify().map_err(|_| AuthError::BadSignature)?;

    if event.kind != Kind::from(NIP98_KIND) {
        return Err(AuthError::WrongKind);
    }

    // Freshness window: rejects both stale (replay) and far-future timestamps.
    let ts = event.created_at.as_secs();
    if now_secs.abs_diff(ts) > max_skew_secs {
        return Err(AuthError::StaleTimestamp);
    }

    let mut url_tag: Option<&str> = None;
    let mut method_tag: Option<&str> = None;
    let mut payload_tag: Option<&str> = None;
    for tag in event.tags.iter() {
        let s = tag.as_slice();
        if s.len() < 2 {
            continue;
        }
        match s[0].as_str() {
            "u" => url_tag = Some(s[1].as_str()),
            "method" => method_tag = Some(s[1].as_str()),
            "payload" => payload_tag = Some(s[1].as_str()),
            _ => {}
        }
    }

    let method_tag = method_tag.ok_or(AuthError::MissingTag("method"))?;
    if !method_tag.eq_ignore_ascii_case(method) {
        return Err(AuthError::MethodMismatch);
    }

    let url_tag = url_tag.ok_or(AuthError::MissingTag("u"))?;
    if !url_matches(url_tag, expected_url, host_strict) {
        return Err(AuthError::UrlMismatch);
    }

    // The payload tag is optional (NIP-98); when present it must be the hex
    // sha256 of the body, binding the exact request contents to the signature.
    if let Some(p) = payload_tag {
        let digest = hex::encode(Sha256::digest(body));
        if !p.eq_ignore_ascii_case(&digest) {
            return Err(AuthError::PayloadMismatch);
        }
    }

    Ok(VerifiedAuth {
        pubkey_hex: event.pubkey.to_hex(),
        event_id: event.id.to_hex(),
        created_at: ts,
    })
}

/// Compare the signed `u` tag to the request URL. In `host_strict` mode the full
/// origin (scheme/host/port) plus path+query must match; otherwise only path+query.
fn url_matches(signed: &str, expected: &str, host_strict: bool) -> bool {
    let Ok(signed) = Url::parse(signed) else {
        return false;
    };
    if host_strict {
        match Url::parse(expected) {
            Ok(exp) => {
                signed.scheme() == exp.scheme()
                    && signed.host_str() == exp.host_str()
                    && signed.port_or_known_default() == exp.port_or_known_default()
                    && signed.path() == exp.path()
                    && signed.query() == exp.query()
            }
            Err(_) => false,
        }
    } else {
        path_and_query(&signed).as_str() == expected
    }
}

fn path_and_query(u: &Url) -> String {
    match u.query() {
        Some(q) => format!("{}?{}", u.path(), q),
        None => u.path().to_string(),
    }
}

/// Extract-and-verify helper for handlers: pulls the header/method/URL off the
/// request, reconstructs the expected URL per config, enforces the per-pubkey rate
/// limit and single-use (anti-replay) of the auth event, and returns the caller's
/// pubkey. GET handlers pass an empty body (their auth events carry no payload).
pub fn authenticate(
    auth: &AuthConfig,
    security: &crate::security::Security,
    headers: &axum::http::HeaderMap,
    method: &axum::http::Method,
    uri: &axum::http::Uri,
    body: &[u8],
) -> Result<String, AuthError> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingHeader)?;

    let pq = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_else(|| uri.path());

    let (expected, host_strict) = match &auth.public_base_url {
        Some(base) => (format!("{}{}", base.trim_end_matches('/'), pq), true),
        None => (pq.to_string(), false),
    };

    let now = now_secs();
    let verified = verify_nip98(
        header,
        method.as_str(),
        &expected,
        host_strict,
        body,
        now,
        auth.max_skew_secs,
    )?;

    // Post-verification guards (we now have a trusted pubkey + event id).
    if !security.allow_rate(&verified.pubkey_hex) {
        return Err(AuthError::RateLimited);
    }
    // The event can be replayed only within its freshness window; expire the
    // spent id just past it (+1s guard) so the map self-empties.
    let expiry = verified
        .created_at
        .saturating_add(auth.max_skew_secs)
        .saturating_add(1);
    if !security.check_replay(&verified.event_id, expiry, now) {
        return Err(AuthError::Replayed);
    }
    Ok(verified.pubkey_hex)
}

/// Current unix time in seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, Keys, Tag, Timestamp};

    const URL: &str = "http://abcdefghijklmnop.onion/v1/assets";
    const PATH: &str = "/v1/assets";

    /// Build a NIP-98 Authorization header value, optionally with a payload tag.
    fn header_with(
        keys: &Keys,
        method: &str,
        url: &str,
        created_at: u64,
        payload: Option<&str>,
    ) -> String {
        let mut tags = vec![
            Tag::parse(["u".to_string(), url.to_string()]).unwrap(),
            Tag::parse(["method".to_string(), method.to_string()]).unwrap(),
        ];
        if let Some(p) = payload {
            tags.push(Tag::parse(["payload".to_string(), p.to_string()]).unwrap());
        }
        let event = EventBuilder::new(Kind::from(NIP98_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap();
        format!("Nostr {}", B64.encode(event.as_json()))
    }

    fn verify_path(header: &str, method: &str, now: u64) -> Result<VerifiedAuth, AuthError> {
        verify_nip98(header, method, PATH, false, &[], now, 60)
    }

    #[test]
    fn valid_auth_passes_host_agnostic() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", URL, now, None);
        let v = verify_path(&h, "GET", now).unwrap();
        assert_eq!(v.pubkey_hex, keys.public_key().to_hex());
    }

    #[test]
    fn valid_auth_passes_host_strict() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", URL, now, None);
        let v = verify_nip98(&h, "GET", URL, true, &[], now, 60).unwrap();
        assert_eq!(v.pubkey_hex, keys.public_key().to_hex());
    }

    #[test]
    fn expired_event_rejected() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", URL, now - 3600, None);
        assert_eq!(verify_path(&h, "GET", now), Err(AuthError::StaleTimestamp));
    }

    #[test]
    fn future_event_rejected() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", URL, now + 3600, None);
        assert_eq!(verify_path(&h, "GET", now), Err(AuthError::StaleTimestamp));
    }

    #[test]
    fn wrong_method_rejected() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", URL, now, None);
        assert_eq!(verify_path(&h, "POST", now), Err(AuthError::MethodMismatch));
    }

    #[test]
    fn wrong_url_rejected() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", "http://x.onion/v1/universe/roots", now, None);
        assert_eq!(verify_path(&h, "GET", now), Err(AuthError::UrlMismatch));
    }

    #[test]
    fn host_strict_rejects_different_host() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", "http://evil.onion/v1/assets", now, None);
        assert_eq!(
            verify_nip98(&h, "GET", URL, true, &[], now, 60),
            Err(AuthError::UrlMismatch)
        );
    }

    #[test]
    fn missing_header_scheme_rejected() {
        let now = 1_700_000_000;
        assert_eq!(
            verify_nip98("Bearer xyz", "GET", PATH, false, &[], now, 60),
            Err(AuthError::BadScheme)
        );
    }

    #[test]
    fn tampered_signature_rejected() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let h = header_with(&keys, "GET", URL, now, None);
        // Flip one nibble of the signature so id parses but verify() fails.
        let b64 = h.strip_prefix("Nostr ").unwrap();
        let json = String::from_utf8(B64.decode(b64).unwrap()).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let sig = v["sig"].as_str().unwrap().to_string();
        let mut chars: Vec<char> = sig.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        v["sig"] = serde_json::Value::String(chars.into_iter().collect());
        let bad = format!("Nostr {}", B64.encode(serde_json::to_string(&v).unwrap()));
        assert_eq!(verify_path(&bad, "GET", now), Err(AuthError::BadSignature));
    }

    #[test]
    fn payload_tag_binds_body() {
        let keys = Keys::generate();
        let now = 1_700_000_000;
        let digest = hex::encode(Sha256::digest(b"hello"));
        let h = header_with(&keys, "POST", URL, now, Some(&digest));
        // Matching body passes.
        assert!(verify_nip98(&h, "POST", URL, true, b"hello", now, 60).is_ok());
        // Mismatched body is rejected.
        assert_eq!(
            verify_nip98(&h, "POST", URL, true, b"world", now, 60),
            Err(AuthError::PayloadMismatch)
        );
    }

    use crate::security::{Security, SecurityConfig};

    fn test_auth_config() -> AuthConfig {
        AuthConfig {
            public_base_url: None,
            max_skew_secs: 60,
            admin_pubkey: None,
            allow_admin_claim: false,
        }
    }

    fn headers_with(value: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn authenticate_rejects_replay() {
        let keys = Keys::generate();
        let auth = test_auth_config();
        // Generous limiter so the *only* thing rejecting the 2nd call is replay.
        let sec = Security::new(SecurityConfig {
            burst: 100.0,
            refill_per_sec: 100.0,
        });
        let uri: axum::http::Uri = "/v1/assets".parse().unwrap();
        let method = axum::http::Method::GET;
        // created_at must be within skew of *real* now (authenticate uses now_secs()).
        let h = header_with(&keys, "GET", URL, now_secs(), None);
        let headers = headers_with(&h);

        let first = authenticate(&auth, &sec, &headers, &method, &uri, &[]);
        assert_eq!(first.unwrap(), keys.public_key().to_hex());
        let second = authenticate(&auth, &sec, &headers, &method, &uri, &[]);
        assert_eq!(second, Err(AuthError::Replayed));
    }

    #[test]
    fn authenticate_rate_limits_per_pubkey() {
        let keys = Keys::generate();
        let auth = test_auth_config();
        // Burst of 1, negligible refill: the 2nd distinct request is throttled.
        let sec = Security::new(SecurityConfig {
            burst: 1.0,
            refill_per_sec: 0.000_001,
        });
        let uri: axum::http::Uri = "/v1/assets".parse().unwrap();
        let method = axum::http::Method::GET;
        let now = now_secs();
        // Distinct created_at -> distinct event ids, so replay is not the cause.
        let h1 = header_with(&keys, "GET", URL, now, None);
        let h2 = header_with(&keys, "GET", URL, now + 1, None);

        let r1 = authenticate(&auth, &sec, &headers_with(&h1), &method, &uri, &[]);
        assert!(r1.is_ok());
        let r2 = authenticate(&auth, &sec, &headers_with(&h2), &method, &uri, &[]);
        assert_eq!(r2, Err(AuthError::RateLimited));
    }
}
