//! SQLite ownership registry — the heart of per-user isolation on a shared tapd.
//!
//! tapd itself has **no notion of per-user ownership**: any caller with the
//! macaroon can act on any asset. The gateway holds the macaroon and enforces
//! ownership here instead: at mint time it records `asset_id → owner_pubkey`, and
//! every scoped read (Phase 1) and every mutating action (send/burn, later phases)
//! is checked against this table. An asset with no row is owned by nobody and is
//! therefore invisible/untouchable through the gateway.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

pub struct Registry {
    // rusqlite's Connection is !Sync; serialize access behind a mutex. The gateway
    // is not write-heavy (one row per mint) so a single connection is plenty.
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("asset {0} is already registered to another owner")]
    AlreadyOwned(String),
}

impl Registry {
    /// Open (creating if needed) the registry at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory registry, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, RegistryError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, RegistryError> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS ownership (
                 asset_id     TEXT PRIMARY KEY,
                 owner_pubkey TEXT NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_ownership_owner
                 ON ownership(owner_pubkey);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record a freshly minted asset as owned by `owner_pubkey`.
    ///
    /// Idempotent for the *same* owner (re-recording is a no-op), but rejects an
    /// attempt to claim an asset already owned by someone else — a mint can only
    /// establish ownership once.
    // Phase 2: called when a mint succeeds. Tested now; wired to the mint route next.
    #[allow(dead_code)]
    pub fn record_mint(
        &self,
        asset_id: &str,
        owner_pubkey: &str,
        created_at: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.lock();
        if let Some(existing) = owner_of_conn(&conn, asset_id)? {
            return if existing == owner_pubkey {
                Ok(())
            } else {
                Err(RegistryError::AlreadyOwned(asset_id.to_string()))
            };
        }
        conn.execute(
            "INSERT INTO ownership (asset_id, owner_pubkey, created_at) VALUES (?1, ?2, ?3)",
            (asset_id, owner_pubkey, created_at),
        )?;
        Ok(())
    }

    /// Owner pubkey of an asset, if registered.
    #[allow(dead_code)]
    pub fn owner_of(&self, asset_id: &str) -> Result<Option<String>, RegistryError> {
        let conn = self.lock();
        owner_of_conn(&conn, asset_id)
    }

    /// True iff `pubkey` owns `asset_id`. Unregistered assets are owned by nobody.
    // Phase 2: the isolation check every send/burn goes through.
    #[allow(dead_code)]
    pub fn is_owner(&self, asset_id: &str, pubkey: &str) -> Result<bool, RegistryError> {
        Ok(self.owner_of(asset_id)?.as_deref() == Some(pubkey))
    }

    /// All asset ids owned by `pubkey` (for scoping the caller's asset list).
    pub fn assets_of(&self, pubkey: &str) -> Result<Vec<String>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT asset_id FROM ownership WHERE owner_pubkey = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([pubkey], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[allow(dead_code)] // reachable only from owner_of/record_mint (Phase 2) so far
fn owner_of_conn(conn: &Connection, asset_id: &str) -> Result<Option<String>, RegistryError> {
    let mut stmt = conn.prepare("SELECT owner_pubkey FROM ownership WHERE asset_id = ?1")?;
    let mut rows = stmt.query([asset_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "alice_pubkey_hex";
    const BOB: &str = "bob_pubkey_hex";
    const CAROL: &str = "carol_pubkey_hex";

    fn reg() -> Registry {
        Registry::open_in_memory().unwrap()
    }

    #[test]
    fn records_and_reads_owner() {
        let r = reg();
        r.record_mint("asset1", ALICE, 100).unwrap();
        assert_eq!(r.owner_of("asset1").unwrap().as_deref(), Some(ALICE));
        assert!(r.is_owner("asset1", ALICE).unwrap());
        assert!(!r.is_owner("asset1", BOB).unwrap());
    }

    #[test]
    fn unregistered_asset_owned_by_nobody() {
        let r = reg();
        assert_eq!(r.owner_of("ghost").unwrap(), None);
        assert!(!r.is_owner("ghost", ALICE).unwrap());
    }

    #[test]
    fn record_mint_is_idempotent_for_same_owner() {
        let r = reg();
        r.record_mint("asset1", ALICE, 100).unwrap();
        // Re-recording for the same owner is a no-op, not an error.
        r.record_mint("asset1", ALICE, 200).unwrap();
        assert_eq!(r.owner_of("asset1").unwrap().as_deref(), Some(ALICE));
    }

    #[test]
    fn rejects_reclaim_by_different_owner() {
        let r = reg();
        r.record_mint("asset1", ALICE, 100).unwrap();
        let err = r.record_mint("asset1", BOB, 200).unwrap_err();
        assert!(matches!(err, RegistryError::AlreadyOwned(_)));
        // Ownership is unchanged.
        assert_eq!(r.owner_of("asset1").unwrap().as_deref(), Some(ALICE));
    }

    #[test]
    fn assets_of_scopes_by_owner() {
        let r = reg();
        r.record_mint("a1", ALICE, 1).unwrap();
        r.record_mint("a2", ALICE, 2).unwrap();
        r.record_mint("b1", BOB, 3).unwrap();
        assert_eq!(r.assets_of(ALICE).unwrap(), vec!["a1", "a2"]);
        assert_eq!(r.assets_of(BOB).unwrap(), vec!["b1"]);
        assert!(r.assets_of(CAROL).unwrap().is_empty());
    }
}
