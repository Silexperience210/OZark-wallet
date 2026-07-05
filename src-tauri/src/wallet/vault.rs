use std::fs::remove_file;
use std::path::PathBuf;

use argon2::{Config, ThreadMode, Variant, Version};
use rand::{rngs::OsRng, RngCore};
use tauri::{AppHandle, Manager};
use tauri_plugin_stronghold::stronghold::Stronghold;
use zeroize::Zeroizing;

use super::seed::{self, SeedError};

const SNAPSHOT_FILE: &str = "ozark-wallet.stronghold";
const SALT_FILE: &str = "ozark-wallet.salt";
const CLIENT_NAME: &[u8] = b"ark-client";
const MNEMONIC_KEY: &str = "mnemonic";

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

fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let config = Config {
        variant: Variant::Argon2id,
        version: Version::Version13,
        mem_cost: 65536,
        time_cost: 3,
        lanes: 4,
        thread_mode: ThreadMode::Parallel,
        secret: &[],
        ad: &[],
        hash_length: 32,
    };

    argon2::hash_raw(password.as_bytes(), salt, &config)
        .map(Zeroizing::new)
        .map_err(|e| VaultError::Argon2(e.to_string()))
}

fn generate_salt() -> Vec<u8> {
    let mut salt = vec![0u8; 16];
    OsRng.fill_bytes(&mut salt[..]);
    salt
}

fn read_salt(app_handle: &AppHandle) -> Result<Vec<u8>, VaultError> {
    let path = salt_path(app_handle)?;
    if !path.exists() {
        return Err(VaultError::NotInitialized);
    }
    Ok(std::fs::read(&path)?)
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

    let new_salt = generate_salt();
    let key = derive_key(password, &new_salt)?;
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
    write_salt(&salt_tmp, &new_salt)?;

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

    let salt = read_salt(app_handle)?;
    let key = derive_key(password, &salt)?;

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
