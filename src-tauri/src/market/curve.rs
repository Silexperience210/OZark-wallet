//! Affine bonding curve: `price(supply) = (p0_num + k_num * supply) / denom`
//! sats per token, integrated exactly over integer supply.
//!
//! The curve is the single source of truth for how many sats a buy costs and how
//! many a sell refunds. Everything is exact integer/rational arithmetic — no
//! floats — so the same inputs always produce the same output on every device.
//!
//! # Why affine (and not constant-product yet)
//! V1 uses `p = p0 + k·s`, the simplest curve that (a) has a closed-form
//! integral and (b) starts at a non-degenerate floor. Constant-product with
//! virtual reserves (the exact pump.fun shape) is a future variant; it only
//! changes [`CurveParams::integral_frac`], not the public surface.
//!
//! # Rounding & reserve safety
//! The cost of moving supply from `s1` to `s2` is the exact rational
//! `Δ·(2·p0 + k·(s1+s2)) / (2·denom)`. Buys take the **ceiling** of that value
//! and sells take the **floor**, so a buy-then-immediate-sell of the same amount
//! can never refund more than it cost (the difference is at most 1 sat). This is
//! what keeps the reserve solvent under integer rounding.
//!
//! # The "starts at zero" edge
//! With `p0 = 0` the spot price at `supply = 0` is zero, but the ceiling on buys
//! means the very first token still costs at least 1 sat — the degenerate
//! "first buyer takes everything for free" case is structurally impossible.

use serde::{Deserialize, Serialize};

/// Errors returned by the pricing math. All are recoverable — the curve never
/// panics on hostile input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CurveError {
    /// Parameters violate an invariant (e.g. `denom == 0`, price identically 0,
    /// or `supply_cap == 0`).
    #[error("invalid curve parameters")]
    InvalidParams,
    /// An intermediate computation exceeded the 128-bit working range, or the
    /// final sat amount did not fit in `u64`.
    #[error("arithmetic overflow")]
    Overflow,
    /// A buy would push circulating supply past `supply_cap`.
    #[error("supply cap exceeded")]
    SupplyCapExceeded,
    /// A sell asked to burn more tokens than are in circulation.
    #[error("insufficient circulating supply")]
    InsufficientSupply,
}

/// Parameters of an affine bonding curve.
///
/// Price in sats per token at a given `supply` is `(p0_num + k_num*supply)/denom`.
/// `denom` lets the price be sub-satoshi (e.g. `denom = 1000` gives millisat
/// resolution on the base price) while keeping every field an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurveParams {
    /// Base price numerator — the price floor at `supply == 0`.
    pub p0_num: u64,
    /// Slope numerator — how fast the price rises per unit of supply.
    pub k_num: u64,
    /// Shared price denominator. Must be non-zero.
    pub denom: u64,
    /// Maximum circulating supply the curve will ever sell.
    pub supply_cap: u64,
    /// Reserve (in sats) that, once reached, marks the curve ready to migrate
    /// to a free-floating P2P order book. `0` disables migration.
    pub migration_sats: u64,
}

/// Result of pricing a buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BuyQuote {
    /// Token base units the buyer receives.
    pub tokens: u64,
    /// Sats the buyer pays (rounded up).
    pub cost_sats: u64,
    /// Circulating supply after the buy.
    pub new_supply: u64,
    /// Average price actually paid, in millisats per token.
    pub avg_price_msat: u64,
}

/// Result of pricing a sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SellQuote {
    /// Token base units the seller returns.
    pub tokens: u64,
    /// Sats the seller receives (rounded down).
    pub refund_sats: u64,
    /// Circulating supply after the sell.
    pub new_supply: u64,
    /// Average price actually received, in millisats per token.
    pub avg_price_msat: u64,
}

/// Average price in millisats per token, saturating instead of wrapping.
fn avg_price_msat(sats: u64, tokens: u64) -> u64 {
    if tokens == 0 {
        return 0;
    }
    let v = (sats as u128) * 1000 / (tokens as u128);
    u64::try_from(v).unwrap_or(u64::MAX)
}

impl CurveParams {
    /// Build and validate a curve. Prefer this over a struct literal so the
    /// invariants are checked once up front.
    pub fn new(
        p0_num: u64,
        k_num: u64,
        denom: u64,
        supply_cap: u64,
        migration_sats: u64,
    ) -> Result<Self, CurveError> {
        let c = Self {
            p0_num,
            k_num,
            denom,
            supply_cap,
            migration_sats,
        };
        c.validate()?;
        Ok(c)
    }

    /// Check the structural invariants. Called by [`CurveParams::new`]; call it
    /// again after deserializing untrusted parameters.
    pub fn validate(&self) -> Result<(), CurveError> {
        if self.denom == 0 {
            return Err(CurveError::InvalidParams);
        }
        // A curve that is flat at zero would hand out tokens for free.
        if self.p0_num == 0 && self.k_num == 0 {
            return Err(CurveError::InvalidParams);
        }
        if self.supply_cap == 0 {
            return Err(CurveError::InvalidParams);
        }
        Ok(())
    }

    /// Exact cost of the curve integrated over `[s1, s2]` (assumes `s2 >= s1`),
    /// returned as an un-reduced fraction `(numerator, denominator)` so the
    /// caller can pick the rounding direction.
    ///
    /// `∫ (p0 + k·s) ds = Δ·(2·p0 + k·(s1+s2)) / (2·denom)`.
    fn integral_frac(&self, s1: u64, s2: u64) -> Result<(u128, u128), CurveError> {
        if self.denom == 0 {
            return Err(CurveError::InvalidParams);
        }
        debug_assert!(s2 >= s1);
        let sum = (s1 as u128) + (s2 as u128);
        let delta = (s2 - s1) as u128;
        let ks = (self.k_num as u128)
            .checked_mul(sum)
            .ok_or(CurveError::Overflow)?;
        let two_p0 = 2u128
            .checked_mul(self.p0_num as u128)
            .ok_or(CurveError::Overflow)?;
        let bracket = two_p0.checked_add(ks).ok_or(CurveError::Overflow)?;
        let numer = delta.checked_mul(bracket).ok_or(CurveError::Overflow)?;
        let den = 2u128 * (self.denom as u128);
        Ok((numer, den))
    }

    /// Sats required to buy `amount` tokens starting from `supply` (rounded up).
    pub fn cost_to_buy(&self, supply: u64, amount: u64) -> Result<u64, CurveError> {
        if amount == 0 {
            return Ok(0);
        }
        let s2 = supply.checked_add(amount).ok_or(CurveError::Overflow)?;
        if s2 > self.supply_cap {
            return Err(CurveError::SupplyCapExceeded);
        }
        let (numer, den) = self.integral_frac(supply, s2)?;
        // den = 2*denom > 0 (validated in integral_frac), so div_ceil is safe.
        u64::try_from(numer.div_ceil(den)).map_err(|_| CurveError::Overflow)
    }

    /// Sats refunded for selling `amount` tokens from `supply` (rounded down).
    pub fn refund_to_sell(&self, supply: u64, amount: u64) -> Result<u64, CurveError> {
        if amount == 0 {
            return Ok(0);
        }
        if amount > supply {
            return Err(CurveError::InsufficientSupply);
        }
        let s1 = supply - amount;
        let (numer, den) = self.integral_frac(s1, supply)?;
        u64::try_from(numer / den).map_err(|_| CurveError::Overflow)
    }

    /// Price a fixed-size buy of `amount` tokens.
    pub fn quote_buy(&self, supply: u64, amount: u64) -> Result<BuyQuote, CurveError> {
        let cost_sats = self.cost_to_buy(supply, amount)?;
        let avg = if amount == 0 {
            self.spot_price_msat(supply)?
        } else {
            avg_price_msat(cost_sats, amount)
        };
        Ok(BuyQuote {
            tokens: amount,
            cost_sats,
            new_supply: supply + amount,
            avg_price_msat: avg,
        })
    }

    /// Price a fixed-size sell of `amount` tokens.
    pub fn quote_sell(&self, supply: u64, amount: u64) -> Result<SellQuote, CurveError> {
        let refund_sats = self.refund_to_sell(supply, amount)?;
        let avg = if amount == 0 {
            self.spot_price_msat(supply)?
        } else {
            avg_price_msat(refund_sats, amount)
        };
        Ok(SellQuote {
            tokens: amount,
            refund_sats,
            new_supply: supply - amount,
            avg_price_msat: avg,
        })
    }

    /// Largest whole-token buy affordable with `budget_sats` starting from
    /// `supply`. Solved by binary search on the (strictly increasing) cost
    /// function, so the result is the maximal `tokens` with
    /// `cost_to_buy(supply, tokens) <= budget_sats`.
    pub fn tokens_for_sats(&self, supply: u64, budget_sats: u64) -> Result<BuyQuote, CurveError> {
        let max_delta = self.supply_cap.saturating_sub(supply);
        if budget_sats == 0 || max_delta == 0 {
            return Ok(BuyQuote {
                tokens: 0,
                cost_sats: 0,
                new_supply: supply,
                avg_price_msat: self.spot_price_msat(supply.min(self.supply_cap))?,
            });
        }
        let mut lo: u64 = 0;
        let mut hi: u64 = max_delta;
        while lo < hi {
            // Round the midpoint up (overflow-safe) to converge toward the max.
            let mid = lo + (hi - lo).div_ceil(2);
            match self.cost_to_buy(supply, mid) {
                Ok(c) if c <= budget_sats => lo = mid,
                _ => hi = mid - 1,
            }
        }
        let tokens = lo;
        let cost_sats = self.cost_to_buy(supply, tokens)?;
        let avg = if tokens == 0 {
            self.spot_price_msat(supply)?
        } else {
            avg_price_msat(cost_sats, tokens)
        };
        Ok(BuyQuote {
            tokens,
            cost_sats,
            new_supply: supply + tokens,
            avg_price_msat: avg,
        })
    }

    /// Spot (marginal) price at `supply`, in millisats per token.
    pub fn spot_price_msat(&self, supply: u64) -> Result<u64, CurveError> {
        if self.denom == 0 {
            return Err(CurveError::InvalidParams);
        }
        let ks = (self.k_num as u128)
            .checked_mul(supply as u128)
            .ok_or(CurveError::Overflow)?;
        let per_token = (self.p0_num as u128)
            .checked_add(ks)
            .ok_or(CurveError::Overflow)?;
        let msat = per_token.checked_mul(1000).ok_or(CurveError::Overflow)?;
        let denom = self.denom as u128;
        let rounded = (msat + denom / 2) / denom;
        u64::try_from(rounded).map_err(|_| CurveError::Overflow)
    }

    /// Theoretical reserve backing a circulating `supply` — the floor of the
    /// area under the curve from `0`. Actual reserve is `>=` this (buys round
    /// up), so this is a safe lower bound for progress/UI.
    pub fn reserve_at(&self, supply: u64) -> Result<u64, CurveError> {
        if supply > self.supply_cap {
            return Err(CurveError::SupplyCapExceeded);
        }
        let (numer, den) = self.integral_frac(0, supply)?;
        u64::try_from(numer / den).map_err(|_| CurveError::Overflow)
    }

    /// Progress toward migration in basis points (0..=10_000). `0` when
    /// migration is disabled (`migration_sats == 0`).
    pub fn progress_bp(&self, supply: u64) -> u16 {
        if self.migration_sats == 0 {
            return 0;
        }
        let reserve = self.reserve_at(supply).unwrap_or(0) as u128;
        let bp = reserve.saturating_mul(10_000) / (self.migration_sats as u128);
        bp.min(10_000) as u16
    }

    /// Whether the curve has accumulated enough reserve to graduate to a
    /// free-floating market. Always `false` when migration is disabled.
    pub fn is_migratable(&self, supply: u64) -> bool {
        if self.migration_sats == 0 {
            return false;
        }
        self.reserve_at(supply)
            .map(|r| r >= self.migration_sats)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `price(s) = s` sats/token: p0=0, k=1, denom=1. Easy to hand-check.
    fn lin() -> CurveParams {
        CurveParams::new(0, 1, 1, 1_000_000, 500_000).unwrap()
    }

    #[test]
    fn validate_rejects_bad_params() {
        assert_eq!(
            CurveParams::new(1, 1, 0, 100, 0),
            Err(CurveError::InvalidParams)
        );
        assert_eq!(
            CurveParams::new(0, 0, 1, 100, 0),
            Err(CurveError::InvalidParams)
        );
        assert_eq!(
            CurveParams::new(1, 1, 1, 0, 0),
            Err(CurveError::InvalidParams)
        );
        assert!(CurveParams::new(1, 1, 1, 100, 0).is_ok());
    }

    #[test]
    fn cost_matches_integral() {
        let c = lin();
        // ∫0..10 s ds = 50
        assert_eq!(c.cost_to_buy(0, 10), Ok(50));
        // ∫0..3 s ds = 4.5 -> ceil 5
        assert_eq!(c.cost_to_buy(0, 3), Ok(5));
        // ∫10..20 s ds = 150 (costs more at higher supply -> monotone)
        assert_eq!(c.cost_to_buy(10, 10), Ok(150));
        assert!(c.cost_to_buy(10, 10).unwrap() > c.cost_to_buy(0, 10).unwrap());
    }

    #[test]
    fn refund_floors_and_never_exceeds_cost() {
        let c = lin();
        // ∫10..20 = 150 exactly, floor == ceil here
        assert_eq!(c.refund_to_sell(20, 10), Ok(150));
        // ∫0..3 = 4.5 -> floor 4, while the buy cost was 5
        assert_eq!(c.refund_to_sell(3, 3), Ok(4));
        assert_eq!(c.cost_to_buy(0, 3), Ok(5));
    }

    #[test]
    fn roundtrip_keeps_reserve_solvent() {
        let c = lin();
        for &(s, a) in &[(0, 1), (0, 7), (5, 3), (100, 50), (999, 1), (12_345, 678)] {
            let cost = c.cost_to_buy(s, a).unwrap();
            // sell the same amount straight back from the new supply
            let refund = c.refund_to_sell(s + a, a).unwrap();
            assert!(
                refund <= cost,
                "refund {refund} > cost {cost} at s={s} a={a}"
            );
            assert!(cost - refund <= 1, "rounding gap >1 at s={s} a={a}");
        }
    }

    #[test]
    fn tokens_for_sats_is_maximal() {
        let c = lin();
        // cost(0,10)=50, cost(0,11)=ceil(60.5)=61
        assert_eq!(c.tokens_for_sats(0, 50).unwrap().tokens, 10);
        assert_eq!(c.tokens_for_sats(0, 60).unwrap().tokens, 10);
        assert_eq!(c.tokens_for_sats(0, 61).unwrap().tokens, 11);

        // boundary property over a range of budgets
        for budget in [1u64, 5, 42, 500, 5_000, 123_456] {
            let q = c.tokens_for_sats(0, budget).unwrap();
            assert!(q.cost_sats <= budget);
            if q.tokens < c.supply_cap {
                assert!(
                    c.cost_to_buy(0, q.tokens + 1).unwrap() > budget,
                    "not maximal at budget {budget}"
                );
            }
        }
    }

    #[test]
    fn tokens_for_sats_edges() {
        let c = lin();
        assert_eq!(c.tokens_for_sats(0, 0).unwrap().tokens, 0);
        // supply already at cap -> nothing buyable
        assert_eq!(c.tokens_for_sats(c.supply_cap, 1_000).unwrap().tokens, 0);
    }

    #[test]
    fn supply_cap_and_underflow_are_errors() {
        let c = CurveParams::new(0, 1, 1, 100, 0).unwrap();
        assert_eq!(c.cost_to_buy(95, 10), Err(CurveError::SupplyCapExceeded));
        assert_eq!(c.refund_to_sell(5, 10), Err(CurveError::InsufficientSupply));
    }

    #[test]
    fn first_token_costs_at_least_one_sat() {
        // Ultra-cheap curve: p0=0, k=1, denom=1_000_000 -> spot price ~0 at s=0.
        let c = CurveParams::new(0, 1, 1_000_000, u64::MAX, 0).unwrap();
        assert_eq!(c.spot_price_msat(0), Ok(0));
        // Ceil rounding still charges 1 sat for the first token.
        assert_eq!(c.cost_to_buy(0, 1), Ok(1));
    }

    #[test]
    fn spot_price_tracks_the_line() {
        // price(s) = (1000 + 1*s)/1000 sats -> base 1 sat, +1 msat per unit.
        let c = CurveParams::new(1000, 1, 1000, u64::MAX, 0).unwrap();
        assert_eq!(c.spot_price_msat(0), Ok(1000)); // (1000 + 0)/1000 sat = 1 sat
        assert_eq!(c.spot_price_msat(500), Ok(1500)); // (1000 + 500)/1000 sat = 1.5 sat
    }

    #[test]
    fn overflow_is_reported_not_panicked() {
        let c = CurveParams::new(0, u64::MAX, 1, u64::MAX, 0).unwrap();
        // k * (s1+s2) blows past u128 -> Overflow, no panic.
        assert_eq!(
            c.cost_to_buy(u64::MAX / 2, u64::MAX / 2 - 1),
            Err(CurveError::Overflow)
        );
    }

    #[test]
    fn migration_progress() {
        let c = lin(); // migration_sats = 500_000
                       // reserve_at(1000) = floor(∫0..1000 s ds) = floor(500_000) = 500_000
        assert_eq!(c.reserve_at(1000), Ok(500_000));
        assert_eq!(c.progress_bp(1000), 10_000);
        assert!(c.is_migratable(1000));
        // reserve_at(500) = floor(125_000) -> 2500 bp of the 500_000 target
        assert_eq!(c.reserve_at(500), Ok(125_000));
        assert_eq!(c.progress_bp(500), 2_500);
        assert!(!c.is_migratable(500));
    }

    #[test]
    fn migration_disabled_when_zero() {
        let c = CurveParams::new(1, 1, 1, 1_000, 0).unwrap();
        assert_eq!(c.progress_bp(500), 0);
        assert!(!c.is_migratable(500));
    }

    #[test]
    fn quotes_carry_average_price() {
        let c = lin();
        let b = c.quote_buy(0, 10).unwrap();
        assert_eq!(b.tokens, 10);
        assert_eq!(b.cost_sats, 50);
        assert_eq!(b.new_supply, 10);
        // avg = 50 sats / 10 tokens = 5 sat = 5000 msat
        assert_eq!(b.avg_price_msat, 5_000);

        let s = c.quote_sell(20, 10).unwrap();
        assert_eq!(s.refund_sats, 150);
        assert_eq!(s.new_supply, 10);
        assert_eq!(s.avg_price_msat, 15_000);
    }
}
