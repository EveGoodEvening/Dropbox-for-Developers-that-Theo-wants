//! Crypto primitives: blob encryption, secret encryption, key model.
//!
//! Uses `ring` for authenticated encryption (AES-256-GCM) and `rand` for
//! CSPRNG key generation. Keys are never stored in plaintext config files;
//! they live in the OS keychain or an encrypted file fallback.

pub mod blob;
pub mod hash;
pub mod keys;
pub mod secret;

pub use blob::{decrypt_blob, encrypt_blob, BlobEncryptionError};
pub use hash::{compute_blob_id, verify_blob_id};
pub use keys::{WorkspaceContentKey, WorkspaceSecretKey};
pub use secret::{decrypt_secret, encrypt_secret, SecretEncryptionError};
