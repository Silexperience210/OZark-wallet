//! Nostr identity for the marketplace.
//!
//! The trader's Nostr key is derived **deterministically** from the wallet's
//! BIP-39 mnemonic via NIP-06 (`m/44'/1237'/0'/0/0`). The same seed that backs
//! the Bitcoin wallet therefore also backs the Nostr pubkey used to buy/sell and
//! to sign public market events — no extra key to manage or back up. This pubkey
//! replaces the placeholder local user id in the desk ledger.

use nostr::prelude::*;
use serde::Serialize;

/// Public summary of the local Nostr identity (safe to surface to the UI).
#[derive(Debug, Clone, Serialize)]
pub struct NostrIdentity {
    /// x-only public key, 64-hex — the ledger account id.
    pub pubkey_hex: String,
    /// bech32 `npub…` form for display.
    pub npub: String,
}

/// Derive the NIP-06 Nostr keypair from a BIP-39 mnemonic (no passphrase).
pub fn keys_from_mnemonic(mnemonic: &str) -> Result<Keys, String> {
    Keys::from_mnemonic(mnemonic, None).map_err(|e| e.to_string())
}

/// Public identity summary for a keypair.
pub fn identity(keys: &Keys) -> Result<NostrIdentity, String> {
    let pk = keys.public_key();
    Ok(NostrIdentity {
        pubkey_hex: pk.to_hex(),
        npub: pk.to_bech32().map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // NIP-06 official test vector #1.
    const MNEMONIC: &str =
        "leader monkey parrot ring guide accident before fringe cannon center slice narrow";
    const PRIV_HEX: &str = "7f7ff03d123792d6ac594bfa67bf6d0c0ab55b6b1fdb6249303fe861f1ccba9a";

    #[test]
    fn nip06_derivation_matches_spec() {
        let keys = keys_from_mnemonic(MNEMONIC).unwrap();
        assert_eq!(keys.secret_key().to_secret_hex(), PRIV_HEX);
    }

    #[test]
    fn derivation_is_deterministic_and_well_formed() {
        let a = keys_from_mnemonic(MNEMONIC).unwrap();
        let b = keys_from_mnemonic(MNEMONIC).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        let id = identity(&a).unwrap();
        assert_eq!(id.pubkey_hex.len(), 64);
        assert!(id.npub.starts_with("npub1"));
    }
}
