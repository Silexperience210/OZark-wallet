//! Encrypted hot-backup of the ledger.
//!
//! The SQLite ledger IS the custody record — losing it means tapd still holds the
//! assets but nobody knows whose they are. This takes periodic consistent
//! snapshots (`VACUUM INTO`, safe under WAL) and, when a key is configured,
//! encrypts them at rest with XChaCha20-Poly1305 before writing to a backup
//! directory, keeping the most recent `retention` files. Meant to write to a
//! volume separate from the live DB (ideally shipped off-box by the operator).

use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::registry::Registry;

const PREFIX: &str = "ledger-";

/// Take one snapshot: `VACUUM INTO` a temp file, optionally encrypt, write to
/// `dir/ledger-<unixsecs>.sqlite[.enc]`, then prune to the `retention` newest.
/// Returns the path written. Blocking (fs + sqlite) — run under `spawn_blocking`.
pub fn run_backup(
    registry: &Registry,
    dir: &Path,
    retention: usize,
    key: Option<&[u8; 32]>,
) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|e| format!("create backup dir {}: {e}", dir.display()))?;
    let ts = crate::auth::now_secs();

    // Consistent snapshot to a temp file inside the backup dir, then finalize
    // (read back, maybe encrypt, write the artifact). Always remove the temp.
    let tmp = dir.join(format!(".{PREFIX}{ts}.tmp"));
    registry
        .snapshot_to(&tmp)
        .map_err(|e| format!("snapshot: {e}"))?;
    let outcome = finalize(&tmp, dir, ts, key);
    let _ = fs::remove_file(&tmp);
    let dest = outcome?;

    prune(dir, retention);
    Ok(dest)
}

/// Read the temp snapshot, optionally encrypt, write the final artifact.
fn finalize(tmp: &Path, dir: &Path, ts: u64, key: Option<&[u8; 32]>) -> Result<PathBuf, String> {
    let plain = fs::read(tmp).map_err(|e| format!("read snapshot: {e}"))?;
    let (bytes, ext) = match key {
        Some(k) => (encrypt(k, &plain)?, "sqlite.enc"),
        None => (plain, "sqlite"),
    };
    let dest = dir.join(format!("{PREFIX}{ts}.{ext}"));
    fs::write(&dest, &bytes).map_err(|e| format!("write backup: {e}"))?;
    Ok(dest)
}

/// Encrypt `plaintext` with XChaCha20-Poly1305 under `key`; output layout is a
/// 24-byte random nonce followed by the ciphertext (with its AEAD tag).
fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher =
        XChaCha20Poly1305::new_from_slice(key).map_err(|_| "invalid backup key".to_string())?;
    let mut nonce_bytes = [0u8; 24];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| format!("rng: {e}"))?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| "backup encryption failed".to_string())?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Delete all but the `retention` (min 1) most recent snapshot files.
fn prune(dir: &Path, retention: usize) {
    let retention = retention.max(1);
    let mut snaps: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(PREFIX))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            log::warn!("backup prune read_dir {}: {e}", dir.display());
            return;
        }
    };
    if snaps.len() <= retention {
        return;
    }
    // Oldest first (by mtime), then drop everything past the newest `retention`.
    snaps.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
    for p in &snaps[..snaps.len() - retention] {
        if let Err(e) = fs::remove_file(p) {
            log::warn!("backup prune remove {}: {e}", p.display());
        }
    }
}
