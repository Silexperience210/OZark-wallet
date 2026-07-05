//! Mint reconciliation.
//!
//! A mint is asynchronous: tapd broadcasts a batch immediately, but the asset id
//! only exists once the genesis confirms on-chain. We therefore hold an owner
//! claim keyed by batch (`pending_mints`) and, whenever convenient, resolve it to
//! the real asset id by matching a finalized asset's **anchor txid** to the batch's
//! txid — a cryptographic link, not name-matching. This runs opportunistically on
//! requests (mint/status/assets), so no background daemon is required.

use crate::auth::now_secs;
use crate::registry::Registry;
use crate::tapd::TapdClient;

/// Resolve as many pending mints as possible. Returns the number newly resolved.
/// Best-effort: surfaces tapd errors to the caller (which logs) but never panics.
pub async fn reconcile_mints(tapd: &mut TapdClient, registry: &Registry) -> Result<usize, String> {
    let pending = registry.pending_mints().map_err(|e| e.to_string())?;
    if pending.is_empty() {
        return Ok(0);
    }

    // 1. Learn any still-unknown batch txids from tapd's batch list.
    if pending.iter().any(|p| p.batch_txid.is_empty()) {
        let batches = tapd.list_batches().await?;
        for p in pending.iter().filter(|p| p.batch_txid.is_empty()) {
            if let Some(b) = batches
                .iter()
                .find(|b| b.batch_key == p.batch_key && !b.batch_txid.is_empty())
            {
                registry
                    .set_pending_txid(&p.batch_key, &b.batch_txid)
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    // 2. Match finalized assets to pending mints by anchor txid and record owner.
    let pending: Vec<_> = registry
        .pending_mints()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|p| !p.batch_txid.is_empty())
        .collect();
    if pending.is_empty() {
        return Ok(0);
    }

    let assets = tapd.list_assets().await?;
    let now = now_secs() as i64;
    let mut resolved = 0;
    for p in pending {
        let matched = assets
            .iter()
            .find(|a| !a.anchor_txid.is_empty() && a.anchor_txid == p.batch_txid);
        if let Some(a) = matched {
            if registry
                .resolve_pending_mint(&p.batch_key, &a.asset_id, now)
                .map_err(|e| e.to_string())?
            {
                resolved += 1;
            }
        }
    }
    Ok(resolved)
}
