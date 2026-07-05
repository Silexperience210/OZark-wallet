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

use rusqlite::{Connection, OptionalExtension};

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

/// A mint that has been broadcast but whose asset id is not yet known on-chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMint {
    pub batch_key: String,
    pub batch_txid: String,
    pub owner_pubkey: String,
    pub name: String,
    pub amount: i64,
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
                 batch_key    TEXT NOT NULL DEFAULT '',
                 created_at   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_ownership_owner
                 ON ownership(owner_pubkey);
             CREATE INDEX IF NOT EXISTS idx_ownership_batch
                 ON ownership(batch_key);
             -- A mint is async: tapd returns a batch, the asset id only exists once
             -- the genesis is finalized on-chain. We hold the owner claim here,
             -- keyed by batch, until reconciliation resolves it to an asset id.
             CREATE TABLE IF NOT EXISTS pending_mints (
                 batch_key    TEXT PRIMARY KEY,
                 batch_txid   TEXT NOT NULL DEFAULT '',
                 owner_pubkey TEXT NOT NULL,
                 name         TEXT NOT NULL DEFAULT '',
                 amount       INTEGER NOT NULL DEFAULT 0,
                 created_at   INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Owner pubkey of an asset, if registered.
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

    /// Record a pending mint claim, keyed by the tapd batch. `batch_txid` may be
    /// empty when not yet known; reconciliation fills it in later. Idempotent for
    /// the same owner; rejects a different owner claiming the same batch.
    pub fn add_pending_mint(
        &self,
        batch_key: &str,
        batch_txid: &str,
        owner_pubkey: &str,
        name: &str,
        amount: i64,
        created_at: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.lock();
        let existing: Option<String> = conn
            .query_row(
                "SELECT owner_pubkey FROM pending_mints WHERE batch_key = ?1",
                [batch_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(owner) = existing {
            return if owner == owner_pubkey {
                Ok(())
            } else {
                Err(RegistryError::AlreadyOwned(batch_key.to_string()))
            };
        }
        conn.execute(
            "INSERT INTO pending_mints
                 (batch_key, batch_txid, owner_pubkey, name, amount, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                batch_key,
                batch_txid,
                owner_pubkey,
                name,
                amount,
                created_at,
            ),
        )?;
        Ok(())
    }

    /// All unresolved mint claims.
    pub fn pending_mints(&self) -> Result<Vec<PendingMint>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT batch_key, batch_txid, owner_pubkey, name, amount
             FROM pending_mints ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_pending)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// One pending claim by batch key.
    pub fn pending_mint(&self, batch_key: &str) -> Result<Option<PendingMint>, RegistryError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT batch_key, batch_txid, owner_pubkey, name, amount
             FROM pending_mints WHERE batch_key = ?1",
            [batch_key],
            row_to_pending,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Fill in the on-chain txid of a pending mint once it is known.
    pub fn set_pending_txid(&self, batch_key: &str, batch_txid: &str) -> Result<(), RegistryError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE pending_mints SET batch_txid = ?2 WHERE batch_key = ?1",
            (batch_key, batch_txid),
        )?;
        Ok(())
    }

    /// Resolve a pending mint to its final asset id: record ownership and drop the
    /// pending row, atomically. Returns whether a pending row was resolved (false
    /// if the batch was not, or no longer, pending).
    pub fn resolve_pending_mint(
        &self,
        batch_key: &str,
        asset_id: &str,
        created_at: i64,
    ) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let owner: Option<String> = tx
            .query_row(
                "SELECT owner_pubkey FROM pending_mints WHERE batch_key = ?1",
                [batch_key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        insert_ownership(&tx, asset_id, &owner, batch_key, created_at)?;
        tx.execute(
            "DELETE FROM pending_mints WHERE batch_key = ?1",
            [batch_key],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Asset id recorded for a given mint batch, if it has been reconciled.
    pub fn asset_by_batch_key(&self, batch_key: &str) -> Result<Option<String>, RegistryError> {
        if batch_key.is_empty() {
            return Ok(None);
        }
        let conn = self.lock();
        conn.query_row(
            "SELECT asset_id FROM ownership WHERE batch_key = ?1",
            [batch_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }
}

fn row_to_pending(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingMint> {
    Ok(PendingMint {
        batch_key: r.get(0)?,
        batch_txid: r.get(1)?,
        owner_pubkey: r.get(2)?,
        name: r.get(3)?,
        amount: r.get(4)?,
    })
}

fn owner_of_conn(conn: &Connection, asset_id: &str) -> Result<Option<String>, RegistryError> {
    let mut stmt = conn.prepare("SELECT owner_pubkey FROM ownership WHERE asset_id = ?1")?;
    let mut rows = stmt.query([asset_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Establish ownership of `asset_id`. Idempotent for the same owner; rejects an
/// asset already owned by someone else — a mint can only claim ownership once.
/// Runs on the caller's connection/transaction so it can be composed atomically.
fn insert_ownership(
    conn: &Connection,
    asset_id: &str,
    owner_pubkey: &str,
    batch_key: &str,
    created_at: i64,
) -> Result<(), RegistryError> {
    if let Some(existing) = owner_of_conn(conn, asset_id)? {
        return if existing == owner_pubkey {
            Ok(())
        } else {
            Err(RegistryError::AlreadyOwned(asset_id.to_string()))
        };
    }
    conn.execute(
        "INSERT INTO ownership (asset_id, owner_pubkey, batch_key, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        (asset_id, owner_pubkey, batch_key, created_at),
    )?;
    Ok(())
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

    /// Mint `asset` to `owner` through the real path: a pending claim resolved to
    /// the asset id (the batch key doubles as a unique claim id in tests).
    fn mint(r: &Registry, batch: &str, asset: &str, owner: &str, ts: i64) {
        r.add_pending_mint(batch, "", owner, "n", 1, ts).unwrap();
        r.resolve_pending_mint(batch, asset, ts).unwrap();
    }

    #[test]
    fn records_and_reads_owner() {
        let r = reg();
        mint(&r, "b1", "asset1", ALICE, 100);
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
    fn resolving_same_owner_again_is_idempotent() {
        let r = reg();
        mint(&r, "b1", "asset1", ALICE, 100);
        // A second claim resolving to the same asset for the same owner is a no-op.
        mint(&r, "b1", "asset1", ALICE, 200);
        assert_eq!(r.owner_of("asset1").unwrap().as_deref(), Some(ALICE));
    }

    #[test]
    fn rejects_reclaim_by_different_owner() {
        let r = reg();
        mint(&r, "b1", "asset1", ALICE, 100);
        // A different owner's claim resolving to the same asset id is rejected.
        r.add_pending_mint("b2", "", BOB, "n", 1, 200).unwrap();
        let err = r.resolve_pending_mint("b2", "asset1", 200).unwrap_err();
        assert!(matches!(err, RegistryError::AlreadyOwned(_)));
        // Ownership is unchanged.
        assert_eq!(r.owner_of("asset1").unwrap().as_deref(), Some(ALICE));
    }

    #[test]
    fn assets_of_scopes_by_owner() {
        let r = reg();
        mint(&r, "b1", "a1", ALICE, 1);
        mint(&r, "b2", "a2", ALICE, 2);
        mint(&r, "b3", "b1asset", BOB, 3);
        assert_eq!(r.assets_of(ALICE).unwrap(), vec!["a1", "a2"]);
        assert_eq!(r.assets_of(BOB).unwrap(), vec!["b1asset"]);
        assert!(r.assets_of(CAROL).unwrap().is_empty());
    }

    #[test]
    fn pending_mint_lifecycle() {
        let r = reg();
        // Broadcast with an unknown txid yet.
        r.add_pending_mint("batchA", "", ALICE, "OZK", 1000, 10)
            .unwrap();
        let p = r.pending_mint("batchA").unwrap().unwrap();
        assert_eq!(p.owner_pubkey, ALICE);
        assert_eq!(p.amount, 1000);
        assert_eq!(r.pending_mints().unwrap().len(), 1);

        // Reconciliation learns the txid, then the asset id.
        r.set_pending_txid("batchA", "txidA").unwrap();
        let resolved = r.resolve_pending_mint("batchA", "assetA", 20).unwrap();
        assert!(resolved);

        // Ownership is now recorded and the pending row is gone.
        assert_eq!(r.owner_of("assetA").unwrap().as_deref(), Some(ALICE));
        assert_eq!(
            r.asset_by_batch_key("batchA").unwrap().as_deref(),
            Some("assetA")
        );
        assert!(r.pending_mint("batchA").unwrap().is_none());
        assert!(r.assets_of(ALICE).unwrap().contains(&"assetA".to_string()));
    }

    #[test]
    fn add_pending_mint_rejects_different_owner() {
        let r = reg();
        r.add_pending_mint("batchA", "", ALICE, "OZK", 1, 10)
            .unwrap();
        // Same owner: idempotent.
        r.add_pending_mint("batchA", "", ALICE, "OZK", 1, 10)
            .unwrap();
        // Different owner claiming the same batch: rejected.
        let err = r
            .add_pending_mint("batchA", "", BOB, "OZK", 1, 10)
            .unwrap_err();
        assert!(matches!(err, RegistryError::AlreadyOwned(_)));
    }

    #[test]
    fn resolve_unknown_batch_is_noop() {
        let r = reg();
        assert!(!r.resolve_pending_mint("ghost", "assetX", 1).unwrap());
        assert_eq!(r.owner_of("assetX").unwrap(), None);
    }
}
