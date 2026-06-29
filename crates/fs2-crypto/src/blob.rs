//! Blob encryption: encrypt file content before upload, decrypt after download.
//!
//! Uses AES-256-GCM via `ring` for authenticated encryption. Each blob gets a
//! fresh random nonce. The encryption header (nonce) is stored alongside the
//! ciphertext.

use base64::Engine;
use ring::aead;

use crate::keys::WorkspaceContentKey;

/// Error returned by blob encryption/decryption.
#[derive(Debug, thiserror::Error)]
pub enum BlobEncryptionError {
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

/// Versioned encryption header. Contains the nonce and algorithm version.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptionHeader {
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

/// Encrypt plaintext bytes using the workspace content key.
///
/// Returns `(ciphertext, encryption_header)` where the header contains the
/// base64-encoded nonce.
///
/// # Errors
/// Returns [`BlobEncryptionError::Encrypt`] if encryption fails.
pub fn encrypt_blob(
    key: &WorkspaceContentKey,
    plaintext: &[u8],
) -> Result<(Vec<u8>, EncryptionHeader), BlobEncryptionError> {
    let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, key.as_bytes())
        .map_err(|e| BlobEncryptionError::Encrypt(e.to_string()))?;
    let sealing_key = aead::LessSafeKey::new(unbound_key);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plaintext.to_vec();
    sealing_key
        .seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut in_out)
        .map_err(|e| BlobEncryptionError::Encrypt(e.to_string()))?;

    let header = EncryptionHeader {
        version: VERSION,
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        algorithm: ALGORITHM.to_owned(),
    };

    Ok((in_out, header))
}

/// Decrypt ciphertext using the workspace content key and encryption header.
///
/// # Errors
/// Returns [`BlobEncryptionError::Decrypt`] if decryption fails (wrong key,
/// tampered data, or invalid nonce).
pub fn decrypt_blob(
    key: &WorkspaceContentKey,
    ciphertext: &[u8],
    header: &EncryptionHeader,
) -> Result<Vec<u8>, BlobEncryptionError> {
    if header.algorithm != ALGORITHM {
        return Err(BlobEncryptionError::Decrypt(format!(
            "unsupported algorithm: {}",
            header.algorithm
        )));
    }
    let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, key.as_bytes())
        .map_err(|e| BlobEncryptionError::Decrypt(e.to_string()))?;
    let opening_key = aead::LessSafeKey::new(unbound_key);

    let nonce_bytes = base64::engine::general_purpose::STANDARD.decode(&header.nonce)?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(BlobEncryptionError::Decrypt(
            "invalid nonce length".to_owned(),
        ));
    }
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(&nonce_bytes);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_arr);

    let mut ciphertext_owned = ciphertext.to_vec();
    let plaintext = opening_key
        .open_in_place(nonce, aead::Aad::empty(), &mut ciphertext_owned)
        .map_err(|_| BlobEncryptionError::Decrypt("authentication failed".to_owned()))?;

    Ok(plaintext.to_vec())
}

use rand::RngCore;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = WorkspaceContentKey::generate();
        let plaintext = b"hello world, this is file content";
        let (ciphertext, header) = encrypt_blob(&key, plaintext).unwrap();
        assert_ne!(&ciphertext[..plaintext.len()], plaintext);
        let decrypted = decrypt_blob(&key, &ciphertext, &header).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = WorkspaceContentKey::generate();
        let key2 = WorkspaceContentKey::generate();
        let (ciphertext, header) = encrypt_blob(&key1, b"secret data").unwrap();
        assert!(decrypt_blob(&key2, &ciphertext, &header).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = WorkspaceContentKey::generate();
        let (mut ciphertext, header) = encrypt_blob(&key, b"original data").unwrap();
        // Flip a byte in the ciphertext.
        ciphertext[0] ^= 0xff;
        assert!(decrypt_blob(&key, &ciphertext, &header).is_err());
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let key = WorkspaceContentKey::generate();
        let (ciphertext, header) = encrypt_blob(&key, b"").unwrap();
        let decrypted = decrypt_blob(&key, &ciphertext, &header).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn large_plaintext_roundtrip() {
        let key = WorkspaceContentKey::generate();
        let plaintext: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let (ciphertext, header) = encrypt_blob(&key, &plaintext).unwrap();
        let decrypted = decrypt_blob(&key, &ciphertext, &header).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn header_has_correct_algorithm() {
        let key = WorkspaceContentKey::generate();
        let (_, header) = encrypt_blob(&key, b"data").unwrap();
        assert_eq!(header.algorithm, "aes-256-gcm");
        assert_eq!(header.version, 1);
    }
}
