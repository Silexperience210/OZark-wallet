//! Versioned Argon2id key-derivation parameters.
//!
//! The wallet vault and encrypted backups both derive their AES key from the
//! user password with Argon2id. Historically both used a fixed 64 MiB memory
//! cost. To harden new material without locking out existing wallets/backups,
//! the parameter set is now *versioned*: the version is persisted next to the
//! salt (vault) or in the backup header (backup), so pre-existing data keeps
//! deriving with the exact parameters it was written with, while newly created
//! data uses the stronger set.
//!
//! Adding a version: append a new arm to [`Argon2Params::for_version`] and bump
//! [`KDF_VERSION_LATEST`]. Never change the values of an existing version — that
//! would silently make every wallet/backup written under it undecryptable.

use argon2::{Config, ThreadMode, Variant, Version};
use zeroize::Zeroizing;

/// KDF parameter version written for all newly-created material.
pub const KDF_VERSION_LATEST: u8 = 2;

/// Length of the derived key in bytes (AES-256 key).
const KEY_LEN: u32 = 32;

/// Argon2id cost parameters for a given KDF version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    /// Memory cost in KiB.
    pub mem_cost: u32,
    /// Number of iterations (time cost).
    pub time_cost: u32,
    /// Degree of parallelism (lanes).
    pub lanes: u32,
}

impl Argon2Params {
    /// Resolve the cost parameters for a persisted KDF version.
    ///
    /// Returns `None` for an unknown version, which means the material was
    /// written by a newer build than the one reading it (forward-incompatible).
    pub fn for_version(version: u8) -> Option<Self> {
        match version {
            // v1: the original parameters (64 MiB). Kept verbatim so wallets and
            // backups created before versioning still open. DO NOT edit.
            1 => Some(Self {
                mem_cost: 65_536,
                time_cost: 3,
                lanes: 4,
            }),
            // v2: hardened. 256 MiB memory cost (4x v1), same iterations/lanes.
            2 => Some(Self {
                mem_cost: 262_144,
                time_cost: 3,
                lanes: 4,
            }),
            _ => None,
        }
    }

    /// Derive a 32-byte key with these parameters.
    /// The key is wrapped in `Zeroizing` so it is wiped from memory on drop.
    pub fn derive(&self, password: &str, salt: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        let config = Config {
            variant: Variant::Argon2id,
            version: Version::Version13,
            mem_cost: self.mem_cost,
            time_cost: self.time_cost,
            lanes: self.lanes,
            thread_mode: ThreadMode::Parallel,
            secret: &[],
            ad: &[],
            hash_length: KEY_LEN,
        };
        argon2::hash_raw(password.as_bytes(), salt, &config)
            .map(Zeroizing::new)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_version_resolves() {
        assert!(Argon2Params::for_version(KDF_VERSION_LATEST).is_some());
    }

    #[test]
    fn v1_params_are_frozen() {
        // Regression guard: v1 must never change or every legacy vault breaks.
        let p = Argon2Params::for_version(1).unwrap();
        assert_eq!(p.mem_cost, 65_536);
        assert_eq!(p.time_cost, 3);
        assert_eq!(p.lanes, 4);
    }

    #[test]
    fn v2_is_hardened() {
        let p = Argon2Params::for_version(2).unwrap();
        assert_eq!(p.mem_cost, 262_144);
    }

    #[test]
    fn unknown_version_is_none() {
        assert!(Argon2Params::for_version(0).is_none());
        assert!(Argon2Params::for_version(255).is_none());
    }

    #[test]
    fn versions_derive_distinct_keys() {
        let salt = [7u8; 16];
        let k1 = Argon2Params::for_version(1)
            .unwrap()
            .derive("pw", &salt)
            .unwrap();
        let k2 = Argon2Params::for_version(2)
            .unwrap()
            .derive("pw", &salt)
            .unwrap();
        assert_eq!(k1.len(), 32);
        assert_eq!(k2.len(), 32);
        // Different memory cost => different derived key for the same input.
        assert_ne!(k1.to_vec(), k2.to_vec());
    }

    #[test]
    fn derive_is_deterministic() {
        let salt = [9u8; 16];
        let a = Argon2Params::for_version(1)
            .unwrap()
            .derive("pw", &salt)
            .unwrap();
        let b = Argon2Params::for_version(1)
            .unwrap()
            .derive("pw", &salt)
            .unwrap();
        assert_eq!(a.to_vec(), b.to_vec());
    }
}
