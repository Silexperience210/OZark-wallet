//! The desk listener: an in-app background loop that serves remote buyers over
//! Nostr DMs. The exact same logic is meant to run later as a 24/7 Umbrel daemon
//! — only where it runs changes.
//!
//! Each poll: fetch DMs → parse a [`DeskRequest`] → reply. On `BuyQuote` the desk
//! quotes and returns an LN invoice; on `PaymentProof` it verifies the preimage,
//! credits the buyer's pubkey through the desk, publishes the trade to the public
//! tape, and confirms. Orders are keyed by the request's gift-wrap id, so
//! re-processing is idempotent.
//!
//! Behaviour here can only be validated on-device (it needs live Lightning +
//! relays); CI only type-checks it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bark::lightning_invoice::Bolt11Invoice;
use nostr::Keys;

use super::commands::{announcement_of, tape_entry};
use super::desk::Desk;
use super::nostr_client;
use super::settle::{DeskRequest, DeskResponse, Order, OrderStatus};
use super::store;
use crate::ark::ArkService;

const POLL_INTERVAL_SECS: u64 = 20;

/// Everything the listener needs (cheap Arc handles + the desk keys).
pub struct ListenerCtx {
    pub keys: Keys,
    pub desk: Arc<Mutex<Desk>>,
    pub orders: Arc<Mutex<HashMap<String, Order>>>,
    pub ark: Arc<Mutex<Option<ArkService>>>,
    pub data_dir: PathBuf,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn err(msg: impl Into<String>) -> DeskResponse {
    DeskResponse::Error {
        message: msg.into(),
    }
}

fn filled(o: &Order, tokens: u64, sats: u64) -> DeskResponse {
    DeskResponse::Filled {
        order_id: o.order_id.clone(),
        asset_id: o.asset_id.clone(),
        side: "buy".to_string(),
        tokens,
        sats,
    }
}

/// The payment hash (hex) encoded in a BOLT11 invoice.
fn payment_hash_of(invoice: &str) -> Option<String> {
    invoice
        .parse::<Bolt11Invoice>()
        .ok()
        .map(|inv| inv.payment_hash().to_string())
}

/// Run the listener until the task is aborted.
pub async fn run(ctx: ListenerCtx) {
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        if let Err(e) = poll_once(&ctx, &mut seen).await {
            log::warn!("desk listener poll error: {e}");
        }
        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}

async fn poll_once(ctx: &ListenerCtx, seen: &mut HashSet<String>) -> Result<(), String> {
    let dms = nostr_client::receive_dms(&ctx.keys).await?;
    for (wrap_id, sender, content) in dms {
        if !seen.insert(wrap_id.clone()) {
            continue; // already handled in this session
        }
        let request: DeskRequest = match serde_json::from_str(&content) {
            Ok(r) => r,
            Err(_) => continue, // not a settlement message
        };
        let response = handle(ctx, &wrap_id, &sender, request).await;
        let json = serde_json::to_string(&response).unwrap_or_default();
        if let Err(e) = nostr_client::send_dm(&ctx.keys, &sender, &json).await {
            log::warn!("desk reply to {sender} failed: {e}");
        }
    }
    Ok(())
}

async fn handle(ctx: &ListenerCtx, wrap_id: &str, sender: &str, req: DeskRequest) -> DeskResponse {
    match req {
        DeskRequest::BuyQuote {
            asset_id,
            budget_sats,
        } => handle_buy_quote(ctx, wrap_id, sender, &asset_id, budget_sats).await,
        DeskRequest::PaymentProof { order_id, preimage } => {
            handle_payment_proof(ctx, &order_id, &preimage)
        }
        DeskRequest::OrderStatus { order_id } => handle_order_status(ctx, &order_id),
        DeskRequest::Sell {
            asset_id,
            amount,
            payout_invoice,
        } => handle_sell(ctx, sender, &asset_id, amount, &payout_invoice).await,
    }
}

/// Remote sell: the seller (who holds a custodial balance on this desk) sends a
/// payout invoice; the desk pays it, then debits the ledger. Payment happens
/// **before** the debit, so a routing failure just leaves the seller's tokens
/// untouched.
async fn handle_sell(
    ctx: &ListenerCtx,
    seller: &str,
    asset_id: &str,
    amount: u64,
    payout_invoice: &str,
) -> DeskResponse {
    // Quote the payout under the desk lock (also checks the seller's balance).
    let payout = {
        let desk = match ctx.desk.lock() {
            Ok(d) => d,
            Err(_) => return err("desk busy"),
        };
        let market = match desk.market(asset_id) {
            Ok(m) => m,
            Err(e) => return err(e.to_string()),
        };
        match market.preview_sell(seller, amount) {
            Ok(p) => p.payout_sats,
            Err(e) => return err(e.to_string()),
        }
    };
    if payout == 0 {
        return err("payout is zero");
    }

    // Pay the seller first.
    let ark = match ctx.ark.lock().ok().and_then(|a| a.clone()) {
        Some(a) => a,
        None => return err("lightning not ready"),
    };
    if let Err(e) = ark
        .pay_lightning_invoice(payout_invoice.to_string(), Some(payout))
        .await
    {
        return err(format!("payout failed: {e}"));
    }

    // Now debit the ledger + reduce the reserve.
    let (trade, ann) = {
        let mut desk = match ctx.desk.lock() {
            Ok(d) => d,
            Err(_) => return err("desk busy after payout"),
        };
        let trade = match desk.sell(asset_id, seller, amount, now_secs()) {
            Ok(t) => t,
            Err(e) => return err(e.to_string()),
        };
        let _ = store::save(&ctx.data_dir, &desk);
        let ann = match desk.market(asset_id) {
            Ok(m) => announcement_of(m),
            Err(e) => return err(e.to_string()),
        };
        (trade, ann)
    };

    let keys = ctx.keys.clone();
    let entry = tape_entry(asset_id, seller, &trade);
    tauri::async_runtime::spawn(async move {
        let _ = nostr_client::publish_trade(&keys, &entry).await;
        let _ = nostr_client::publish_announcement(&keys, &ann).await;
    });

    DeskResponse::Filled {
        order_id: String::new(),
        asset_id: asset_id.to_string(),
        side: "sell".to_string(),
        tokens: amount,
        sats: payout,
    }
}

async fn handle_buy_quote(
    ctx: &ListenerCtx,
    wrap_id: &str,
    sender: &str,
    asset_id: &str,
    budget_sats: u64,
) -> DeskResponse {
    // Idempotent: an order already exists for this request → resend its invoice.
    if let Some(existing) = ctx.orders.lock().ok().and_then(|o| o.get(wrap_id).cloned()) {
        return existing.invoice_response();
    }

    // Quote under the desk lock (dropped before the await).
    let preview = {
        let desk = match ctx.desk.lock() {
            Ok(d) => d,
            Err(_) => return err("desk busy"),
        };
        let market = match desk.market(asset_id) {
            Ok(m) => m,
            Err(e) => return err(e.to_string()),
        };
        match market.preview_buy(budget_sats) {
            Ok(p) => p,
            Err(e) => return err(e.to_string()),
        }
    };

    // Create the LN invoice for the total (curve cost + creator fee).
    let ark = match ctx.ark.lock().ok().and_then(|a| a.clone()) {
        Some(a) => a,
        None => return err("lightning not ready"),
    };
    let invoice = match ark
        .create_bolt11_invoice(preview.total_sats, Some(format!("OZark buy {asset_id}")))
        .await
    {
        Ok(inv) => inv,
        Err(e) => return err(e),
    };
    let payment_hash = match payment_hash_of(&invoice) {
        Some(h) => h,
        None => return err("could not read the invoice payment hash"),
    };

    let order = Order {
        order_id: wrap_id.to_string(),
        buyer_pubkey: sender.to_string(),
        asset_id: asset_id.to_string(),
        budget_sats,
        tokens: preview.tokens,
        cost_sats: preview.cost_sats,
        fee_sats: preview.fee_sats,
        invoice,
        payment_hash,
        status: OrderStatus::AwaitingPayment,
        created_at: now_secs(),
    };
    if let Ok(mut orders) = ctx.orders.lock() {
        orders.insert(order.order_id.clone(), order.clone());
        let _ = store::save_orders(&ctx.data_dir, &orders);
    }
    order.invoice_response()
}

fn handle_payment_proof(ctx: &ListenerCtx, order_id: &str, preimage: &str) -> DeskResponse {
    let order = match ctx
        .orders
        .lock()
        .ok()
        .and_then(|o| o.get(order_id).cloned())
    {
        Some(o) => o,
        None => return err("unknown order"),
    };
    if order.status == OrderStatus::Filled {
        return filled(&order, order.tokens, order.cost_sats);
    }
    if !order.verify_preimage(preimage) {
        return err("invalid preimage");
    }

    // The buyer paid the invoice (total = cost + fee); credit their pubkey at the
    // current curve price, and snapshot the announcement.
    let (trade, ann) = {
        let mut desk = match ctx.desk.lock() {
            Ok(d) => d,
            Err(_) => return err("desk busy"),
        };
        let trade = match desk.buy(
            &order.asset_id,
            &order.buyer_pubkey,
            order.cost_sats + order.fee_sats,
            now_secs(),
        ) {
            Ok(t) => t,
            Err(e) => return err(e.to_string()),
        };
        let _ = store::save(&ctx.data_dir, &desk);
        let ann = match desk.market(&order.asset_id) {
            Ok(m) => announcement_of(m),
            Err(e) => return err(e.to_string()),
        };
        (trade, ann)
    };

    if let Ok(mut orders) = ctx.orders.lock() {
        if let Some(o) = orders.get_mut(order_id) {
            o.status = OrderStatus::Filled;
        }
        let _ = store::save_orders(&ctx.data_dir, &orders);
    }

    // Publish the trade to the public tape + refresh the announcement.
    let keys = ctx.keys.clone();
    let entry = tape_entry(&order.asset_id, &order.buyer_pubkey, &trade);
    tauri::async_runtime::spawn(async move {
        let _ = nostr_client::publish_trade(&keys, &entry).await;
        let _ = nostr_client::publish_announcement(&keys, &ann).await;
    });

    filled(&order, trade.tokens, trade.sats)
}

fn handle_order_status(ctx: &ListenerCtx, order_id: &str) -> DeskResponse {
    match ctx
        .orders
        .lock()
        .ok()
        .and_then(|o| o.get(order_id).cloned())
    {
        Some(o) if o.status == OrderStatus::Filled => filled(&o, o.tokens, o.cost_sats),
        Some(o) => DeskResponse::Pending {
            order_id: o.order_id,
        },
        None => err("unknown order"),
    }
}
