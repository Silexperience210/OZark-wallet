//! Bonding-curve marketplace for Taproot Assets.
//!
//! The market is split in layers: [`curve`] holds the pure pricing math (no
//! network, fully unit-tested), and later modules add the execution desk
//! (reserve accounting, ledger) and Nostr discovery. V1 wires only [`curve`].
//!
//! Design note — **rounding is asymmetric on purpose**: buys round the sat cost
//! *up*, sells round the sat refund *down*. Both directions therefore favour the
//! reserve, so integer rounding can never let the pool pay out more than it took
//! in. See [`curve::CurveParams`] for the details.

pub mod commands;
pub mod curve;
pub mod desk;
pub mod identity;
pub mod nostr_client;
pub mod store;

pub use desk::Desk;
