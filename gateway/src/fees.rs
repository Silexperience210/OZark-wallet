//! Fee policy: what a chargeable on-chain operation costs the user (sats),
//! debited from their custodial sats balance and credited to the operator. The
//! fee = max(floor, estimated network fee) + operator margin, so the operator's
//! node stays roughly cost-neutral (users cover their own on-chain cost) plus a
//! configurable markup.

use serde::Serialize;

/// A transparent fee breakdown, returned to the caller before an action.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FeeQuote {
    /// Estimated on-chain network fee (sats).
    pub network_sats: u64,
    /// Operator markup on top (sats).
    pub margin_sats: u64,
    /// What the user is actually charged (network + margin).
    pub total_sats: u64,
    /// Whether this node actually debits the fee. When false, `total_sats` is a
    /// display-only estimate and nothing is charged — the client should say so.
    pub charged: bool,
}

/// Fee parameters, derived from config.
#[derive(Debug, Clone, Copy)]
pub struct FeePolicy {
    /// Whether fees are actually debited. When false, `quote` still computes an
    /// estimate (for display) but the routes charge nothing.
    pub charge: bool,
    pub margin_bps: u64,
    pub floor_sats: u64,
    pub mint_vsize: u64,
    pub send_vsize: u64,
    pub default_rate: u32,
}

impl FeePolicy {
    /// Quote the fee for `op` (`"mint"` or `"send"`) at `fee_rate_sat_vb` (0 => the
    /// policy default). Network estimate = rate × assumed vsize, floored; margin =
    /// network × margin_bps / 10000.
    pub fn quote(&self, op: &str, fee_rate_sat_vb: u32) -> FeeQuote {
        let rate = if fee_rate_sat_vb == 0 {
            self.default_rate
        } else {
            fee_rate_sat_vb
        } as u64;
        let vsize = if op == "mint" {
            self.mint_vsize
        } else {
            self.send_vsize
        };
        let network = rate.saturating_mul(vsize).max(self.floor_sats);
        let margin = network.saturating_mul(self.margin_bps) / 10_000;
        FeeQuote {
            network_sats: network,
            margin_sats: margin,
            total_sats: network.saturating_add(margin),
            charged: self.charge,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> FeePolicy {
        FeePolicy {
            charge: true,
            margin_bps: 1000, // 10%
            floor_sats: 100,
            mint_vsize: 250,
            send_vsize: 200,
            default_rate: 5,
        }
    }

    #[test]
    fn quote_applies_margin_and_vsize() {
        // mint at 10 sat/vB: network = 10*250 = 2500, margin 10% = 250, total 2750.
        let q = policy().quote("mint", 10);
        assert_eq!(
            (q.network_sats, q.margin_sats, q.total_sats),
            (2500, 250, 2750)
        );
        // send at 10 sat/vB: network = 10*200 = 2000, margin 200, total 2200.
        let q = policy().quote("send", 10);
        assert_eq!(
            (q.network_sats, q.margin_sats, q.total_sats),
            (2000, 200, 2200)
        );
        assert!(q.charged); // policy() has charge = true

        // A node with fees off still quotes an estimate but flags it not charged.
        let off = FeePolicy {
            charge: false,
            ..policy()
        };
        let q = off.quote("send", 10);
        assert_eq!(q.total_sats, 2200);
        assert!(!q.charged);
    }

    #[test]
    fn quote_uses_default_rate_and_floor() {
        // rate 0 -> default 5; send network = 5*200 = 1000.
        let q = policy().quote("send", 0);
        assert_eq!(q.network_sats, 1000);
        // A tiny vsize/rate is floored at 100.
        let p = FeePolicy {
            mint_vsize: 1,
            default_rate: 1,
            ..policy()
        };
        let q = p.quote("mint", 1);
        assert_eq!(q.network_sats, 100); // floored
    }
}
