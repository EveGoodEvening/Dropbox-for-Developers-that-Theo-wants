//! Secret encryption: encrypt env var values with the workspace secret key.
//!
//! Uses AES-256-GCM with associated data binding the value to its metadata
//! (workspace ID, env var ID, name, environment) to prevent substitution
//! attacks.

use base64::Engine;
use ring::aead;

use crate::keys::WorkspaceSecretKey;

/// Error returned by secret encryption/decryption.
#[derive(Debug, thiserror::Error)]
pub enum SecretEncryptionError {
    /// Encryption failed.
    #[error("encryption failed: {0}")]
    Encrypt(String),
    /// Decryption failed (wrong key or tampered data).
    #[error("decryption failed: {0}")]
    Decrypt(String),
    /// Invalid base64 encoding.
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Secret encryption header (same structure as blob header).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SecretHeader {
    /// Algorithm version.
    pub version: u8,
    /// Nonce, base64-encoded.
    pub nonce: String,
    /// Algorithm name.
    pub algorithm: String,
}

const NONCE_LEN: usize = 12;
const ALGORITHM: &str = "aes-256-gcm";
const VERSION: u8 = 1;

/// Associated data for secret encryption. Binds the ciphertext to its metadata.
#[derive(Debug, Clone)]
pub struct SecretAssociatedData<'a> {
    /// Workspace ID string.
    pub workspace_id: &'a str,
    /// Env var ID string.
    pub env_var_id: &'a str,
    /// Env var name.
    pub name: &'a str,
    /// Environment (dev, test, prod, etc.).
    pub environment: &'a str,
}

/// Encrypt a secret value using the workspace secret key.
///
/// Returns `(ciphertext_base64, header)`.
///
/// # Errors
/// Returns [`SecretEncryptionError::Encrypt`] if encryption fails.
pub fn encrypt_secret(
    key: &WorkspaceSecretKey,
    plaintext: &[u8],
    aad: &SecretAssociatedData<'_>,
) -> Result<(String, SecretHeader), SecretEncryptionError> {
    let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, key.as_bytes())
        .map_err(|e| SecretEncryptionError::Encrypt(e.to_string()))?;
    let sealing_key = aead::LessSafeKey::new(unbound_key);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);

    // Build associated data: workspace_id|env_var_id|name|environment
    let aad_bytes = build_aad(aad);

    let mut in_out = plaintext.to_vec();
    sealing_key
        .seal_in_place_append_tag(nonce, aead::Aad::from(aad_bytes), &mut in_out)
        .map_err(|e| SecretEncryptionError::Encrypt(e.to_string()))?;

    let header = SecretHeader {
        version: VERSION,
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        algorithm: ALGORITHM.to_owned(),
    };

    let ciphertext_b64 = base64::engine::general_purpose::STANDARD.encode(&in_out);
    Ok((ciphertext_b64, header))
}

/// Decrypt a secret value using the workspace secret key.
///
/// # Errors
/// Returns [`SecretEncryptionError::Decrypt`] if decryption fails.
pub fn decrypt_secret(
    key: &WorkspaceSecretKey,
    ciphertext_b64: &str,
    header: &SecretHeader,
    aad: &SecretAssociatedData<'_>,
) -> Result<Vec<u8>, SecretEncryptionError> {
    if header.algorithm != ALGORITHM {
        return Err(SecretEncryptionError::Decrypt(format!(
            "unsupported algorithm: {}",
            header.algorithm
        )));
    }
    let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, key.as_bytes())
        .map_err(|e| SecretEncryptionError::Decrypt(e.to_string()))?;
    let opening_key = aead::LessSafeKey::new(unbound_key);

    let nonce_bytes = base64::engine::general_purpose::STANDARD.decode(&header.nonce)?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(SecretEncryptionError::Decrypt(
            "invalid nonce length".to_owned(),
        ));
    }
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(&nonce_bytes);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_arr);

    let mut ciphertext = base64::engine::general_purpose::STANDARD.decode(ciphertext_b64)?;
    let aad_bytes = build_aad(aad);

    let plaintext = opening_key
        .open_in_place(nonce, aead::Aad::from(aad_bytes), &mut ciphertext)
        .map_err(|_| SecretEncryptionError::Decrypt("authentication failed".to_owned()))?;

    Ok(plaintext.to_vec())
}

fn build_aad(aad: &SecretAssociatedData<'_>) -> Vec<u8> {
    format!(
        "{}|{}|{}|{}",
        aad.workspace_id, aad.env_var_id, aad.name, aad.environment
    )
    .into_bytes()
}

use rand::RngCore;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_aad() -> SecretAssociatedData<'static> {
        SecretAssociatedData {
            workspace_id: "ws-123",
            env_var_id: "env-456",
            name: "STRIPE_SECRET_KEY",
            environment: "dev",
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = WorkspaceSecretKey::generate();
        let plaintext = b"sk_test_12345secret";
        let aad = test_aad();
        let (ciphertext, header) = encrypt_secret(&key, plaintext, &aad).unwrap();
        let decrypted = decrypt_secret(&key, &ciphertext, &header, &aad).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = WorkspaceSecretKey::generate();
        let key2 = WorkspaceSecretKey::generate();
        let aad = test_aad();
        let (ciphertext, header) = encrypt_secret(&key1, b"secret", &aad).unwrap();
        assert!(decrypt_secret(&key2, &ciphertext, &header, &aad).is_err());
    }

    #[test]
    fn wrong_aad_fails() {
        let key = WorkspaceSecretKey::generate();
        let aad = test_aad();
        let (ciphertext, header) = encrypt_secret(&key, b"secret", &aad).unwrap();
        // Use different AAD for decryption.
        let wrong_aad = SecretAssociatedData {
            workspace_id: "ws-999",
            env_var_id: "env-456",
            name: "STRIPE_SECRET_KEY",
            environment: "dev",
        };
        assert!(decrypt_secret(&key, &ciphertext, &header, &wrong_aad).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = WorkspaceSecretKey::generate();
        let aad = test_aad();
        let (ciphertext_b64, header) = encrypt_secret(&key, b"secret", &aad).unwrap();
        // Flip a bit in the decoded ciphertext.
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(&ciphertext_b64)
            .unwrap();
        bytes[0] ^= 0xff;
        let tampered = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert!(decrypt_secret(&key, &tampered, &header, &aad).is_err());
    }
}
