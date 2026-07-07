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
use serde::Serialize;

/// The kinds of balance-affecting events recorded in the per-user history.
/// Direction is implied by the kind; `amount` is always positive.
pub mod event_kind {
    pub const MINT: &str = "mint";
    pub const RECEIVE: &str = "receive";
    pub const SEND: &str = "send";
    pub const LN_SEND: &str = "ln_send";
    pub const LN_RECEIVE: &str = "ln_receive";
    pub const BURN: &str = "burn";
    pub const TRANSFER_IN: &str = "transfer_in";
    pub const TRANSFER_OUT: &str = "transfer_out";
    /// Sats deposit (Lightning top-up of the custodial sats balance).
    pub const DEPOSIT: &str = "deposit";
    /// Sats fee charged for an on-chain op (paid by the user to the operator).
    pub const FEE: &str = "fee";
    /// Sats fee earned by the operator.
    pub const FEE_EARNED: &str = "fee_earned";
}

/// The asset id used in history rows for the native sats balance (deposits/fees),
/// so the per-user history can carry them alongside asset events.
pub const SATS: &str = "sats";

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
    #[error("insufficient sats: {pubkey} holds less than {amount}")]
    InsufficientSats { pubkey: String, amount: u64 },
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

/// A Lightning-asset invoice awaiting settlement to credit. Keyed by the invoice's
/// hex payment hash, which reconciliation polls via lnd's `LookupInvoice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLnReceive {
    pub r_hash: String,
    pub asset_id: String,
    pub pubkey: String,
    pub amount: u64,
}

/// A Lightning deposit awaiting settlement to credit the user's sats balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSatsDeposit {
    pub r_hash: String,
    pub pubkey: String,
    pub amount: u64,
}

/// A debited-but-unresolved payment (send / ln_send). Recovery matches these
/// against the node's real state after a crash. `reference` is the LN payment hash
/// (ln_send) — recovery tracks it via `TrackPaymentV2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    pub id: i64,
    pub kind: String,
    pub pubkey: String,
    pub asset_id: String,
    pub amount: u64,
    pub reference: Option<String>,
    pub counterparty: Option<String>,
    pub created_at: i64,
}

/// One entry in a user's transaction history — the custodial analog of tapd's
/// node-global transfer list, but scoped to a single user via the ledger. Every
/// balance-affecting action appends one (transfers append two: out + in).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerEvent {
    pub id: i64,
    pub asset_id: String,
    /// One of [`event_kind`].
    pub kind: String,
    pub amount: u64,
    /// The other party where meaningful: recipient/sender pubkey for transfers,
    /// the destination address for a send/receive. `None` for mint/burn.
    pub counterparty: Option<String>,
    /// A reference to the underlying action: on-chain txid (send/burn), receive
    /// address, or mint batch key. `None` for internal transfers.
    pub reference: Option<String>,
    pub created_at: i64,
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
             );
             -- Lightning-asset invoices awaiting settlement, to credit on settle.
             -- Keyed by the invoice's hex payment hash (polled via LookupInvoice).
             CREATE TABLE IF NOT EXISTS pending_ln_receives (
                 r_hash     TEXT PRIMARY KEY,
                 asset_id   TEXT NOT NULL,
                 pubkey     TEXT NOT NULL,
                 amount     INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );
             -- Durable record of a payment that has been DEBITED but whose outcome
             -- is not yet known (send / ln_send). Written atomically with the debit
             -- BEFORE the tapd/lnd call; cleared on the terminal outcome (settle =
             -- record history, refund = credit back). A crash mid-payment therefore
             -- leaves a recoverable row instead of a silent debit.
             CREATE TABLE IF NOT EXISTS in_flight (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind         TEXT NOT NULL,
                 pubkey       TEXT NOT NULL,
                 asset_id     TEXT NOT NULL,
                 amount       INTEGER NOT NULL,
                 reference    TEXT,
                 counterparty TEXT,
                 created_at   INTEGER NOT NULL
             );
             -- Small key/value settings, e.g. the operator pubkey once claimed.
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Per-user native sats balance (custodial), funded by Lightning
             -- deposits and spent on on-chain operation fees.
             CREATE TABLE IF NOT EXISTS sats_balances (
                 pubkey TEXT PRIMARY KEY,
                 amount INTEGER NOT NULL DEFAULT 0
             );
             -- Lightning deposit invoices awaiting settlement, to credit sats.
             CREATE TABLE IF NOT EXISTS pending_sats_deposits (
                 r_hash     TEXT PRIMARY KEY,
                 pubkey     TEXT NOT NULL,
                 amount     INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );
             -- Per-user transaction history. Append-only; one row per balance move
             -- (transfers append two: transfer_out for the sender, transfer_in for
             -- the recipient). Ordered by `id` so display order matches insertion
             -- regardless of clock skew between event timestamps.
             CREATE TABLE IF NOT EXISTS ledger_events (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 pubkey       TEXT NOT NULL,
                 asset_id     TEXT NOT NULL,
                 kind         TEXT NOT NULL,
                 amount       INTEGER NOT NULL,
                 counterparty TEXT,
                 reference    TEXT,
                 created_at   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_pubkey ON ledger_events(pubkey, id DESC);",
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

    /// Total custodial liability per asset: the sum of every user's positive
    /// balance, as `(asset_id, total)`. The solvency audit compares this against
    /// tapd's actual holding — the invariant is `total ≤ node holding`, per asset.
    pub fn total_liabilities_by_asset(&self) -> Result<Vec<(String, u64)>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT asset_id, SUM(amount) FROM balances WHERE amount > 0 GROUP BY asset_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Write a consistent snapshot of the ledger to `path` via `VACUUM INTO` (safe
    /// under WAL, no writer pause). The caller encrypts + rotates the result.
    pub fn snapshot_to(&self, path: &Path) -> Result<(), RegistryError> {
        let dest = path.to_string_lossy().to_string();
        let conn = self.lock();
        conn.execute("VACUUM INTO ?1", [dest])?;
        Ok(())
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
        let now = crate::auth::now_secs() as i64;
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        debit_conn(&tx, asset_id, from, amount)?;
        credit_conn(&tx, asset_id, to, amount)?;
        record_event_conn(
            &tx,
            from,
            asset_id,
            event_kind::TRANSFER_OUT,
            amount,
            Some(to),
            None,
            now,
        )?;
        record_event_conn(
            &tx,
            to,
            asset_id,
            event_kind::TRANSFER_IN,
            amount,
            Some(from),
            None,
            now,
        )?;
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
        let credited = amount.max(0) as u64;
        credit_conn(&tx, asset_id, &owner, credited)?;
        record_event_conn(
            &tx,
            &owner,
            asset_id,
            event_kind::MINT,
            credited,
            None,
            Some(batch_key),
            created_at,
        )?;
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
        let credited = amount.max(0) as u64;
        credit_conn(&tx, &asset_id, &pubkey, credited)?;
        record_event_conn(
            &tx,
            &pubkey,
            &asset_id,
            event_kind::RECEIVE,
            credited,
            Some(addr),
            Some(addr),
            crate::auth::now_secs() as i64,
        )?;
        tx.execute("DELETE FROM pending_receives WHERE addr = ?1", [addr])?;
        tx.commit()?;
        Ok(true)
    }

    // ---- Pending Lightning receives ---------------------------------------

    /// Record a Lightning-asset invoice awaiting settlement for `pubkey`, keyed by
    /// its hex payment hash. Idempotent on the hash (re-issuing is a no-op).
    pub fn add_pending_ln_receive(
        &self,
        r_hash: &str,
        asset_id: &str,
        pubkey: &str,
        amount: u64,
        created_at: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO pending_ln_receives
                 (r_hash, asset_id, pubkey, amount, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (r_hash, asset_id, pubkey, amount as i64, created_at),
        )?;
        Ok(())
    }

    /// All Lightning-asset invoices still awaiting settlement.
    pub fn pending_ln_receives(&self) -> Result<Vec<PendingLnReceive>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT r_hash, asset_id, pubkey, amount
             FROM pending_ln_receives ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingLnReceive {
                r_hash: r.get(0)?,
                asset_id: r.get(1)?,
                pubkey: r.get(2)?,
                amount: r.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Credit a settled Lightning-asset invoice to its recipient and drop the
    /// pending row, atomically. Returns whether a pending invoice was resolved.
    pub fn resolve_ln_receive(&self, r_hash: &str) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT asset_id, pubkey, amount FROM pending_ln_receives WHERE r_hash = ?1",
                [r_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((asset_id, pubkey, amount)) = row else {
            return Ok(false);
        };
        let credited = amount.max(0) as u64;
        credit_conn(&tx, &asset_id, &pubkey, credited)?;
        record_event_conn(
            &tx,
            &pubkey,
            &asset_id,
            event_kind::LN_RECEIVE,
            credited,
            None,
            Some(r_hash),
            crate::auth::now_secs() as i64,
        )?;
        tx.execute(
            "DELETE FROM pending_ln_receives WHERE r_hash = ?1",
            [r_hash],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Drop a pending Lightning receive **without** crediting — for an invoice that
    /// was canceled/expired. Returns whether a row was removed.
    pub fn delete_pending_ln_receive(&self, r_hash: &str) -> Result<bool, RegistryError> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM pending_ln_receives WHERE r_hash = ?1",
            [r_hash],
        )?;
        Ok(n > 0)
    }

    /// Purge pending Lightning receives created before `cutoff` (unix secs),
    /// whatever their state — a safety net for invoices lnd no longer knows about,
    /// so the poll set can't grow without bound. Returns the number purged.
    pub fn purge_stale_ln_receives(&self, cutoff: i64) -> Result<usize, RegistryError> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM pending_ln_receives WHERE created_at < ?1",
            [cutoff],
        )?;
        Ok(n)
    }

    // ---- In-flight payments (crash recovery) ------------------------------

    /// Atomically debit `pubkey` **and** record the payment as in-flight, so a
    /// crash before the tapd/lnd call resolves can be recovered (never a silent
    /// debit). Errors (incl. insufficient balance) roll back both. Returns the
    /// in-flight row id to resolve with `settle_in_flight` / `refund_in_flight`.
    #[allow(clippy::too_many_arguments)]
    pub fn debit_and_mark_in_flight(
        &self,
        kind: &str,
        asset_id: &str,
        pubkey: &str,
        amount: u64,
        reference: Option<&str>,
        counterparty: Option<&str>,
        created_at: i64,
    ) -> Result<i64, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        debit_conn(&tx, asset_id, pubkey, amount)?;
        tx.execute(
            "INSERT INTO in_flight
                 (kind, pubkey, asset_id, amount, reference, counterparty, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                kind,
                pubkey,
                asset_id,
                amount as i64,
                reference,
                counterparty,
                created_at,
            ),
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Resolve an in-flight payment as **succeeded**: record its history event
    /// (kind/counterparty from the row; `reference_override` wins when given, e.g.
    /// the on-chain txid learned only after the send) and drop the row, atomically.
    /// Returns whether a row was resolved.
    #[allow(clippy::type_complexity)]
    pub fn settle_in_flight(
        &self,
        id: i64,
        reference_override: Option<&str>,
    ) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(String, String, String, i64, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT kind, pubkey, asset_id, amount, counterparty, reference
                 FROM in_flight WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind, pubkey, asset_id, amount, counterparty, reference)) = row else {
            return Ok(false);
        };
        let reference = reference_override.map(|s| s.to_string()).or(reference);
        record_event_conn(
            &tx,
            &pubkey,
            &asset_id,
            &kind,
            amount.max(0) as u64,
            counterparty.as_deref(),
            reference.as_deref(),
            crate::auth::now_secs() as i64,
        )?;
        tx.execute("DELETE FROM in_flight WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    /// Resolve an in-flight payment as **failed**: credit the reserved amount back
    /// and drop the row, atomically. Returns whether a row was resolved.
    pub fn refund_in_flight(&self, id: i64) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT pubkey, asset_id, amount FROM in_flight WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((pubkey, asset_id, amount)) = row else {
            return Ok(false);
        };
        credit_conn(&tx, &asset_id, &pubkey, amount.max(0) as u64)?;
        tx.execute("DELETE FROM in_flight WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    /// All unresolved in-flight payments, oldest first — the recovery work set.
    pub fn in_flight_entries(&self) -> Result<Vec<InFlight>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, pubkey, asset_id, amount, reference, counterparty, created_at
             FROM in_flight ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(InFlight {
                id: r.get(0)?,
                kind: r.get(1)?,
                pubkey: r.get(2)?,
                asset_id: r.get(3)?,
                amount: r.get::<_, i64>(4)? as u64,
                reference: r.get(5)?,
                counterparty: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // ---- Settings (operator claim) ----------------------------------------

    /// The persisted operator pubkey (set via the admin claim), if any.
    pub fn get_admin_pubkey(&self) -> Result<Option<String>, RegistryError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM settings WHERE key = 'admin_pubkey'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Persist the operator pubkey (idempotent overwrite).
    pub fn set_admin_pubkey(&self, pubkey: &str) -> Result<(), RegistryError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('admin_pubkey', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [pubkey],
        )?;
        Ok(())
    }

    // ---- Sats balance + fees ----------------------------------------------

    /// The caller's custodial sats balance.
    pub fn sats_balance_of(&self, pubkey: &str) -> Result<u64, RegistryError> {
        let conn = self.lock();
        sats_balance_conn(&conn, pubkey)
    }

    /// Charge a sats fee: debit `payer`, credit `operator`, atomically, recording a
    /// `fee` event for the payer and `fee_earned` for the operator. Errors
    /// (`InsufficientSats`) roll back. Callers skip this when payer == operator.
    pub fn charge_fee(
        &self,
        payer: &str,
        operator: &str,
        amount: u64,
    ) -> Result<(), RegistryError> {
        let now = crate::auth::now_secs() as i64;
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        debit_sats_conn(&tx, payer, amount)?;
        credit_sats_conn(&tx, operator, amount)?;
        record_event_conn(
            &tx,
            payer,
            SATS,
            event_kind::FEE,
            amount,
            Some(operator),
            None,
            now,
        )?;
        record_event_conn(
            &tx,
            operator,
            SATS,
            event_kind::FEE_EARNED,
            amount,
            Some(payer),
            None,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reverse a fee (tapd action failed after charging): credit the payer back and
    /// debit the operator (saturating — never errors). Best-effort.
    pub fn refund_fee(
        &self,
        payer: &str,
        operator: &str,
        amount: u64,
    ) -> Result<(), RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        credit_sats_conn(&tx, payer, amount)?;
        let op_bal = sats_balance_conn(&tx, operator)?;
        let take = amount.min(op_bal);
        if take > 0 {
            tx.execute(
                "UPDATE sats_balances SET amount = amount - ?2 WHERE pubkey = ?1",
                (operator, take as i64),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record a Lightning deposit invoice awaiting settlement for `pubkey`.
    pub fn add_pending_sats_deposit(
        &self,
        r_hash: &str,
        pubkey: &str,
        amount: u64,
        created_at: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO pending_sats_deposits (r_hash, pubkey, amount, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            (r_hash, pubkey, amount as i64, created_at),
        )?;
        Ok(())
    }

    /// All sats deposits still awaiting settlement.
    pub fn pending_sats_deposits(&self) -> Result<Vec<PendingSatsDeposit>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT r_hash, pubkey, amount FROM pending_sats_deposits ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingSatsDeposit {
                r_hash: r.get(0)?,
                pubkey: r.get(1)?,
                amount: r.get::<_, i64>(2)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Credit a settled sats deposit and drop the pending row, atomically.
    pub fn resolve_sats_deposit(&self, r_hash: &str) -> Result<bool, RegistryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT pubkey, amount FROM pending_sats_deposits WHERE r_hash = ?1",
                [r_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((pubkey, amount)) = row else {
            return Ok(false);
        };
        let credited = amount.max(0) as u64;
        credit_sats_conn(&tx, &pubkey, credited)?;
        record_event_conn(
            &tx,
            &pubkey,
            SATS,
            event_kind::DEPOSIT,
            credited,
            None,
            Some(r_hash),
            crate::auth::now_secs() as i64,
        )?;
        tx.execute(
            "DELETE FROM pending_sats_deposits WHERE r_hash = ?1",
            [r_hash],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Drop a pending sats deposit (canceled/expired invoice) without crediting.
    pub fn delete_pending_sats_deposit(&self, r_hash: &str) -> Result<bool, RegistryError> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM pending_sats_deposits WHERE r_hash = ?1",
            [r_hash],
        )?;
        Ok(n > 0)
    }

    // ---- History ----------------------------------------------------------

    /// Append a history event for `pubkey`. Used by the route layer for actions
    /// whose ledger move is not a single transaction (send/burn: debit, then tapd
    /// call, then record on success). Best-effort — the caller ignores failures so
    /// a history write never fails the underlying action.
    #[allow(clippy::too_many_arguments)]
    pub fn record_event(
        &self,
        pubkey: &str,
        asset_id: &str,
        kind: &str,
        amount: u64,
        counterparty: Option<&str>,
        reference: Option<&str>,
        created_at: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.lock();
        record_event_conn(
            &conn,
            pubkey,
            asset_id,
            kind,
            amount,
            counterparty,
            reference,
            created_at,
        )
    }

    /// The caller's most recent history events, newest first, capped at `limit`.
    pub fn history(&self, pubkey: &str, limit: u32) -> Result<Vec<LedgerEvent>, RegistryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, asset_id, kind, amount, counterparty, reference, created_at
             FROM ledger_events WHERE pubkey = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map((pubkey, limit), |r| {
            Ok(LedgerEvent {
                id: r.get(0)?,
                asset_id: r.get(1)?,
                kind: r.get(2)?,
                amount: r.get::<_, i64>(3)? as u64,
                counterparty: r.get(4)?,
                reference: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
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

fn sats_balance_conn(conn: &Connection, pubkey: &str) -> Result<u64, RegistryError> {
    let amount: Option<i64> = conn
        .query_row(
            "SELECT amount FROM sats_balances WHERE pubkey = ?1",
            [pubkey],
            |r| r.get(0),
        )
        .optional()?;
    Ok(amount.unwrap_or(0).max(0) as u64)
}

fn credit_sats_conn(conn: &Connection, pubkey: &str, amount: u64) -> Result<(), RegistryError> {
    conn.execute(
        "INSERT INTO sats_balances (pubkey, amount) VALUES (?1, ?2)
         ON CONFLICT(pubkey) DO UPDATE SET amount = amount + excluded.amount",
        (pubkey, amount as i64),
    )?;
    Ok(())
}

fn debit_sats_conn(conn: &Connection, pubkey: &str, amount: u64) -> Result<(), RegistryError> {
    let current = sats_balance_conn(conn, pubkey)?;
    if current < amount {
        return Err(RegistryError::InsufficientSats {
            pubkey: pubkey.to_string(),
            amount,
        });
    }
    conn.execute(
        "UPDATE sats_balances SET amount = amount - ?2 WHERE pubkey = ?1",
        (pubkey, amount as i64),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_event_conn(
    conn: &Connection,
    pubkey: &str,
    asset_id: &str,
    kind: &str,
    amount: u64,
    counterparty: Option<&str>,
    reference: Option<&str>,
    created_at: i64,
) -> Result<(), RegistryError> {
    conn.execute(
        "INSERT INTO ledger_events
             (pubkey, asset_id, kind, amount, counterparty, reference, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            pubkey,
            asset_id,
            kind,
            amount as i64,
            counterparty,
            reference,
            created_at,
        ),
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

    #[test]
    fn ln_receive_resolves_credits_and_records_history() {
        let r = reg();
        r.add_pending_ln_receive("deadbeef", "X", BOB, 75, 1)
            .unwrap();
        // Re-issuing the same hash is idempotent (no duplicate pending row).
        r.add_pending_ln_receive("deadbeef", "X", BOB, 75, 1)
            .unwrap();
        assert_eq!(r.pending_ln_receives().unwrap().len(), 1);

        assert!(r.resolve_ln_receive("deadbeef").unwrap());
        assert_eq!(r.balance_of("X", BOB).unwrap(), 75);
        assert!(r.pending_ln_receives().unwrap().is_empty());
        // Idempotent once resolved (pending row is gone).
        assert!(!r.resolve_ln_receive("deadbeef").unwrap());
        assert_eq!(r.balance_of("X", BOB).unwrap(), 75);

        // History carries an ln_receive event referencing the payment hash.
        let ev = r.history(BOB, 50).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, event_kind::LN_RECEIVE);
        assert_eq!(ev[0].amount, 75);
        assert_eq!(ev[0].reference.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn total_liabilities_sums_positive_balances_per_asset() {
        let r = reg();
        r.credit("a1", ALICE, 100).unwrap();
        r.credit("a1", BOB, 50).unwrap();
        r.credit("a2", ALICE, 7).unwrap();
        r.credit("a3", BOB, 5).unwrap();
        r.debit("a3", BOB, 5).unwrap(); // back to zero -> excluded
        let mut liab = r.total_liabilities_by_asset().unwrap();
        liab.sort();
        assert_eq!(liab, vec![("a1".into(), 150), ("a2".into(), 7)]);
    }

    #[test]
    fn ln_receive_purge_and_delete_do_not_credit() {
        let r = reg();
        r.add_pending_ln_receive("old", "X", ALICE, 10, 100)
            .unwrap();
        r.add_pending_ln_receive("new", "X", ALICE, 20, 1000)
            .unwrap();
        // Purge everything created before ts=500 -> drops "old" only.
        assert_eq!(r.purge_stale_ln_receives(500).unwrap(), 1);
        assert_eq!(r.pending_ln_receives().unwrap().len(), 1);
        // Explicit delete (canceled) drops "new" without crediting.
        assert!(r.delete_pending_ln_receive("new").unwrap());
        assert!(r.pending_ln_receives().unwrap().is_empty());
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 0);
        // Idempotent once gone.
        assert!(!r.delete_pending_ln_receive("new").unwrap());
    }

    #[test]
    fn in_flight_settle_records_history_and_keeps_debit() {
        let r = reg();
        r.credit("X", ALICE, 100).unwrap();
        let id = r
            .debit_and_mark_in_flight(
                event_kind::LN_SEND,
                "X",
                ALICE,
                40,
                Some("hash1"),
                Some("dest"),
                1,
            )
            .unwrap();
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 60);
        assert_eq!(r.in_flight_entries().unwrap().len(), 1);

        assert!(r.settle_in_flight(id, None).unwrap());
        assert!(r.in_flight_entries().unwrap().is_empty());
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 60); // stays debited
        let ev = r.history(ALICE, 10).unwrap();
        assert_eq!(ev[0].kind, event_kind::LN_SEND);
        assert_eq!(ev[0].amount, 40);
        assert_eq!(ev[0].reference.as_deref(), Some("hash1"));
        assert_eq!(ev[0].counterparty.as_deref(), Some("dest"));
        // Idempotent once resolved.
        assert!(!r.settle_in_flight(id, None).unwrap());
    }

    #[test]
    fn in_flight_settle_reference_override_wins() {
        let r = reg();
        r.credit("X", ALICE, 100).unwrap();
        let id = r
            .debit_and_mark_in_flight(event_kind::SEND, "X", ALICE, 10, None, Some("taddr"), 1)
            .unwrap();
        r.settle_in_flight(id, Some("txid123")).unwrap();
        let ev = r.history(ALICE, 10).unwrap();
        assert_eq!(ev[0].kind, event_kind::SEND);
        assert_eq!(ev[0].reference.as_deref(), Some("txid123"));
        assert_eq!(ev[0].counterparty.as_deref(), Some("taddr"));
    }

    #[test]
    fn in_flight_refund_restores_balance_without_history() {
        let r = reg();
        r.credit("X", ALICE, 100).unwrap();
        let id = r
            .debit_and_mark_in_flight(event_kind::LN_SEND, "X", ALICE, 40, Some("h"), None, 1)
            .unwrap();
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 60);
        assert!(r.refund_in_flight(id).unwrap());
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 100);
        assert!(r.in_flight_entries().unwrap().is_empty());
        assert!(r.history(ALICE, 10).unwrap().is_empty());
        assert!(!r.refund_in_flight(id).unwrap()); // idempotent
    }

    #[test]
    fn in_flight_debit_insufficient_rolls_back_both() {
        let r = reg();
        r.credit("X", ALICE, 10).unwrap();
        let err = r
            .debit_and_mark_in_flight(event_kind::SEND, "X", ALICE, 50, None, None, 1)
            .unwrap_err();
        assert!(matches!(err, RegistryError::InsufficientBalance { .. }));
        assert_eq!(r.balance_of("X", ALICE).unwrap(), 10);
        assert!(r.in_flight_entries().unwrap().is_empty());
    }

    #[test]
    fn admin_pubkey_persists_and_overwrites() {
        let r = reg();
        assert!(r.get_admin_pubkey().unwrap().is_none());
        r.set_admin_pubkey("abc123").unwrap();
        assert_eq!(r.get_admin_pubkey().unwrap().as_deref(), Some("abc123"));
        r.set_admin_pubkey("def456").unwrap();
        assert_eq!(r.get_admin_pubkey().unwrap().as_deref(), Some("def456"));
    }

    #[test]
    fn sats_deposit_charge_and_refund() {
        let r = reg();
        r.add_pending_sats_deposit("dep1", ALICE, 10_000, 1)
            .unwrap();
        assert!(r.resolve_sats_deposit("dep1").unwrap());
        assert_eq!(r.sats_balance_of(ALICE).unwrap(), 10_000);

        r.charge_fee(ALICE, BOB, 500).unwrap();
        assert_eq!(r.sats_balance_of(ALICE).unwrap(), 9_500);
        assert_eq!(r.sats_balance_of(BOB).unwrap(), 500);

        r.refund_fee(ALICE, BOB, 500).unwrap();
        assert_eq!(r.sats_balance_of(ALICE).unwrap(), 10_000);
        assert_eq!(r.sats_balance_of(BOB).unwrap(), 0);

        let err = r.charge_fee(ALICE, BOB, 999_999).unwrap_err();
        assert!(matches!(err, RegistryError::InsufficientSats { .. }));
        assert_eq!(r.sats_balance_of(ALICE).unwrap(), 10_000);

        let kinds: Vec<String> = r
            .history(ALICE, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&event_kind::DEPOSIT.to_string()));
        assert!(kinds.contains(&event_kind::FEE.to_string()));
    }

    #[test]
    fn history_records_mint_receive_and_transfer_both_sides() {
        let r = reg();
        // mint 1000 to ALICE
        r.add_pending_mint("b1", "", ALICE, "OZK", 1000, 10)
            .unwrap();
        r.resolve_pending_mint("b1", "assetA", 20).unwrap();
        // receive 50 to ALICE
        r.add_pending_receive("addr1", "assetA", ALICE, 50, 5)
            .unwrap();
        r.resolve_receive("addr1").unwrap();
        // internal transfer 30 ALICE -> BOB
        r.transfer("assetA", ALICE, BOB, 30).unwrap();

        // ALICE sees newest-first: transfer_out, receive, mint.
        let alice: Vec<String> = r
            .history(ALICE, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(alice, vec!["transfer_out", "receive", "mint"]);

        // BOB sees only the incoming transfer, with ALICE as counterparty.
        let bob = r.history(BOB, 50).unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].kind, event_kind::TRANSFER_IN);
        assert_eq!(bob[0].amount, 30);
        assert_eq!(bob[0].asset_id, "assetA");
        assert_eq!(bob[0].counterparty.as_deref(), Some(ALICE));

        // The mint event carries its batch key as the reference.
        let mint = r
            .history(ALICE, 50)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == event_kind::MINT)
            .unwrap();
        assert_eq!(mint.amount, 1000);
        assert_eq!(mint.reference.as_deref(), Some("b1"));
    }

    #[test]
    fn record_event_direct_and_limit() {
        let r = reg();
        for i in 0..5u64 {
            r.record_event(
                ALICE,
                "X",
                event_kind::SEND,
                i + 1,
                None,
                Some("txid"),
                i as i64,
            )
            .unwrap();
        }
        assert_eq!(r.history(ALICE, 50).unwrap().len(), 5);
        let two = r.history(ALICE, 2).unwrap();
        assert_eq!(two.len(), 2);
        // Newest first: last inserted (amount 5) then (amount 4).
        assert_eq!(two[0].amount, 5);
        assert_eq!(two[1].amount, 4);
    }
}
