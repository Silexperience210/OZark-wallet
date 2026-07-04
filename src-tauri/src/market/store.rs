//! Persistence for the marketplace desk.
//!
//! The whole [`Desk`] is serialised as a pretty JSON snapshot in the app data
//! dir, mirroring how the ark/tor configs are stored. The desk mutates on every
//! trade, so mutating commands save through after each change. The file holds
//! **accounting state** (reserves, custodial ledger, trade log) — not keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::desk::Desk;
use super::settle::Order;

const DESK_FILE: &str = "market-desk.json";
const ORDERS_FILE: &str = "market-orders.json";

fn desk_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DESK_FILE)
}

/// Load the persisted desk, or an empty one if none exists. A file that exists
/// but cannot be parsed is logged and treated as empty rather than crashing
/// startup (V1 — keeping a `.bak` copy before overwrite is a follow-up).
pub fn load_or_default(data_dir: &Path) -> Desk {
    let path = desk_path(data_dir);
    if !path.exists() {
        return Desk::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str(&json) {
            Ok(desk) => desk,
            Err(e) => {
                log::error!("market desk snapshot at {path:?} is corrupt ({e}); starting empty");
                Desk::default()
            }
        },
        Err(e) => {
            log::error!("could not read market desk snapshot at {path:?} ({e}); starting empty");
            Desk::default()
        }
    }
}

/// Persist the desk snapshot.
pub fn save(data_dir: &Path, desk: &Desk) -> Result<(), String> {
    let path = desk_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(desk).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

fn orders_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ORDERS_FILE)
}

/// Load the desk's remote orders keyed by order id (empty if none).
pub fn load_orders(data_dir: &Path) -> HashMap<String, Order> {
    let path = orders_path(data_dir);
    if !path.exists() {
        return HashMap::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the desk's remote orders.
pub fn save_orders(data_dir: &Path, orders: &HashMap<String, Order>) -> Result<(), String> {
    let path = orders_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(orders).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::curve::CurveParams;
    use crate::market::desk::{MarketSpec, Visibility};

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ozark-desk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_is_empty_desk() {
        let dir = tmp_dir("empty");
        assert!(load_or_default(&dir).markets.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tmp_dir("roundtrip");
        let mut desk = Desk::default();
        desk.create_market(
            MarketSpec {
                token_id: "aa".into(),
                ticker: "OZ".into(),
                name: "OZark".into(),
                creator: "alice".into(),
                params: CurveParams::new(1, 1, 1, 1_000_000, 10_000_000).unwrap(),
                visibility: Visibility::Public,
                creator_fee_bp: 100,
                seed_sats: 5_000,
            },
            42,
        )
        .unwrap();
        save(&dir, &desk).unwrap();

        let loaded = load_or_default(&dir);
        assert_eq!(loaded.markets.len(), 1);
        let m = loaded.market("aa").unwrap();
        assert_eq!(m.ticker, "OZ");
        assert!(m.supply > 0);
        assert_eq!(m.balances.get("alice").copied().unwrap_or(0), m.supply);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
