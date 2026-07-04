//! The bonding-curve **desk**: the money-accounting engine that sits on top of
//! [`super::curve`]. It owns the per-token reserve, the custodial ledger of who
//! holds what during the curve phase, the trade log (the OHLC source), and the
//! buy/sell execution that drives the curve.
//!
//! Like [`super::curve`] this layer is **pure and deterministic** — no clock, no
//! SQLite, no tapd. Callers pass timestamps in; persistence and asset movement
//! are adapters bolted on later. That keeps the invariants unit-testable in CI.
//!
//! # Invariants enforced here
//! - **No premine.** Supply only ever changes inside [`Market::buy`] /
//!   [`Market::sell`]. There is no API to hand out un-bought tokens, so every
//!   circulating token has sats behind it in the reserve. A "seed" at creation
//!   is just the creator's first buy.
//! - **Reserve solvency.** The reserve only ever receives/pays the *pure* curve
//!   amount (fees are charged on top, never out of it), so at all times
//!   `reserve_sats >= params.reserve_at(supply)` — enough to buy back every
//!   outstanding token. Verified by the `sequence_keeps_invariants` test.
//! - **Ledger conservation.** The sum of all user balances always equals the
//!   circulating supply.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::curve::{CurveError, CurveParams};

/// Hard cap on the creator fee: 10 %.
pub const MAX_FEE_BP: u16 = 1_000;

/// Errors from the desk. Curve math errors bubble up via [`DeskError::Curve`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeskError {
    #[error("market not found")]
    MarketNotFound,
    #[error("market already exists")]
    MarketExists,
    /// Buying is only allowed while `Trading`; selling also while `Migrated`.
    #[error("market is not accepting this operation")]
    NotTrading,
    #[error("creator fee exceeds the maximum")]
    FeeTooHigh,
    /// The requested trade rounds to zero tokens (budget too small, or the cap
    /// is reached).
    #[error("amount too small")]
    DustAmount,
    #[error("insufficient token balance")]
    InsufficientBalance,
    /// Should be unreachable given the solvency invariant — kept as a defensive
    /// guard so a logic bug surfaces instead of underflowing.
    #[error("reserve underflow")]
    ReserveUnderflow,
    #[error("curve error: {0}")]
    Curve(#[from] CurveError),
}

/// Whether a market is listed on the public marketplace or kept private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Private,
    Public,
}

/// Lifecycle of a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketStatus {
    /// Curve is live: buys and sells allowed.
    Trading,
    /// Creator-paused: no trading either way.
    Paused,
    /// Curve filled and graduated. Buys are closed; sells still allowed so
    /// holders can exit against the remaining reserve until the P2P order book
    /// (V3) takes over.
    Migrated,
}

/// Buy or sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

/// One executed trade — the atomic unit of the price history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    /// Unix seconds, supplied by the caller (keeps the engine deterministic).
    pub ts: u64,
    pub side: Side,
    pub user: String,
    pub tokens: u64,
    /// Reserve delta — the pure curve amount (excludes fee).
    pub sats: u64,
    pub fee_sats: u64,
    pub supply_after: u64,
    /// Average price of this trade, millisats per token.
    pub price_msat: u64,
}

/// Parameters for a new market.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSpec {
    /// Taproot asset id (hex), or a temporary id until the asset is minted.
    pub token_id: String,
    pub ticker: String,
    pub name: String,
    pub creator: String,
    pub params: CurveParams,
    pub visibility: Visibility,
    /// Creator fee in basis points, charged on top of the curve. `<= MAX_FEE_BP`.
    pub creator_fee_bp: u16,
    /// Optional first buy by the creator to bootstrap the reserve. `0` = pure
    /// fair launch (empty reserve, everyone buys from zero).
    pub seed_sats: u64,
}

/// Preview of a buy without executing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BuyPreview {
    pub tokens: u64,
    pub cost_sats: u64,
    pub fee_sats: u64,
    pub total_sats: u64,
    pub new_supply: u64,
    pub avg_price_msat: u64,
}

/// Preview of a sell without executing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SellPreview {
    pub tokens: u64,
    pub refund_sats: u64,
    pub fee_sats: u64,
    pub payout_sats: u64,
    pub new_supply: u64,
    pub avg_price_msat: u64,
}

/// A single token's market: curve, reserve, custodial ledger and trade history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub token_id: String,
    pub ticker: String,
    pub name: String,
    pub creator: String,
    pub params: CurveParams,
    pub visibility: Visibility,
    pub status: MarketStatus,
    pub supply: u64,
    pub reserve_sats: u64,
    /// Tokens that left custody (withdrawn on-chain or over a Lightning asset
    /// channel) but are still in circulation and still backed by the reserve.
    /// Tracked so the conservation invariant holds once custody can be exited:
    /// `sum(balances) + withdrawn == supply`.
    #[serde(default)]
    pub withdrawn: u64,
    pub creator_fee_bp: u16,
    pub creator_fees_sats: u64,
    pub created_at: u64,
    /// user id -> token units held via the desk.
    pub balances: HashMap<String, u64>,
    pub trades: Vec<Trade>,
}

impl Market {
    fn fee_on(&self, sats: u64) -> u64 {
        ((sats as u128) * (self.creator_fee_bp as u128) / 10_000) as u64
    }

    /// Current spot price, millisats per token.
    pub fn spot_price_msat(&self) -> Result<u64, CurveError> {
        self.params.spot_price_msat(self.supply)
    }

    /// Progress toward migration, in basis points.
    pub fn progress_bp(&self) -> u16 {
        self.params.progress_bp(self.supply)
    }

    /// Price a buy of as many tokens as `budget_sats` affords (fee included in
    /// the budget), without mutating state.
    pub fn preview_buy(&self, budget_sats: u64) -> Result<BuyPreview, DeskError> {
        if self.status != MarketStatus::Trading {
            return Err(DeskError::NotTrading);
        }
        // The fee rides on top of the curve cost, so the budget available to the
        // curve is B * 10000 / (10000 + fee_bp).
        let curve_budget =
            ((budget_sats as u128) * 10_000 / (10_000 + self.creator_fee_bp as u128)) as u64;
        let q = self.params.tokens_for_sats(self.supply, curve_budget)?;
        if q.tokens == 0 {
            return Err(DeskError::DustAmount);
        }
        let fee_sats = self.fee_on(q.cost_sats);
        Ok(BuyPreview {
            tokens: q.tokens,
            cost_sats: q.cost_sats,
            fee_sats,
            total_sats: q.cost_sats + fee_sats,
            new_supply: q.new_supply,
            avg_price_msat: q.avg_price_msat,
        })
    }

    /// Price a sell of `amount` tokens by `user`, without mutating state.
    pub fn preview_sell(&self, user: &str, amount: u64) -> Result<SellPreview, DeskError> {
        // Selling is allowed while Trading or Migrated, blocked only when Paused.
        if self.status == MarketStatus::Paused {
            return Err(DeskError::NotTrading);
        }
        if amount == 0 {
            return Err(DeskError::DustAmount);
        }
        let bal = self.balances.get(user).copied().unwrap_or(0);
        if bal < amount {
            return Err(DeskError::InsufficientBalance);
        }
        let q = self.params.quote_sell(self.supply, amount)?;
        let fee_sats = self.fee_on(q.refund_sats);
        Ok(SellPreview {
            tokens: amount,
            refund_sats: q.refund_sats,
            fee_sats,
            payout_sats: q.refund_sats - fee_sats,
            new_supply: q.new_supply,
            avg_price_msat: q.avg_price_msat,
        })
    }

    /// Execute a buy: `budget_sats` in, tokens credited to `user`.
    pub fn buy(&mut self, user: &str, budget_sats: u64, ts: u64) -> Result<Trade, DeskError> {
        let p = self.preview_buy(budget_sats)?;
        self.supply = p.new_supply;
        self.reserve_sats += p.cost_sats;
        self.creator_fees_sats += p.fee_sats;
        *self.balances.entry(user.to_string()).or_default() += p.tokens;
        let trade = Trade {
            ts,
            side: Side::Buy,
            user: user.to_string(),
            tokens: p.tokens,
            sats: p.cost_sats,
            fee_sats: p.fee_sats,
            supply_after: self.supply,
            price_msat: p.avg_price_msat,
        };
        self.trades.push(trade.clone());
        self.maybe_migrate();
        Ok(trade)
    }

    /// Execute a sell: `amount` tokens returned by `user`, sats paid out.
    pub fn sell(&mut self, user: &str, amount: u64, ts: u64) -> Result<Trade, DeskError> {
        let p = self.preview_sell(user, amount)?;
        self.reserve_sats = self
            .reserve_sats
            .checked_sub(p.refund_sats)
            .ok_or(DeskError::ReserveUnderflow)?;
        self.creator_fees_sats += p.fee_sats;
        let entry = self
            .balances
            .get_mut(user)
            .ok_or(DeskError::InsufficientBalance)?;
        *entry -= amount;
        if *entry == 0 {
            self.balances.remove(user);
        }
        self.supply -= amount;
        let trade = Trade {
            ts,
            side: Side::Sell,
            user: user.to_string(),
            tokens: amount,
            sats: p.refund_sats,
            fee_sats: p.fee_sats,
            supply_after: self.supply,
            price_msat: p.avg_price_msat,
        };
        self.trades.push(trade.clone());
        self.maybe_migrate();
        Ok(trade)
    }

    /// Move `amount` tokens out of `user`'s custodial balance so they can be
    /// withdrawn (on-chain via `send_asset`, or over a Lightning asset channel).
    ///
    /// The tokens stay **in circulation** and stay backed by the reserve — only
    /// custody changes — so `supply` and `reserve_sats` are untouched and the
    /// count shifts into `withdrawn`. This is transport-agnostic: the actual
    /// asset move is performed by the caller (tapd); this only updates the
    /// ledger, and should be called *after* the move succeeds.
    pub fn withdraw(&mut self, user: &str, amount: u64) -> Result<(), DeskError> {
        if amount == 0 {
            return Err(DeskError::DustAmount);
        }
        let bal = self.balances.get(user).copied().unwrap_or(0);
        if bal < amount {
            return Err(DeskError::InsufficientBalance);
        }
        let entry = self
            .balances
            .get_mut(user)
            .ok_or(DeskError::InsufficientBalance)?;
        *entry -= amount;
        if *entry == 0 {
            self.balances.remove(user);
        }
        self.withdrawn = self.withdrawn.saturating_add(amount);
        Ok(())
    }

    fn maybe_migrate(&mut self) {
        if self.status == MarketStatus::Trading && self.params.is_migratable(self.supply) {
            self.status = MarketStatus::Migrated;
        }
    }
}

/// The collection of all markets on this node's desk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Desk {
    pub markets: HashMap<String, Market>,
}

impl Desk {
    /// Register a new market. If `seed_sats > 0`, the creator's opening buy is
    /// executed immediately (funding the reserve — never a free premine).
    pub fn create_market(&mut self, spec: MarketSpec, ts: u64) -> Result<(), DeskError> {
        spec.params.validate()?;
        if spec.creator_fee_bp > MAX_FEE_BP {
            return Err(DeskError::FeeTooHigh);
        }
        let id = spec.token_id.clone();
        if self.markets.contains_key(&id) {
            return Err(DeskError::MarketExists);
        }
        let mut m = Market {
            token_id: id.clone(),
            ticker: spec.ticker,
            name: spec.name,
            creator: spec.creator.clone(),
            params: spec.params,
            visibility: spec.visibility,
            status: MarketStatus::Trading,
            supply: 0,
            reserve_sats: 0,
            withdrawn: 0,
            creator_fee_bp: spec.creator_fee_bp,
            creator_fees_sats: 0,
            created_at: ts,
            balances: HashMap::new(),
            trades: Vec::new(),
        };
        if spec.seed_sats > 0 {
            m.buy(&spec.creator, spec.seed_sats, ts)?;
        }
        self.markets.insert(id, m);
        Ok(())
    }

    pub fn market(&self, token_id: &str) -> Result<&Market, DeskError> {
        self.markets.get(token_id).ok_or(DeskError::MarketNotFound)
    }

    pub fn buy(
        &mut self,
        token_id: &str,
        user: &str,
        budget_sats: u64,
        ts: u64,
    ) -> Result<Trade, DeskError> {
        self.markets
            .get_mut(token_id)
            .ok_or(DeskError::MarketNotFound)?
            .buy(user, budget_sats, ts)
    }

    pub fn sell(
        &mut self,
        token_id: &str,
        user: &str,
        amount: u64,
        ts: u64,
    ) -> Result<Trade, DeskError> {
        self.markets
            .get_mut(token_id)
            .ok_or(DeskError::MarketNotFound)?
            .sell(user, amount, ts)
    }

    /// Move `amount` tokens of `token_id` out of `user`'s custodial balance.
    pub fn withdraw(&mut self, token_id: &str, user: &str, amount: u64) -> Result<(), DeskError> {
        self.markets
            .get_mut(token_id)
            .ok_or(DeskError::MarketNotFound)?
            .withdraw(user, amount)
    }

    /// Publicly listed markets only (the marketplace feed).
    pub fn public_markets(&self) -> Vec<&Market> {
        self.markets
            .values()
            .filter(|m| m.visibility == Visibility::Public)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> CurveParams {
        // price(s) = 1 + s sats/token, cap 1M, migration target very high so it
        // does not trigger mid-test unless a test overrides it.
        CurveParams::new(1, 1, 1, 1_000_000, 10_000_000_000).unwrap()
    }

    fn spec(fee: u16, seed: u64) -> MarketSpec {
        MarketSpec {
            token_id: "aa".into(),
            ticker: "OZ".into(),
            name: "OZark".into(),
            creator: "alice".into(),
            params: params(),
            visibility: Visibility::Public,
            creator_fee_bp: fee,
            seed_sats: seed,
        }
    }

    /// The two load-bearing invariants, checked after every op.
    fn check(m: &Market) {
        let held: u64 = m.balances.values().sum();
        assert_eq!(held + m.withdrawn, m.supply, "ledger + withdrawn != supply");
        assert!(
            m.reserve_sats >= m.params.reserve_at(m.supply).unwrap(),
            "reserve insolvent: {} < {}",
            m.reserve_sats,
            m.params.reserve_at(m.supply).unwrap()
        );
    }

    #[test]
    fn fair_launch_starts_empty() {
        let mut d = Desk::default();
        d.create_market(spec(0, 0), 0).unwrap();
        let m = d.market("aa").unwrap();
        assert_eq!(m.supply, 0);
        assert_eq!(m.reserve_sats, 0);
        assert!(m.balances.is_empty());
        assert!(m.trades.is_empty());
        check(m);
    }

    #[test]
    fn seed_is_first_buy_no_premine() {
        let mut d = Desk::default();
        d.create_market(spec(0, 10_000), 0).unwrap();
        let m = d.market("aa").unwrap();
        assert!(m.supply > 0);
        // The creator holds exactly what the seed bought — nothing minted free.
        assert_eq!(*m.balances.get("alice").unwrap(), m.supply);
        assert_eq!(m.trades.len(), 1);
        assert_eq!(m.reserve_sats, m.trades[0].sats);
        check(m);
    }

    #[test]
    fn duplicate_and_fee_guards() {
        let mut d = Desk::default();
        d.create_market(spec(0, 0), 0).unwrap();
        assert_eq!(d.create_market(spec(0, 0), 0), Err(DeskError::MarketExists));
        let mut d2 = Desk::default();
        assert_eq!(
            d2.create_market(spec(2_000, 0), 0),
            Err(DeskError::FeeTooHigh)
        );
    }

    #[test]
    fn buy_then_full_unwind_returns_reserve() {
        let mut d = Desk::default();
        d.create_market(spec(0, 0), 0).unwrap();
        d.buy("aa", "bob", 100_000, 1).unwrap();
        check(d.market("aa").unwrap());
        let held = *d.market("aa").unwrap().balances.get("bob").unwrap();
        d.sell("aa", "bob", held, 2).unwrap();
        let m = d.market("aa").unwrap();
        assert_eq!(m.supply, 0);
        assert!(m.balances.is_empty());
        check(m);
        // No fee: reserve returns to ~0 (only integer rounding dust remains).
        assert!(m.reserve_sats <= 1, "residual reserve {}", m.reserve_sats);
    }

    #[test]
    fn fee_rides_on_top_of_reserve() {
        let mut d = Desk::default();
        d.create_market(spec(100, 0), 0).unwrap(); // 1 %
        let t = d.buy("aa", "bob", 1_000_000, 1).unwrap();
        let m = d.market("aa").unwrap();
        assert!(t.fee_sats > 0);
        assert_eq!(m.creator_fees_sats, t.fee_sats);
        // Reserve receives only the pure curve cost, never the fee.
        assert_eq!(m.reserve_sats, t.sats);
        // Buyer never pays more than their budget.
        assert!(t.sats + t.fee_sats <= 1_000_000);
        check(m);
    }

    #[test]
    fn dust_and_balance_errors() {
        let mut d = Desk::default();
        d.create_market(spec(0, 0), 0).unwrap();
        assert_eq!(d.buy("aa", "bob", 0, 1), Err(DeskError::DustAmount));
        assert_eq!(
            d.sell("aa", "bob", 5, 1),
            Err(DeskError::InsufficientBalance)
        );
    }

    #[test]
    fn sequence_keeps_invariants() {
        let mut d = Desk::default();
        d.create_market(spec(50, 5_000), 0).unwrap();
        let ops: [(&str, &str, u64); 6] = [
            ("buy", "bob", 30_000),
            ("buy", "carol", 120_000),
            ("sell", "bob", 100),
            ("buy", "bob", 4_000),
            ("sell", "carol", 250),
            ("buy", "dave", 1),
        ];
        let mut ts = 1;
        for (side, user, amt) in ops {
            if side == "buy" {
                let _ = d.buy("aa", user, amt, ts);
            } else {
                let bal = d
                    .market("aa")
                    .unwrap()
                    .balances
                    .get(user)
                    .copied()
                    .unwrap_or(0);
                if bal > 0 {
                    let _ = d.sell("aa", user, amt.min(bal), ts);
                }
            }
            check(d.market("aa").unwrap());
            ts += 1;
        }
    }

    #[test]
    fn migration_freezes_buys_allows_exit() {
        let mut d = Desk::default();
        // Low migration target so one large buy trips it.
        let p = CurveParams::new(1, 1, 1, 1_000_000, 1_000).unwrap();
        d.create_market(
            MarketSpec {
                token_id: "m".into(),
                ticker: "M".into(),
                name: "M".into(),
                creator: "alice".into(),
                params: p,
                visibility: Visibility::Public,
                creator_fee_bp: 0,
                seed_sats: 0,
            },
            0,
        )
        .unwrap();
        d.buy("m", "bob", 1_000_000, 1).unwrap();
        assert_eq!(d.market("m").unwrap().status, MarketStatus::Migrated);
        // Buys are closed post-migration...
        assert_eq!(d.buy("m", "carol", 10_000, 2), Err(DeskError::NotTrading));
        // ...but holders can still exit.
        let bal = *d.market("m").unwrap().balances.get("bob").unwrap();
        assert!(d.sell("m", "bob", bal, 3).is_ok());
        check(d.market("m").unwrap());
    }

    #[test]
    fn pause_blocks_both_sides() {
        let mut d = Desk::default();
        d.create_market(spec(0, 0), 0).unwrap();
        d.buy("aa", "bob", 50_000, 1).unwrap();
        d.markets.get_mut("aa").unwrap().status = MarketStatus::Paused;
        assert_eq!(d.buy("aa", "bob", 1_000, 2), Err(DeskError::NotTrading));
        let bal = *d.market("aa").unwrap().balances.get("bob").unwrap();
        assert_eq!(d.sell("aa", "bob", bal, 3), Err(DeskError::NotTrading));
    }

    #[test]
    fn withdraw_leaves_custody_but_stays_circulating() {
        let mut d = Desk::default();
        d.create_market(spec(0, 0), 0).unwrap();
        d.buy("aa", "bob", 100_000, 1).unwrap();
        let before = d.market("aa").unwrap().clone();
        let held = *before.balances.get("bob").unwrap();
        let half = held / 2;

        d.withdraw("aa", "bob", half).unwrap();
        let m = d.market("aa").unwrap();
        // supply and reserve untouched — only custody changed
        assert_eq!(m.supply, before.supply);
        assert_eq!(m.reserve_sats, before.reserve_sats);
        assert_eq!(m.withdrawn, half);
        assert_eq!(*m.balances.get("bob").unwrap(), held - half);
        check(m); // sum(balances) + withdrawn == supply

        // cannot withdraw more than the remaining custodial balance
        assert_eq!(
            d.withdraw("aa", "bob", held),
            Err(DeskError::InsufficientBalance)
        );
    }
}
