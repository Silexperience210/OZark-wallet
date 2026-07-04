//! Nostr discovery for the marketplace.
//!
//! A public token is announced as an **addressable** Nostr event (kind
//! [`MARKET_KIND`]) authored by the desk's pubkey. The `d` tag is the asset id,
//! so re-publishing replaces the previous announcement for that token. All the
//! metadata (curve params, fee, ticker…) lives in the JSON content — the market
//! catalogue is therefore fully public and discoverable by any wallet.
//!
//! This module keeps the *pure* event build/parse here (unit-tested, no
//! network); the async relay I/O (publish / fetch) lives alongside and is not
//! exercised in CI (no outbound network in the test sandbox).

use std::time::Duration;

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// Custom addressable event kind for an OZark market token announcement.
pub const MARKET_KIND: u16 = 30333;

/// Relays the app talks to out of the box — zero configuration for the user.
pub const DEFAULT_RELAYS: [&str; 3] = [
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

/// The public metadata a desk announces for one token. Serialised into the event
/// content; the event author (pubkey) is the desk that runs the market.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAnnouncement {
    pub asset_id: String,
    pub ticker: String,
    pub name: String,
    pub p0_num: u64,
    pub k_num: u64,
    pub denom: u64,
    pub supply_cap: u64,
    pub migration_sats: u64,
    pub creator_fee_bp: u16,
    // Live snapshot as of the last publish. Because the event is addressable
    // (replaceable), the desk re-publishes on trades, so the public catalogue
    // reflects an up-to-date price without any private channel.
    pub supply: u64,
    pub reserve_sats: u64,
    pub spot_price_msat: u64,
    pub status: String,
}

/// A token discovered on the relays, with the (signed) desk pubkey that runs it.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredToken {
    /// Hex pubkey of the desk — taken from the event signature, not spoofable.
    pub desk_pubkey: String,
    pub ann: TokenAnnouncement,
}

/// Build a signed, addressable announcement event for a token.
pub fn build_token_event(keys: &Keys, ann: &TokenAnnouncement) -> Result<Event, String> {
    let content = serde_json::to_string(ann).map_err(|e| e.to_string())?;
    EventBuilder::new(Kind::Custom(MARKET_KIND), content)
        .tags([Tag::identifier(ann.asset_id.clone())])
        .sign_with_keys(keys)
        .map_err(|e| e.to_string())
}

/// Parse a token announcement from an event (right kind + JSON content).
pub fn parse_token_event(event: &Event) -> Option<TokenAnnouncement> {
    if event.kind != Kind::Custom(MARKET_KIND) {
        return None;
    }
    serde_json::from_str(&event.content).ok()
}

/// Connect to the default relays, publish (replace) the token announcement, and
/// disconnect. Returns the event id hex.
pub async fn publish_announcement(keys: &Keys, ann: &TokenAnnouncement) -> Result<String, String> {
    let event = build_token_event(keys, ann)?;
    let client = Client::new(keys.clone());
    for relay in DEFAULT_RELAYS {
        client.add_relay(relay).await.map_err(|e| e.to_string())?;
    }
    client.connect().await;
    client.send_event(&event).await.map_err(|e| e.to_string())?;
    client.disconnect().await;
    Ok(event.id.to_hex())
}

/// Query the default relays for all token announcements (the public catalogue).
pub async fn fetch_catalog(keys: &Keys) -> Result<Vec<DiscoveredToken>, String> {
    let client = Client::new(keys.clone());
    for relay in DEFAULT_RELAYS {
        client.add_relay(relay).await.map_err(|e| e.to_string())?;
    }
    client.connect().await;
    let filter = Filter::new().kind(Kind::Custom(MARKET_KIND));
    let events = client
        .fetch_events(filter, Duration::from_secs(8))
        .await
        .map_err(|e| e.to_string())?;
    client.disconnect().await;
    let out = events
        .into_iter()
        .filter_map(|e| {
            parse_token_event(&e).map(|ann| DiscoveredToken {
                desk_pubkey: e.pubkey.to_hex(),
                ann,
            })
        })
        .collect();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TokenAnnouncement {
        TokenAnnouncement {
            asset_id: "aa".into(),
            ticker: "OZ".into(),
            name: "OZark".into(),
            p0_num: 1_000_000_000,
            k_num: 99_000,
            denom: 1_000_000_000,
            supply_cap: 1_000_000,
            migration_sats: 0,
            creator_fee_bp: 100,
            supply: 1_234,
            reserve_sats: 5_678,
            spot_price_msat: 1_500,
            status: "trading".into(),
        }
    }

    #[test]
    fn announcement_round_trips_through_event() {
        let keys = Keys::generate();
        let ann = sample();
        let event = build_token_event(&keys, &ann).unwrap();
        assert_eq!(event.kind, Kind::Custom(MARKET_KIND));
        assert_eq!(event.pubkey, keys.public_key());
        let parsed = parse_token_event(&event).expect("parse");
        assert_eq!(parsed, ann);
    }

    #[test]
    fn wrong_kind_is_ignored() {
        let keys = Keys::generate();
        let e = EventBuilder::new(Kind::TextNote, "hi")
            .sign_with_keys(&keys)
            .unwrap();
        assert!(parse_token_event(&e).is_none());
    }
}
