#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Sync-side helpers that prepare local data for backend upload.

use fs2_core::BlobId;
use fs2_crypto::{encrypt_blob, CryptoError, EncryptionHeader, WorkspaceContentKey};

/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-sync"
}

/// Ciphertext-only payload that may be sent to the object store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobUploadPlan {
    pub blob_id: BlobId,
    pub ciphertext: Vec<u8>,
    pub plaintext_size: u64,
    pub encryption_header: EncryptionHeader,
}

impl BlobUploadPlan {
    /// Encrypts plaintext before producing any uploadable object-store payload.
    pub fn from_plaintext(
        plaintext: &[u8],
        content_key: &WorkspaceContentKey,
    ) -> Result<Self, CryptoError> {
        let encrypted = encrypt_blob(plaintext, content_key)?;
        Ok(Self {
            blob_id: encrypted.blob_id,
            ciphertext: encrypted.ciphertext,
            plaintext_size: plaintext.len() as u64,
            encryption_header: encrypted.header,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2_crypto::{decrypt_blob, EncryptedBlob};

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-sync");
    }

    #[test]
    fn blob_upload_plan_never_contains_plaintext_payload() -> Result<(), CryptoError> {
        let key = WorkspaceContentKey::from_bytes([7; 32]);
        let plaintext = b"known secret source bytes";

        let plan = BlobUploadPlan::from_plaintext(plaintext, &key)?;

        assert_ne!(plan.ciphertext, plaintext);
        assert!(!plan
            .ciphertext
            .windows(plaintext.len())
            .any(|window| window == plaintext));
        assert_eq!(plan.plaintext_size, plaintext.len() as u64);
        assert_eq!(plan.encryption_header.aad, "fs2:blob:v1");
        assert_eq!(plan.blob_id.as_str(), plan.blob_id.to_string());
        let decrypted = decrypt_blob(
            &EncryptedBlob {
                blob_id: plan.blob_id,
                header: plan.encryption_header,
                ciphertext: plan.ciphertext,
            },
            &key,
        )?;
        assert_eq!(decrypted, plaintext);
        Ok(())
    }
}
