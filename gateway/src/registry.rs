//! Custodial balance ledger — per-user isolation on a shared tapd.
//!
//! tapd itself has **no notion of per-user ownership**: any caller with the
//! macaroon can act on any asset. The gateway holds the macaroon and tracks who
//! owns what here instead, as a balance ledger `(asset_id, pubkey) → amount`. tapd
//! holds the real assets; this ledger records each user's share. Every mutating
//! action (send/burn/transfer) checks and debits the caller's balance, so no user
//! can touch another's holdings. Internal transfers between two gateway users are a
//! pure ledger move — instant and free, no on-chain transaction.
//!
//! Invariant (per asset): the sum of ledger balances never exceeds tapd's actual
//! holding; credits happen only on confirmed mint/receive, debits only on
//! send/burn (with a refund if the tapd call fails).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

pub struct Registry {
    // rusqlite's Connection is !Sync; serialize access behind a mutex. The gateway
    // is not write-heavy so a single connection is plenty.
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("insufficient balance: {pubkey} holds less than {amount} of {asset_id}")]
    InsufficientBalance {
        asset_id: String,
        pubkey: String,
        amount: u64,
    },
    #[error("mint batch {0} is already claimed by another owner")]
    BatchClaimed(String),
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

/// A receive address awaiting an incoming transfer to credit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReceive {
    pub addr: String,
    pub asset_id: String,
    pub pubkey: String,
    pub amount: u64,
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
             -- Per-user balance of each asset. tapd holds the real assets; this is
             -- the custodial accounting of who owns which share.
             CREATE TABLE IF NOT EXISTS balances (
                 asset_id TEXT NOT NULL,
                 pubkey   TEXT NOT NULL,
                 amount   INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (asset_id, pubkey)
             );
             CREATE INDEX IF NOT EXISTS idx_balances_pubkey ON balances(pubkey);
             -- A mint is async: tapd returns a batch, the asset id only exists once
             -- the genesis confirms. Hold the owner claim here until reconciliation.
             CREATE TABLE IF NOT EXISTS pending_mints (
                 batch_key    TEXT PRIMARY KEY,
                 batch_txid   TEXT NOT NULL DEFAULT '',
                 owner_pubkey TEXT NOT NULL,
                 name         TEXT NOT NULL DEFAULT '',
                 amount       INTEGER NOT NULL DEFAULT 0,
                 created_at   INTEGER NOT NULL
             );
             -- Resolved mints (batch -> asset id), for status lookup and audit.
             CREATE TABLE IF NOT EXISTS mints (
                 batch_key    TEXT PRIMARY KEY,
                 asset_id     TEXT NOT NULL,
                 owner_pubkey TEXT NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             -- Receive addresses awaiting an incoming transfer, to credit on confirm.
             CREATE TABLE IF NOT EXISTS pending_receives (
                 addr       TEXT PRIMARY KEY,
                 asset_id   TEXT NOT NULL,
                 pubkey     TEXT NOT NULL,
                 amount     INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- Balances ---------------------------------------------------------

    /// Units of `asset_id` held by `pubkey` (0 if none).
    pub fn balance_of(&self, asset_id: &str, pubkey: &str) -> Result<u64, RegistryError> {
        let conn = self.lock();
        balance_conn(&conn, asset_id, pubkey)
    }

    /// Every non-zero holding of `pubkey`, as `(asset_id, amount)`.
    pub fn holdings(&self, pubkey: &str) -> Result<Vec<(String, u64)>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT asset_id, amount FROM balances
             WHERE pubkey = ?1 AND amount > 0 ORDER BY asset_id",
        )?;
        let rows = stmt.query_map([pubkey], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Credit `amount` of `asset_id` to `pubkey`. Used on confirmed mint/receive,
    /// and to refund a failed send/burn.
    pub fn credit(&self, asset_id: &str, pubkey: &str, amount: u64) -> Result<(), RegistryError> {
        let conn = self.lock();
        credit_conn(&conn, asset_id, pubkey, amount)
    }

    /// Debit `amount` of `asset_id` from `pubkey`, or error if the balance is too
    /// low. Used before a send/burn (reserve-then-act, refund on failure).
    pub fn debit(&self, asset_id: &str, pubkey: &str, amount: u64) -> Result<(), RegistryError> {
        let conn = self.lock();
        debit_conn(&conn, asset_id, pubkey, amount)
    }

    /// Instant internal transfer: debit `from`, credit `to`, atomically. No tapd
    /// transaction — a pure ledger move between two gateway users.
    pub fn transfer(
        &self,
        asset_id: &str,
        from: &str,
        to: &str,
        amount: u64,
    ) -> Result<(), RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        debit_conn(&tx, asset_id, from, amount)?;
        credit_conn(&tx, asset_id, to, amount)?;
        tx.commit()?;
        Ok(())
    }

    // ---- Pending mints ----------------------------------------------------

    /// Record a pending mint claim, keyed by the tapd batch. `batch_txid` may be
    /// empty when not yet known; reconciliation fills it in. Idempotent for the
    /// same owner; rejects a different owner claiming the same batch.
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
                Err(RegistryError::BatchClaimed(batch_key.to_string()))
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
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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

    /// Resolve a pending mint to its asset id: credit the minter the full minted
    /// amount, record the mint, and drop the pending row — atomically. Returns
    /// whether a pending row was resolved.
    pub fn resolve_pending_mint(
        &self,
        batch_key: &str,
        asset_id: &str,
        created_at: i64,
    ) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner_pubkey, amount FROM pending_mints WHERE batch_key = ?1",
                [batch_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((owner, amount)) = row else {
            return Ok(false);
        };
        credit_conn(&tx, asset_id, &owner, amount.max(0) as u64)?;
        tx.execute(
            "INSERT OR IGNORE INTO mints (batch_key, asset_id, owner_pubkey, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            (batch_key, asset_id, &owner, created_at),
        )?;
        tx.execute(
            "DELETE FROM pending_mints WHERE batch_key = ?1",
            [batch_key],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Asset id a resolved mint produced, with its owner (for status lookup).
    pub fn mint_result(&self, batch_key: &str) -> Result<Option<(String, String)>, RegistryError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT asset_id, owner_pubkey FROM mints WHERE batch_key = ?1",
            [batch_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- Pending receives -------------------------------------------------

    /// Record a receive address awaiting an incoming transfer for `pubkey`.
    pub fn add_pending_receive(
        &self,
        addr: &str,
        asset_id: &str,
        pubkey: &str,
        amount: u64,
        created_at: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO pending_receives
                 (addr, asset_id, pubkey, amount, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (addr, asset_id, pubkey, amount as i64, created_at),
        )?;
        Ok(())
    }

    /// All receive addresses still awaiting funds.
    pub fn pending_receives(&self) -> Result<Vec<PendingReceive>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT addr, asset_id, pubkey, amount FROM pending_receives ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingReceive {
                addr: r.get(0)?,
                asset_id: r.get(1)?,
                pubkey: r.get(2)?,
                amount: r.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Credit a confirmed receive to its recipient and drop the pending row,
    /// atomically. Returns whether a pending receive was resolved.
    pub fn resolve_receive(&self, addr: &str) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT asset_id, pubkey, amount FROM pending_receives WHERE addr = ?1",
                [addr],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((asset_id, pubkey, amount)) = row else {
            return Ok(false);
        };
        credit_conn(&tx, &asset_id, &pubkey, amount.max(0) as u64)?;
        tx.execute("DELETE FROM pending_receives WHERE addr = ?1", [addr])?;
        tx.commit()?;
        Ok(true)
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

fn balance_conn(conn: &Connection, asset_id: &str, pubkey: &str) -> Result<u64, RegistryError> {
    let amount: Option<i64> = conn
        .query_row(
            "SELECT amount FROM balances WHERE asset_id = ?1 AND pubkey = ?2",
            (asset_id, pubkey),
            |r| r.get(0),
        )
        .optional()?;
    Ok(amount.unwrap_or(0).max(0) as u64)
}

fn credit_conn(
    conn: &Connection,
    asset_id: &str,
    pubkey: &str,
    amount: u64,
) -> Result<(), RegistryError> {
    conn.execute(
        "INSERT INTO balances (asset_id, pubkey, amount) VALUES (?1, ?2, ?3)
         ON CONFLICT(asset_id, pubkey) DO UPDATE SET amount = amount + excluded.amount",
        (asset_id, pubkey, amount as i64),
    )?;
    Ok(())
}

fn debit_conn(
    conn: &Connection,
    asset_id: &str,
    pubkey: &str,
    amount: u64,
) -> Result<(), RegistryError> {
    let current = balance_conn(conn, asset_id, pubkey)?;
    if current < amount {
        return Err(RegistryError::InsufficientBalance {
            asset_id: asset_id.to_string(),
            pubkey: pubkey.to_string(),
            amount,
        });
    }
    conn.execute(
        "UPDATE balances SET amount = amount - ?3 WHERE asset_id = ?1 AND pubkey = ?2",
        (asset_id, pubkey, amount as i64),
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

    #[test]
    fn credit_accumulates_and_reads_back() {
        let r = reg();
        r.credit("X", ALICE, 100).unwrap();
        r.credit("X", ALICE, 50).unwrap();
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 150);
        assert_eq!(r.balance_of("X", BOB).unwrap(), 0);
    }

    #[test]
    fn debit_checks_sufficiency() {
        let r = reg();
        r.credit("X", ALICE, 100).unwrap();
        r.debit("X", ALICE, 40).unwrap();
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 60);
        let err = r.debit("X", ALICE, 1000).unwrap_err();
        assert!(matches!(err, RegistryError::InsufficientBalance { .. }));
        // Balance unchanged after a rejected debit.
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 60);
    }

    #[test]
    fn transfer_moves_balance_atomically() {
        let r = reg();
        r.credit("X", ALICE, 100).unwrap();
        r.transfer("X", ALICE, BOB, 30).unwrap();
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 70);
        assert_eq!(r.balance_of("X", BOB).unwrap(), 30);
    }

    #[test]
    fn transfer_rejects_insufficient_and_leaves_balances() {
        let r = reg();
        r.credit("X", ALICE, 10).unwrap();
        let err = r.transfer("X", ALICE, BOB, 50).unwrap_err();
        assert!(matches!(err, RegistryError::InsufficientBalance { .. }));
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 10);
        assert_eq!(r.balance_of("X", BOB).unwrap(), 0);
    }

    #[test]
    fn holdings_list_nonzero_only() {
        let r = reg();
        r.credit("a1", ALICE, 5).unwrap();
        r.credit("a2", ALICE, 7).unwrap();
        r.credit("a3", ALICE, 1).unwrap();
        r.debit("a3", ALICE, 1).unwrap(); // back to zero -> excluded
        r.credit("b1", BOB, 9).unwrap();
        assert_eq!(
            r.holdings(ALICE).unwrap(),
            vec![("a1".to_string(), 5), ("a2".to_string(), 7)]
        );
        assert!(r.holdings(CAROL).unwrap().is_empty());
    }

    #[test]
    fn mint_resolves_and_credits_full_amount() {
        let r = reg();
        r.add_pending_mint("batchA", "", ALICE, "OZK", 1000, 10)
            .unwrap();
        assert_eq!(r.pending_mints().unwrap().len(), 1);
        r.set_pending_txid("batchA", "txidA").unwrap();
        assert!(r.resolve_pending_mint("batchA", "assetA", 20).unwrap());
        assert_eq!(r.balance_of("assetA", ALICE).unwrap(), 1000);
        assert!(r.pending_mint("batchA").unwrap().is_none());
        assert_eq!(
            r.mint_result("batchA").unwrap(),
            Some(("assetA".to_string(), ALICE.to_string()))
        );
    }

    #[test]
    fn add_pending_mint_rejects_different_owner() {
        let r = reg();
        r.add_pending_mint("batchA", "", ALICE, "OZK", 1, 10)
            .unwrap();
        r.add_pending_mint("batchA", "", ALICE, "OZK", 1, 10)
            .unwrap(); // idempotent
        let err = r
            .add_pending_mint("batchA", "", BOB, "OZK", 1, 10)
            .unwrap_err();
        assert!(matches!(err, RegistryError::BatchClaimed(_)));
    }

    #[test]
    fn resolve_unknown_batch_is_noop() {
        let r = reg();
        assert!(!r.resolve_pending_mint("ghost", "assetX", 1).unwrap());
        assert_eq!(r.balance_of("assetX", ALICE).unwrap(), 0);
    }

    #[test]
    fn receive_resolves_and_credits() {
        let r = reg();
        r.add_pending_receive("taddr1", "X", BOB, 250, 1).unwrap();
        assert_eq!(r.pending_receives().unwrap().len(), 1);
        assert!(r.resolve_receive("taddr1").unwrap());
        assert_eq!(r.balance_of("X", BOB).unwrap(), 250);
        assert!(r.pending_receives().unwrap().is_empty());
        // Resolving again is a no-op (idempotent — pending row is gone).
        assert!(!r.resolve_receive("taddr1").unwrap());
        assert_eq!(r.balance_of("X", BOB).unwrap(), 250);
    }
}
