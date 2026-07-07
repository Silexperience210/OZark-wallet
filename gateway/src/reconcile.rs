//! Mint reconciliation.
//!
//! A mint is asynchronous: tapd broadcasts a batch immediately, but the asset id
//! only exists once the genesis confirms on-chain. We therefore hold an owner
//! claim keyed by batch (`pending_mints`) and, whenever convenient, resolve it to
//! the real asset id by matching a finalized asset's **anchor txid** to the batch's
//! txid — a cryptographic link, not name-matching. This runs opportunistically on
//! requests (mint/status/assets), so no background daemon is required.

use std::collections::HashMap;

use crate::auth::now_secs;
use crate::registry::Registry;
use crate::tapd::{lnrpc, TapdClient};

/// Run both reconcilers, logging (not propagating) errors — a best-effort refresh
/// invoked on requests so confirmed mints/receives surface promptly.
pub async fn reconcile_all(tapd: &mut TapdClient, registry: &Registry) {
    if let Err(e) = reconcile_mints(tapd, registry).await {
        log::warn!("reconcile mints: {e}");
    }
    if let Err(e) = reconcile_receives(tapd, registry).await {
        log::warn!("reconcile receives: {e}");
    }
    if let Err(e) = reconcile_ln_receives(tapd, registry).await {
        log::warn!("reconcile ln receives: {e}");
    }
}

/// Credit settled Lightning-asset invoices to the users that requested them, and
/// drop canceled/expired ones. Polls lnd's `LookupInvoice` per pending invoice
/// (needs an lnd macaroon — see `TapdClient::connect`); a single lookup failure is
/// logged and skipped so one bad hash never stalls the rest. Returns the number
/// newly credited.
pub async fn reconcile_ln_receives(
    tapd: &mut TapdClient,
    registry: &Registry,
) -> Result<usize, String> {
    let pending = registry.pending_ln_receives().map_err(|e| e.to_string())?;
    if pending.is_empty() {
        return Ok(0);
    }
    let settled = lnrpc::invoice::InvoiceState::Settled as i32;
    let canceled = lnrpc::invoice::InvoiceState::Canceled as i32;
    let mut resolved = 0;
    for p in pending {
        match tapd.lookup_invoice_state(&p.r_hash).await {
            Ok(state) if state == settled => {
                if registry
                    .resolve_ln_receive(&p.r_hash)
                    .map_err(|e| e.to_string())?
                {
                    resolved += 1;
                }
            }
            // Canceled/expired — drop it so we stop polling a dead invoice.
            Ok(state) if state == canceled => {
                let _ = registry.delete_pending_ln_receive(&p.r_hash);
            }
            // OPEN / ACCEPTED — still waiting.
            Ok(_) => {}
            Err(e) => log::warn!("lookup_invoice {}: {e}", p.r_hash),
        }
    }
    Ok(resolved)
}

/// Audit custodial solvency: for each asset with outstanding liabilities, compare
/// the sum of user balances to tapd's actual holding. Logs an **error** on any
/// drift (liabilities exceeding holdings) — the one invariant that must never
/// break. Best-effort; surfaces the tapd fetch error to the caller (which logs).
pub async fn audit_solvency(tapd: &mut TapdClient, registry: &Registry) -> Result<(), String> {
    let liabilities = registry
        .total_liabilities_by_asset()
        .map_err(|e| e.to_string())?;
    if liabilities.is_empty() {
        return Ok(());
    }
    // tapd may report several anchors for one asset id; sum them into the holding.
    let mut held: HashMap<String, u128> = HashMap::new();
    for a in tapd.list_assets().await? {
        *held.entry(a.asset_id).or_default() += a.amount as u128;
    }
    for (asset_id, liability) in liabilities {
        let holding = held.get(&asset_id).copied().unwrap_or(0);
        if u128::from(liability) > holding {
            log::error!(
                "SOLVENCY DRIFT asset {asset_id}: liabilities {liability} > node holding {holding}"
            );
        }
    }
    Ok(())
}

/// Credit confirmed incoming transfers to the users that generated their receive
/// addresses. Returns the number newly credited.
pub async fn reconcile_receives(
    tapd: &mut TapdClient,
    registry: &Registry,
) -> Result<usize, String> {
    let pending = registry.pending_receives().map_err(|e| e.to_string())?;
    if pending.is_empty() {
        return Ok(0);
    }
    let events = tapd.addr_receives().await?;
    let mut resolved = 0;
    for p in pending {
        let done = events.iter().any(|e| e.completed && e.addr == p.addr);
        if done
            && registry
                .resolve_receive(&p.addr)
                .map_err(|e| e.to_string())?
        {
            resolved += 1;
        }
    }
    Ok(resolved)
}

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
