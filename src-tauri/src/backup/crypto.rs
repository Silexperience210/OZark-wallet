use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use rand::{rngs::OsRng, RngCore};

use crate::kdf::{Argon2Params, KDF_VERSION_LATEST};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

/// Magic prefix marking a versioned backup blob (`OZBK`). Legacy blobs have no
/// prefix and begin directly with the 16-byte salt; the two are told apart by
/// this marker. A random legacy salt colliding with the marker (2^-32) would at
/// worst fail to decrypt — never silently misparse — so the heuristic is safe.
const MAGIC: &[u8; 4] = b"OZBK";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("argon2 error: {0}")]
    Argon2(String),
    #[error("unsupported KDF version: {0} (backup written by a newer build)")]
    UnsupportedKdfVersion(u8),
    #[error("encryption error: {0}")]
    Encrypt(String),
    #[error("decryption error: {0}")]
    Decrypt(String),
    #[error("invalid backup format")]
    InvalidFormat,
    #[error("invalid base64")]
    InvalidBase64(#[from] base64::DecodeError),
}

fn derive_key(
    password: &str,
    salt: &[u8],
    version: u8,
) -> Result<zeroize::Zeroizing<Vec<u8>>, BackupError> {
    let params =
        Argon2Params::for_version(version).ok_or(BackupError::UnsupportedKdfVersion(version))?;
    params.derive(password, salt).map_err(BackupError::Argon2)
}

/// Encrypt plaintext with AES-256-GCM using a password.
///
/// Returns a base64-encoded string with the versioned layout:
/// `MAGIC(4) + version(1) + salt(16) + nonce(12) + ciphertext`.
pub fn encrypt(plaintext: &str, password: &str) -> Result<String, BackupError> {
    let version = KDF_VERSION_LATEST;

    let mut salt = vec![0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt[..]);

    let key = derive_key(password, &salt, version)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| BackupError::Encrypt(e.to_string()))?;

    let mut nonce_bytes = vec![0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes[..]);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| BackupError::Encrypt(e.to_string()))?;

    let mut output = Vec::with_capacity(MAGIC.len() + 1 + SALT_LEN + NONCE_LEN + ciphertext.len());
    output.extend_from_slice(MAGIC);
    output.push(version);
    output.extend_from_slice(&salt);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&output))
}

/// Decrypt a backup produced by `encrypt`.
///
/// Accepts both the versioned layout (prefixed with `MAGIC`) and the legacy
/// unversioned layout (`salt + nonce + ciphertext`, KDF v1) so backups exported
/// before versioning can still be restored.
pub fn decrypt(backup_b64: &str, password: &str) -> Result<String, BackupError> {
    let data = BASE64.decode(backup_b64)?;

    let (version, salt, nonce_bytes, ciphertext) = if data.starts_with(MAGIC) {
        let rest = &data[MAGIC.len()..];
        // version(1) + salt + nonce + at least one ciphertext byte
        if rest.len() < 1 + SALT_LEN + NONCE_LEN + 1 {
            return Err(BackupError::InvalidFormat);
        }
        let version = rest[0];
        let salt = &rest[1..1 + SALT_LEN];
        let nonce = &rest[1 + SALT_LEN..1 + SALT_LEN + NONCE_LEN];
        let ct = &rest[1 + SALT_LEN + NONCE_LEN..];
        (version, salt, nonce, ct)
    } else {
        // Legacy unversioned blob: salt + nonce + ciphertext, always KDF v1.
        if data.len() < SALT_LEN + NONCE_LEN + 1 {
            return Err(BackupError::InvalidFormat);
        }
        let salt = &data[..SALT_LEN];
        let nonce = &data[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ct = &data[SALT_LEN + NONCE_LEN..];
        (1u8, salt, nonce, ct)
    };

    let key = derive_key(password, salt, version)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| BackupError::Decrypt(e.to_string()))?;

    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| BackupError::Decrypt(e.to_string()))?;

    String::from_utf8(plaintext).map_err(|_| BackupError::Decrypt("invalid utf8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let plaintext = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let password = "super_secure_password_123";
        let encrypted = encrypt(plaintext, password).unwrap();
        let decrypted = decrypt(&encrypted, password).unwrap();
        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_wrong_password() {
        let plaintext = "secret seed phrase";
        let encrypted = encrypt(plaintext, "correct_password").unwrap();
        assert!(decrypt(&encrypted, "wrong_password").is_err());
    }

    #[test]
    fn test_new_blob_is_versioned() {
        let encrypted = encrypt("seed phrase", "password").unwrap();
        let decoded = BASE64.decode(&encrypted).unwrap();
        assert!(decoded.starts_with(MAGIC));
        assert_eq!(decoded[MAGIC.len()], KDF_VERSION_LATEST);
    }

    #[test]
    fn test_backup_format_length() {
        let encrypted = encrypt("seed phrase", "password").unwrap();
        let decoded = BASE64.decode(&encrypted).unwrap();
        assert!(decoded.len() > MAGIC.len() + 1 + SALT_LEN + NONCE_LEN);
    }

    #[test]
    fn test_invalid_format() {
        let short = BASE64.encode("short");
        assert!(decrypt(&short, "password").is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let encrypted = encrypt("seed phrase", "password").unwrap();
        let mut decoded = BASE64.decode(&encrypted).unwrap();
        // Flip the last ciphertext byte.
        let last = decoded.len() - 1;
        decoded[last] ^= 0xFF;
        let tampered = BASE64.encode(&decoded);
        assert!(matches!(
            decrypt(&tampered, "password"),
            Err(BackupError::Decrypt(_))
        ));
    }

    #[test]
    fn test_tampered_nonce_fails() {
        let encrypted = encrypt("seed phrase", "password").unwrap();
        let mut decoded = BASE64.decode(&encrypted).unwrap();
        // Nonce starts right after MAGIC + version + salt.
        let nonce_start = MAGIC.len() + 1 + SALT_LEN;
        decoded[nonce_start] ^= 0xFF;
        let tampered = BASE64.encode(&decoded);
        assert!(decrypt(&tampered, "password").is_err());
    }

    #[test]
    fn test_invalid_base64() {
        assert!(decrypt("not-valid-base64!!!", "password").is_err());
    }

    #[test]
    fn test_empty_plaintext() {
        let encrypted = encrypt("", "password").unwrap();
        let decrypted = decrypt(&encrypted, "password").unwrap();
        assert_eq!(decrypted, "");
    }

    /// A backup written in the pre-versioning layout (`salt + nonce + ct`, KDF
    /// v1, no magic prefix) must still restore.
    #[test]
    fn test_legacy_unversioned_blob_decrypts() {
        let plaintext = "legacy seed phrase words";
        let password = "legacy_password";

        // Reproduce the old on-disk format by hand using v1 parameters.
        let mut salt = vec![0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt[..]);
        let key = Argon2Params::for_version(1)
            .unwrap()
            .derive(password, &salt)
            .unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let mut nonce_bytes = vec![0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes[..]);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher.encrypt(nonce, plaintext.as_bytes()).unwrap();

        let mut legacy = Vec::new();
        legacy.extend_from_slice(&salt);
        legacy.extend_from_slice(&nonce_bytes);
        legacy.extend_from_slice(&ct);
        let legacy_b64 = BASE64.encode(&legacy);

        let decrypted = decrypt(&legacy_b64, password).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
