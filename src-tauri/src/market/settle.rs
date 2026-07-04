//! Settlement protocol for remote (custodial) trading over Nostr DMs.
//!
//! A remote trader and a desk exchange JSON messages inside encrypted Nostr DMs.
//! The flow is custodial-over-Lightning: the buyer asks for a quote, the desk
//! replies with an LN invoice, the buyer pays it, and once the desk sees the
//! payment it credits the buyer's pubkey and confirms.
//!
//! This module holds the *pure* wire types + the order lifecycle (unit-tested).
//! The DM transport, the LN invoice/payment plumbing and the desk listener build
//! on top of these types.

use bitcoin::hashes::{sha256, Hash};
use serde::{Deserialize, Serialize};

/// Payment window before an unpaid order is abandoned (seconds).
pub const ORDER_TTL_SECS: u64 = 900; // 15 minutes

/// A message from a remote trader to a desk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeskRequest {
    /// Buy for `budget_sats`; the desk replies with an [`DeskResponse::Invoice`].
    BuyQuote { asset_id: String, budget_sats: u64 },
    /// Sell `amount` tokens; `payout_invoice` is where the sats should be paid.
    Sell {
        asset_id: String,
        amount: u64,
        payout_invoice: String,
    },
    /// Poll the status of a previously created order.
    OrderStatus { order_id: String },
    /// Prove payment of a buy order by revealing the LN preimage. The desk
    /// credits the tokens once `sha256(preimage) == order.payment_hash`.
    PaymentProof { order_id: String, preimage: String },
}

/// A message from a desk to a remote trader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeskResponse {
    /// Pay this invoice to complete the buy.
    Invoice {
        order_id: String,
        invoice: String,
        tokens: u64,
        cost_sats: u64,
        fee_sats: u64,
    },
    /// The order has been settled (credited / paid out).
    Filled {
        order_id: String,
        asset_id: String,
        side: String,
        tokens: u64,
        sats: u64,
    },
    /// The order exists but is not settled yet.
    Pending { order_id: String },
    /// The request could not be served.
    Error { message: String },
}

/// Lifecycle of a desk order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Invoice issued, awaiting the buyer's payment.
    AwaitingPayment,
    /// Paid and credited.
    Filled,
    /// Expired before it was paid.
    Expired,
}

/// A buy order held by the desk while it waits for the Lightning payment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub buyer_pubkey: String,
    pub asset_id: String,
    pub budget_sats: u64,
    pub tokens: u64,
    pub cost_sats: u64,
    pub fee_sats: u64,
    pub invoice: String,
    /// Hex payment hash, used to match the invoice against claimed receives.
    pub payment_hash: String,
    pub status: OrderStatus,
    pub created_at: u64,
}

impl Order {
    /// Whether an unpaid order has passed its payment window.
    pub fn is_expired(&self, now: u64) -> bool {
        self.status == OrderStatus::AwaitingPayment
            && now.saturating_sub(self.created_at) > ORDER_TTL_SECS
    }

    /// Verify a revealed LN preimage against this order's payment hash. The
    /// preimage can only be known by whoever paid the invoice, so this is a
    /// self-contained proof of payment — no need to poll the LN node.
    pub fn verify_preimage(&self, preimage_hex: &str) -> bool {
        let Ok(bytes) = hex::decode(preimage_hex) else {
            return false;
        };
        sha256::Hash::hash(&bytes).to_string() == self.payment_hash.to_lowercase()
    }

    /// The invoice response a buyer should receive for this order.
    pub fn invoice_response(&self) -> DeskResponse {
        DeskResponse::Invoice {
            order_id: self.order_id.clone(),
            invoice: self.invoice.clone(),
            tokens: self.tokens,
            cost_sats: self.cost_sats,
            fee_sats: self.fee_sats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_round_trip(r: &DeskRequest) {
        let s = serde_json::to_string(r).unwrap();
        assert_eq!(&serde_json::from_str::<DeskRequest>(&s).unwrap(), r);
    }

    fn resp_round_trip(r: &DeskResponse) {
        let s = serde_json::to_string(r).unwrap();
        assert_eq!(&serde_json::from_str::<DeskResponse>(&s).unwrap(), r);
    }

    #[test]
    fn requests_round_trip() {
        req_round_trip(&DeskRequest::BuyQuote {
            asset_id: "aa".into(),
            budget_sats: 10_000,
        });
        req_round_trip(&DeskRequest::Sell {
            asset_id: "aa".into(),
            amount: 100,
            payout_invoice: "lnbc1...".into(),
        });
        req_round_trip(&DeskRequest::OrderStatus {
            order_id: "o1".into(),
        });
    }

    #[test]
    fn responses_round_trip() {
        resp_round_trip(&DeskResponse::Invoice {
            order_id: "o1".into(),
            invoice: "lnbc1...".into(),
            tokens: 42,
            cost_sats: 100,
            fee_sats: 1,
        });
        resp_round_trip(&DeskResponse::Filled {
            order_id: "o1".into(),
            asset_id: "aa".into(),
            side: "buy".into(),
            tokens: 42,
            sats: 100,
        });
        resp_round_trip(&DeskResponse::Pending {
            order_id: "o1".into(),
        });
        resp_round_trip(&DeskResponse::Error {
            message: "nope".into(),
        });
    }

    #[test]
    fn request_uses_tagged_json() {
        let s = serde_json::to_string(&DeskRequest::BuyQuote {
            asset_id: "aa".into(),
            budget_sats: 5,
        })
        .unwrap();
        assert!(s.contains("\"kind\":\"buy_quote\""));
    }

    #[test]
    fn order_expiry_and_invoice_response() {
        let mut o = Order {
            order_id: "o1".into(),
            buyer_pubkey: "pk".into(),
            asset_id: "aa".into(),
            budget_sats: 100,
            tokens: 40,
            cost_sats: 99,
            fee_sats: 1,
            invoice: "lnbc1...".into(),
            payment_hash: "ab".into(),
            status: OrderStatus::AwaitingPayment,
            created_at: 1_000,
        };
        assert!(!o.is_expired(1_000 + ORDER_TTL_SECS));
        assert!(o.is_expired(1_001 + ORDER_TTL_SECS));
        // a filled order never "expires"
        o.status = OrderStatus::Filled;
        assert!(!o.is_expired(1_000_000));

        match o.invoice_response() {
            DeskResponse::Invoice {
                tokens, cost_sats, ..
            } => {
                assert_eq!(tokens, 40);
                assert_eq!(cost_sats, 99);
            }
            _ => panic!("expected invoice response"),
        }
    }

    #[test]
    fn preimage_proof_verifies() {
        let mut o = Order {
            order_id: "o1".into(),
            buyer_pubkey: "pk".into(),
            asset_id: "aa".into(),
            budget_sats: 100,
            tokens: 40,
            cost_sats: 99,
            fee_sats: 1,
            invoice: "lnbc1...".into(),
            // sha256(0x00)
            payment_hash: "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d".into(),
            status: OrderStatus::AwaitingPayment,
            created_at: 0,
        };
        assert!(o.verify_preimage("00"));
        assert!(!o.verify_preimage("01")); // wrong preimage
        assert!(!o.verify_preimage("zz")); // not hex
                                           // stored hash comparison is case-insensitive
        o.payment_hash = o.payment_hash.to_uppercase();
        assert!(o.verify_preimage("00"));
    }
}
