use std::fs::remove_file;
use std::path::PathBuf;

use rand::{rngs::OsRng, RngCore};
use tauri::{AppHandle, Manager};
use tauri_plugin_stronghold::stronghold::Stronghold;
use zeroize::Zeroizing;

use crate::kdf::{Argon2Params, KDF_VERSION_LATEST};

use super::seed::{self, SeedError};

const SNAPSHOT_FILE: &str = "ozark-wallet.stronghold";
const SALT_FILE: &str = "ozark-wallet.salt";
const CLIENT_NAME: &[u8] = b"ark-client";
const MNEMONIC_KEY: &str = "mnemonic";
const SALT_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("stronghold error: {0}")]
    Stronghold(String),
    #[error("stronghold internal error: {0}")]
    StrongholdInternal(#[from] tauri_plugin_stronghold::stronghold::Error),
    #[error("stronghold client error: {0}")]
    Client(#[from] iota_stronghold::ClientError),
    #[error("seed error: {0}")]
    Seed(#[from] SeedError),
    #[error("argon2 error: {0}")]
    Argon2(String),
    #[error("unsupported KDF version: {0} (wallet written by a newer build)")]
    UnsupportedKdfVersion(u8),
    #[error("wallet not initialized")]
    NotInitialized,
    #[error("invalid password")]
    InvalidPassword,
    #[error("incorrect password")]
    WrongPassword,
    #[error("corrupted snapshot")]
    CorruptedSnapshot,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid utf8")]
    Utf8(#[from] std::string::FromUtf8Error),
}

fn app_data_dir(app_handle: &AppHandle) -> Result<PathBuf, VaultError> {
    let dir = app_handle
        .path()
        .app_local_data_dir()
        .map_err(|e| VaultError::Stronghold(e.to_string()))?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn snapshot_path(app_handle: &AppHandle) -> Result<PathBuf, VaultError> {
    Ok(app_data_dir(app_handle)?.join(SNAPSHOT_FILE))
}

fn salt_path(app_handle: &AppHandle) -> Result<PathBuf, VaultError> {
    Ok(app_data_dir(app_handle)?.join(SALT_FILE))
}

fn derive_key(password: &str, salt: &[u8], version: u8) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let params =
        Argon2Params::for_version(version).ok_or(VaultError::UnsupportedKdfVersion(version))?;
    params.derive(password, salt).map_err(VaultError::Argon2)
}

fn generate_salt() -> Vec<u8> {
    let mut salt = vec![0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt[..]);
    salt
}

/// Serialize a salt with its KDF version as `[version][salt..]`.
fn encode_salt(version: u8, salt: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + salt.len());
    out.push(version);
    out.extend_from_slice(salt);
    out
}

/// Parse a salt file, returning `(kdf_version, salt)`.
///
/// Two on-disk formats are accepted for backward compatibility:
/// * legacy — exactly `SALT_LEN` raw bytes, no version prefix → KDF v1;
/// * versioned — `[version][SALT_LEN salt]` (`SALT_LEN + 1` bytes).
///
/// The two are disambiguated purely by length, so an existing wallet's 16-byte
/// salt keeps deriving with v1 parameters while new wallets write v2.
fn decode_salt(data: &[u8]) -> Result<(u8, Vec<u8>), VaultError> {
    match data.len() {
        SALT_LEN => Ok((1, data.to_vec())),
        len if len == SALT_LEN + 1 => Ok((data[0], data[1..].to_vec())),
        _ => Err(VaultError::CorruptedSnapshot),
    }
}

fn read_salt(app_handle: &AppHandle) -> Result<(u8, Vec<u8>), VaultError> {
    let path = salt_path(app_handle)?;
    if !path.exists() {
        return Err(VaultError::NotInitialized);
    }
    decode_salt(&std::fs::read(&path)?)
}

fn write_salt(path: &std::path::Path, salt: &[u8]) -> Result<(), VaultError> {
    std::fs::write(path, salt)?;
    Ok(())
}

/// Atomically write a new wallet snapshot + salt.
///
/// The new snapshot is written to a temporary file first. Only after a successful
/// `Stronghold::save()` are the old files backed up and replaced. If anything fails
/// after the backups are created, the old snapshot/salt are restored from `.bak`.
fn write_wallet_atomic(
    app_handle: &AppHandle,
    password: &str,
    mnemonic: &str,
) -> Result<(), VaultError> {
    let _ = seed::validate_mnemonic(mnemonic)?;

    let snapshot = snapshot_path(app_handle)?;
    let salt = salt_path(app_handle)?;

    let snapshot_tmp = snapshot.with_extension("stronghold.tmp");
    let salt_tmp = salt.with_extension("salt.tmp");
    let snapshot_bak = snapshot.with_extension("stronghold.bak");
    let salt_bak = salt.with_extension("salt.bak");

    // Remove any stale temp files from a previous interrupted write.
    if snapshot_tmp.exists() {
        remove_file(&snapshot_tmp)?;
    }
    if salt_tmp.exists() {
        remove_file(&salt_tmp)?;
    }

    // New wallets (and password changes / re-saves) are written with the latest
    // hardened KDF parameters; the version is persisted in the salt file so the
    // matching parameters are used to re-derive the key on unlock.
    let new_salt = generate_salt();
    let version = KDF_VERSION_LATEST;
    let key = derive_key(password, &new_salt, version)?;
    let stronghold = Stronghold::new(&snapshot_tmp, key.to_vec())?;
    let client = stronghold.create_client(CLIENT_NAME)?;
    client
        .store()
        .insert(
            MNEMONIC_KEY.as_bytes().to_vec(),
            mnemonic.as_bytes().to_vec(),
            None,
        )
        .map_err(|e| VaultError::Stronghold(e.to_string()))?;
    stronghold.save()?;
    write_salt(&salt_tmp, &encode_salt(version, &new_salt))?;

    // Rollback helper: restore old files if the replacement fails part-way.
    let rollback = || {
        if snapshot_bak.exists() && !snapshot.exists() {
            let _ = std::fs::rename(&snapshot_bak, &snapshot);
        }
        if salt_bak.exists() && !salt.exists() {
            let _ = std::fs::rename(&salt_bak, &salt);
        }
    };

    let result: Result<(), VaultError> = (|| {
        if snapshot.exists() {
            if snapshot_bak.exists() {
                remove_file(&snapshot_bak)?;
            }
            std::fs::rename(&snapshot, &snapshot_bak)?;
        }
        if salt.exists() {
            if salt_bak.exists() {
                remove_file(&salt_bak)?;
            }
            std::fs::rename(&salt, &salt_bak)?;
        }
        std::fs::rename(&snapshot_tmp, &snapshot)?;
        std::fs::rename(&salt_tmp, &salt)?;
        Ok(())
    })();

    if result.is_err() {
        rollback();
    }
    result?;

    // Replacement succeeded: remove backups.
    if snapshot_bak.exists() {
        remove_file(&snapshot_bak)?;
    }
    if salt_bak.exists() {
        remove_file(&salt_bak)?;
    }

    Ok(())
}

/// Initialize a new wallet vault with the given password and mnemonic.
pub fn create_wallet(
    app_handle: &AppHandle,
    password: &str,
    mnemonic: &str,
) -> Result<String, VaultError> {
    write_wallet_atomic(app_handle, password, mnemonic)?;
    Ok(mnemonic.to_string())
}

/// Generate a new wallet and store it encrypted with the given password.
pub fn generate_wallet(
    app_handle: &AppHandle,
    password: &str,
    word_count: usize,
) -> Result<String, VaultError> {
    let mnemonic = seed::generate_mnemonic(word_count)?;
    let phrase = mnemonic.to_string();
    create_wallet(app_handle, password, &phrase)?;
    Ok(phrase)
}

/// Returns true if a wallet snapshot exists on disk.
pub fn has_wallet(app_handle: &AppHandle) -> bool {
    snapshot_path(app_handle)
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Unlock the wallet and return the stored mnemonic phrase.
/// Uses a single Stronghold load so the client is not loaded twice.
pub fn unlock_and_get_mnemonic(
    app_handle: &AppHandle,
    password: &str,
) -> Result<String, VaultError> {
    let stronghold = load_stronghold(app_handle, password)?;
    let client = match stronghold.inner().get_client(CLIENT_NAME) {
        Ok(client) => client,
        Err(_) => stronghold.load_client(CLIENT_NAME)?,
    };
    let bytes = client
        .store()
        .get(MNEMONIC_KEY.as_bytes())
        .map_err(|e| VaultError::Stronghold(e.to_string()))?
        .ok_or(VaultError::InvalidPassword)?;
    let phrase = String::from_utf8(bytes)?;
    Ok(phrase)
}

/// Retrieve the stored mnemonic phrase. Requires the correct password.
pub fn get_mnemonic(app_handle: &AppHandle, password: &str) -> Result<String, VaultError> {
    unlock_and_get_mnemonic(app_handle, password)
}

/// Store an arbitrary secret in the Stronghold vault.
/// Requires the wallet password. The secret is encrypted with the same key as the seed.
pub fn store_secret(
    app_handle: &AppHandle,
    password: &str,
    key: &str,
    value: &str,
) -> Result<(), VaultError> {
    let stronghold = load_stronghold(app_handle, password)?;
    let client = match stronghold.inner().get_client(CLIENT_NAME) {
        Ok(client) => client,
        Err(_) => stronghold.load_client(CLIENT_NAME)?,
    };
    client
        .store()
        .insert(key.as_bytes().to_vec(), value.as_bytes().to_vec(), None)
        .map_err(|e| VaultError::Stronghold(e.to_string()))?;
    stronghold.save()?;
    Ok(())
}

/// Load an arbitrary secret from the Stronghold vault.
/// Requires the wallet password.
pub fn load_secret(
    app_handle: &AppHandle,
    password: &str,
    key: &str,
) -> Result<String, VaultError> {
    let stronghold = load_stronghold(app_handle, password)?;
    let client = match stronghold.inner().get_client(CLIENT_NAME) {
        Ok(client) => client,
        Err(_) => stronghold.load_client(CLIENT_NAME)?,
    };
    let bytes = client
        .store()
        .get(key.as_bytes())
        .map_err(|e| VaultError::Stronghold(e.to_string()))?
        .ok_or(VaultError::InvalidPassword)?;
    String::from_utf8(bytes).map_err(Into::into)
}

fn load_stronghold(app_handle: &AppHandle, password: &str) -> Result<Stronghold, VaultError> {
    let snapshot = snapshot_path(app_handle)?;
    if !snapshot.exists() {
        // No snapshot on disk: the wallet was never created (or was deleted).
        return Err(VaultError::NotInitialized);
    }

    let (version, salt) = read_salt(app_handle)?;
    let key = derive_key(password, &salt, version)?;

    // Opening the snapshot decrypts it with the derived key. A failure here means
    // the password is wrong (key cannot decrypt the snapshot) — the snapshot file
    // itself is present, so this is distinct from "not initialized".
    let stronghold =
        Stronghold::new(&snapshot, key.to_vec()).map_err(|_| VaultError::WrongPassword)?;

    // The snapshot decrypted successfully. From here, any failure to obtain the
    // client or the stored mnemonic means the snapshot structure is damaged, not
    // that the password was wrong — surface that as a distinct error.
    let client = match stronghold.inner().get_client(CLIENT_NAME) {
        Ok(client) => client,
        Err(_) => stronghold
            .load_client(CLIENT_NAME)
            .map_err(|_| VaultError::CorruptedSnapshot)?,
    };

    let _ = client
        .store()
        .get(MNEMONIC_KEY.as_bytes())
        .map_err(|_| VaultError::CorruptedSnapshot)?
        .ok_or(VaultError::CorruptedSnapshot)?;

    Ok(stronghold)
}

/// Change the wallet password.
pub fn change_password(
    app_handle: &AppHandle,
    old_password: &str,
    new_password: &str,
) -> Result<(), VaultError> {
    let mnemonic = get_mnemonic(app_handle, old_password)?;
    write_wallet_atomic(app_handle, new_password, &mnemonic)?;
    Ok(())
}

/// Permanently delete the wallet snapshot from disk.
pub fn delete_wallet(app_handle: &AppHandle) -> Result<(), VaultError> {
    let snapshot = snapshot_path(app_handle)?;
    if snapshot.exists() {
        remove_file(&snapshot)?;
    }
    let salt = salt_path(app_handle)?;
    if salt.exists() {
        remove_file(&salt)?;
    }
    // Also remove leftover backup/temp files from interrupted writes.
    for ext in ["stronghold.bak", "salt.bak", "stronghold.tmp", "salt.tmp"] {
        let path = snapshot.with_extension(ext);
        if path.exists() {
            let _ = remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_16_byte_salt_decodes_as_v1() {
        let raw = vec![0xABu8; SALT_LEN];
        let (version, salt) = decode_salt(&raw).unwrap();
        assert_eq!(version, 1);
        assert_eq!(salt, raw);
    }

    #[test]
    fn versioned_salt_roundtrips() {
        let salt = generate_salt();
        let encoded = encode_salt(KDF_VERSION_LATEST, &salt);
        assert_eq!(encoded.len(), SALT_LEN + 1);
        assert_eq!(encoded[0], KDF_VERSION_LATEST);
        let (version, decoded) = decode_salt(&encoded).unwrap();
        assert_eq!(version, KDF_VERSION_LATEST);
        assert_eq!(decoded, salt);
    }

    #[test]
    fn wrong_length_salt_is_rejected() {
        assert!(matches!(
            decode_salt(&[1u8; 8]),
            Err(VaultError::CorruptedSnapshot)
        ));
        assert!(matches!(
            decode_salt(&[1u8; SALT_LEN + 5]),
            Err(VaultError::CorruptedSnapshot)
        ));
    }

    #[test]
    fn derive_key_selects_params_by_version() {
        let salt = [3u8; SALT_LEN];
        let k1 = derive_key("pw", &salt, 1).unwrap();
        let k2 = derive_key("pw", &salt, 2).unwrap();
        // Distinct KDF parameters must yield distinct keys.
        assert_ne!(k1.to_vec(), k2.to_vec());
    }

    #[test]
    fn derive_key_rejects_unknown_version() {
        assert!(matches!(
            derive_key("pw", &[0u8; SALT_LEN], 200),
            Err(VaultError::UnsupportedKdfVersion(200))
        ));
    }
}
