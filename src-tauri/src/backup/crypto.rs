use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use argon2::{Config, ThreadMode, Variant, Version};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use rand::{rngs::OsRng, RngCore};
use zeroize::Zeroizing;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("argon2 error: {0}")]
    Argon2(String),
    #[error("encryption error: {0}")]
    Encrypt(String),
    #[error("decryption error: {0}")]
    Decrypt(String),
    #[error("invalid backup format")]
    InvalidFormat,
    #[error("invalid base64")]
    InvalidBase64(#[from] base64::DecodeError),
}

/// Derive a 32-byte encryption key from a password and salt using Argon2id.
/// The returned key is wrapped in `Zeroizing` so it is wiped from memory on drop.
pub fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<Vec<u8>>, BackupError> {
    let config = Config {
        variant: Variant::Argon2id,
        version: Version::Version13,
        mem_cost: 65536,
        time_cost: 3,
        lanes: 4,
        thread_mode: ThreadMode::Parallel,
        secret: &[],
        ad: &[],
        hash_length: KEY_LEN as u32,
    };

    argon2::hash_raw(password.as_bytes(), salt, &config)
        .map(Zeroizing::new)
        .map_err(|e| BackupError::Argon2(e.to_string()))
}

/// Encrypt plaintext with AES-256-GCM using a password.
/// Returns a base64-encoded string containing: salt + nonce + ciphertext.
pub fn encrypt(plaintext: &str, password: &str) -> Result<String, BackupError> {
    let mut salt = vec![0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt[..]);

    let key = derive_key(password, &salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| BackupError::Encrypt(e.to_string()))?;

    let mut nonce_bytes = vec![0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes[..]);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| BackupError::Encrypt(e.to_string()))?;

    let mut output = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    output.extend_from_slice(&salt);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&output))
}

/// Decrypt a backup produced by `encrypt`.
pub fn decrypt(backup_b64: &str, password: &str) -> Result<String, BackupError> {
    let data = BASE64.decode(backup_b64)?;

    if data.len() < SALT_LEN + NONCE_LEN + 1 {
        return Err(BackupError::InvalidFormat);
    }

    let salt = &data[..SALT_LEN];
    let nonce_bytes = &data[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &data[SALT_LEN + NONCE_LEN..];

    let key = derive_key(password, salt)?;
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
    fn test_backup_format_length() {
        let encrypted = encrypt("seed phrase", "password").unwrap();
        let decoded = BASE64.decode(&encrypted).unwrap();
        assert!(decoded.len() > SALT_LEN + NONCE_LEN);
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
        decoded[SALT_LEN] ^= 0xFF;
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
}
