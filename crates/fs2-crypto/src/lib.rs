#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Workspace key types and authenticated encryption helpers.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use fs2_core::{BlobId, EnvVarId, WorkspaceId};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, fs, io, path::PathBuf};
use zeroize::Zeroize;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const ENCRYPTION_VERSION: u8 = 1;
const XCHACHA20_POLY1305: &str = "xchacha20poly1305";
pub const DEV_KEYSTORE_WARNING: &str =
    "development encrypted file key store; prefer OS keychain in production";

/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-crypto"
}

#[derive(Clone, PartialEq, Eq)]
struct SecretBytes([u8; KEY_LEN]);

impl SecretBytes {
    fn generate() -> Self {
        let mut bytes = [0_u8; KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    const fn as_slice(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

macro_rules! key_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name(SecretBytes);

        impl $name {
            #[must_use]
            pub fn generate() -> Self {
                Self(SecretBytes::generate())
            }

            #[must_use]
            pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
                Self(SecretBytes::from_bytes(bytes))
            }

            #[must_use]
            pub const fn expose_for_test(&self) -> &[u8; KEY_LEN] {
                self.0.as_slice()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }
    };
}

key_type!(
    WorkspaceKeyEncryptionKey,
    "Workspace key-encryption key used to wrap workspace keys for a local key store."
);
key_type!(
    WorkspaceContentKey,
    "Workspace content-encryption key used for file blob encryption."
);
key_type!(
    WorkspaceSecretKey,
    "Workspace secret-encryption key used for environment value encryption."
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeys {
    pub content: WorkspaceContentKey,
    pub secret: WorkspaceSecretKey,
}

impl WorkspaceKeys {
    #[must_use]
    pub fn generate() -> Self {
        Self {
            content: WorkspaceContentKey::generate(),
            secret: WorkspaceSecretKey::generate(),
        }
    }
}

#[derive(Debug)]
pub enum CryptoError {
    InvalidHeader(String),
    InvalidKey(String),
    Aead(String),
    Io(io::Error),
    Json(serde_json::Error),
    BlobId(fs2_core::ParseFs2IdError),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHeader(message) | Self::InvalidKey(message) | Self::Aead(message) => {
                formatter.write_str(message)
            }
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::BlobId(error) => write!(formatter, "blob id error: {error}"),
        }
    }
}

impl std::error::Error for CryptoError {}

impl From<io::Error> for CryptoError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CryptoError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<fs2_core::ParseFs2IdError> for CryptoError {
    fn from(error: fs2_core::ParseFs2IdError) -> Self {
        Self::BlobId(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionHeader {
    pub version: u8,
    pub algorithm: String,
    pub nonce: String,
    pub aad: String,
}

impl EncryptionHeader {
    fn new(nonce: &[u8; NONCE_LEN], aad: impl Into<String>) -> Self {
        Self {
            version: ENCRYPTION_VERSION,
            algorithm: XCHACHA20_POLY1305.to_owned(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            aad: aad.into(),
        }
    }

    fn nonce_bytes(&self) -> Result<[u8; NONCE_LEN], CryptoError> {
        if self.version != ENCRYPTION_VERSION {
            return Err(CryptoError::InvalidHeader(
                "unsupported encryption header version".to_owned(),
            ));
        }
        if self.algorithm != XCHACHA20_POLY1305 {
            return Err(CryptoError::InvalidHeader(
                "unsupported encryption algorithm".to_owned(),
            ));
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(&self.nonce)
            .map_err(|error| CryptoError::InvalidHeader(error.to_string()))?;
        nonce.try_into().map_err(|_| {
            CryptoError::InvalidHeader("xchacha20poly1305 nonce must be 24 bytes".to_owned())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedBlob {
    pub blob_id: BlobId,
    pub header: EncryptionHeader,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedSecret {
    pub header: EncryptionHeader,
    pub ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvSecretAssociatedData {
    pub workspace_id: WorkspaceId,
    pub env_var_id: EnvVarId,
    pub env_var_name: String,
    pub environment: String,
}

pub trait WorkspaceKeyStore {
    fn save_workspace_keys(
        &self,
        workspace_id: WorkspaceId,
        keys: &WorkspaceKeys,
    ) -> Result<(), CryptoError>;

    fn load_workspace_keys(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<WorkspaceKeys>, CryptoError>;
}

#[derive(Debug, Clone)]
pub struct DevEncryptedFileKeyStore {
    root: PathBuf,
    wrapping_key: WorkspaceKeyEncryptionKey,
}

impl DevEncryptedFileKeyStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, wrapping_key: WorkspaceKeyEncryptionKey) -> Self {
        Self {
            root: root.into(),
            wrapping_key,
        }
    }

    fn path_for(&self, workspace_id: WorkspaceId) -> PathBuf {
        self.root.join(format!("{workspace_id}.keys.json.enc"))
    }
}

impl WorkspaceKeyStore for DevEncryptedFileKeyStore {
    fn save_workspace_keys(
        &self,
        workspace_id: WorkspaceId,
        keys: &WorkspaceKeys,
    ) -> Result<(), CryptoError> {
        fs::create_dir_all(&self.root)?;
        let plaintext = serde_json::to_vec(&SerializableWorkspaceKeys::from(keys))?;
        let aad = key_file_aad(workspace_id);
        let (header, ciphertext) = encrypt_bytes(&plaintext, self.wrapping_key.0.as_slice(), &aad)?;
        let file = DevKeyFile {
            warning: DEV_KEYSTORE_WARNING.to_owned(),
            header,
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        fs::write(
            self.path_for(workspace_id),
            serde_json::to_vec_pretty(&file)?,
        )?;
        Ok(())
    }

    fn load_workspace_keys(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<WorkspaceKeys>, CryptoError> {
        let path = self.path_for(workspace_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(CryptoError::Io(error)),
        };
        let file = serde_json::from_slice::<DevKeyFile>(&bytes)?;
        if file.warning != DEV_KEYSTORE_WARNING {
            return Err(CryptoError::InvalidHeader(
                "development key file warning mismatch".to_owned(),
            ));
        }
        let ciphertext = URL_SAFE_NO_PAD
            .decode(file.ciphertext)
            .map_err(|error| CryptoError::InvalidHeader(error.to_string()))?;
        let plaintext = decrypt_bytes(
            &ciphertext,
            self.wrapping_key.0.as_slice(),
            &file.header,
            &key_file_aad(workspace_id),
        )?;
        let keys = serde_json::from_slice::<SerializableWorkspaceKeys>(&plaintext)?;
        Ok(Some(keys.try_into()?))
    }
}

pub fn encrypt_blob(
    plaintext: &[u8],
    key: &WorkspaceContentKey,
) -> Result<EncryptedBlob, CryptoError> {
    let (header, ciphertext) = encrypt_bytes(plaintext, key.0.as_slice(), "fs2:blob:v1")?;
    let blob_id = BlobId::new(format!(
        "sha256:{}",
        hex_lower(&Sha256::digest(&ciphertext))
    ))?;
    Ok(EncryptedBlob {
        blob_id,
        header,
        ciphertext,
    })
}

pub fn decrypt_blob(
    blob: &EncryptedBlob,
    key: &WorkspaceContentKey,
) -> Result<Vec<u8>, CryptoError> {
    let expected = BlobId::new(format!(
        "sha256:{}",
        hex_lower(&Sha256::digest(&blob.ciphertext))
    ))?;
    if blob.blob_id != expected {
        return Err(CryptoError::Aead(
            "blob ciphertext hash does not match blob id".to_owned(),
        ));
    }
    decrypt_bytes(
        &blob.ciphertext,
        key.0.as_slice(),
        &blob.header,
        "fs2:blob:v1",
    )
}

pub fn encrypt_env_value(
    value: &str,
    key: &WorkspaceSecretKey,
    associated_data: &EnvSecretAssociatedData,
) -> Result<EncryptedSecret, CryptoError> {
    let aad = serde_json::to_string(associated_data)?;
    let (header, ciphertext) = encrypt_bytes(value.as_bytes(), key.0.as_slice(), &aad)?;
    Ok(EncryptedSecret {
        header,
        ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
    })
}

pub fn decrypt_env_value(
    secret: &EncryptedSecret,
    key: &WorkspaceSecretKey,
    associated_data: &EnvSecretAssociatedData,
) -> Result<String, CryptoError> {
    let aad = serde_json::to_string(associated_data)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&secret.ciphertext)
        .map_err(|error| CryptoError::InvalidHeader(error.to_string()))?;
    let plaintext = decrypt_bytes(&ciphertext, key.0.as_slice(), &secret.header, &aad)?;
    String::from_utf8(plaintext).map_err(|error| CryptoError::Aead(error.to_string()))
}

fn encrypt_bytes(
    plaintext: &[u8],
    key: &[u8; KEY_LEN],
    aad: &str,
) -> Result<(EncryptionHeader, Vec<u8>), CryptoError> {
    let mut nonce = [0_u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|error| CryptoError::InvalidKey(error.to_string()))?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|error| CryptoError::Aead(error.to_string()))?;
    Ok((EncryptionHeader::new(&nonce, aad), ciphertext))
}

fn decrypt_bytes(
    ciphertext: &[u8],
    key: &[u8; KEY_LEN],
    header: &EncryptionHeader,
    expected_aad: &str,
) -> Result<Vec<u8>, CryptoError> {
    if header.aad != expected_aad {
        return Err(CryptoError::InvalidHeader(
            "associated data mismatch".to_owned(),
        ));
    }
    let nonce = header.nonce_bytes()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|error| CryptoError::InvalidKey(error.to_string()))?;
    cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: expected_aad.as_bytes(),
            },
        )
        .map_err(|error| CryptoError::Aead(error.to_string()))
}

fn key_file_aad(workspace_id: WorkspaceId) -> String {
    format!("fs2:dev-key-file:v1:{workspace_id}")
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializableWorkspaceKeys {
    content: String,
    secret: String,
}

impl From<&WorkspaceKeys> for SerializableWorkspaceKeys {
    fn from(value: &WorkspaceKeys) -> Self {
        Self {
            content: URL_SAFE_NO_PAD.encode(value.content.0.as_slice()),
            secret: URL_SAFE_NO_PAD.encode(value.secret.0.as_slice()),
        }
    }
}

impl TryFrom<SerializableWorkspaceKeys> for WorkspaceKeys {
    type Error = CryptoError;

    fn try_from(value: SerializableWorkspaceKeys) -> Result<Self, Self::Error> {
        Ok(Self {
            content: WorkspaceContentKey::from_bytes(decode_key(&value.content)?),
            secret: WorkspaceSecretKey::from_bytes(decode_key(&value.secret)?),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DevKeyFile {
    warning: String,
    header: EncryptionHeader,
    ciphertext: String,
}

fn decode_key(value: &str) -> Result<[u8; KEY_LEN], CryptoError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|error| CryptoError::InvalidKey(error.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| CryptoError::InvalidKey("workspace keys must be 32 bytes".to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-crypto");
    }

    #[test]
    fn generated_keys_are_redacted_in_debug() {
        let key = WorkspaceContentKey::generate();
        let debug = format!("{key:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&URL_SAFE_NO_PAD.encode(key.expose_for_test())));
    }

    #[test]
    fn blob_encryption_round_trips_and_rejects_tampering() -> Result<(), Box<dyn std::error::Error>>
    {
        let key = WorkspaceContentKey::generate();
        let encrypted = encrypt_blob(b"source bytes", &key)?;

        assert_ne!(encrypted.ciphertext, b"source bytes");
        assert_eq!(decrypt_blob(&encrypted, &key)?, b"source bytes");

        let mut tampered = encrypted;
        tampered.ciphertext[0] ^= 0x01;
        assert!(decrypt_blob(&tampered, &key).is_err());
        Ok(())
    }

    #[test]
    fn env_secret_uses_associated_data_and_wrong_key_fails(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let key = WorkspaceSecretKey::generate();
        let wrong_key = WorkspaceSecretKey::generate();
        let aad = env_aad("STRIPE_SECRET_KEY", "dev");
        let encrypted = encrypt_env_value("sk_test_fake", &key, &aad)?;

        assert!(!encrypted.ciphertext.contains("sk_test_fake"));
        assert_eq!(decrypt_env_value(&encrypted, &key, &aad)?, "sk_test_fake");
        assert!(decrypt_env_value(&encrypted, &wrong_key, &aad).is_err());

        let wrong_aad = env_aad("STRIPE_SECRET_KEY", "prod");
        assert!(decrypt_env_value(&encrypted, &key, &wrong_aad).is_err());
        Ok(())
    }

    #[test]
    fn encrypted_file_key_store_does_not_write_plaintext_keys(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let workspace_id = WorkspaceId::new_v4();
        let wrapping_key = WorkspaceKeyEncryptionKey::generate();
        let keys = WorkspaceKeys::generate();
        let store = DevEncryptedFileKeyStore::new(dir.path(), wrapping_key);

        store.save_workspace_keys(workspace_id, &keys)?;
        let file = fs::read_to_string(dir.path().join(format!("{workspace_id}.keys.json.enc")))?;

        assert!(file.contains(DEV_KEYSTORE_WARNING));
        assert!(!file.contains(&URL_SAFE_NO_PAD.encode(keys.content.expose_for_test())));
        assert!(!file.contains(&URL_SAFE_NO_PAD.encode(keys.secret.expose_for_test())));

        let loaded = store
            .load_workspace_keys(workspace_id)?
            .ok_or("missing keys")?;
        assert_eq!(
            loaded.content.expose_for_test(),
            keys.content.expose_for_test()
        );
        assert_eq!(
            loaded.secret.expose_for_test(),
            keys.secret.expose_for_test()
        );
        Ok(())
    }

    #[test]
    fn encrypted_file_key_store_distinguishes_missing_from_inaccessible(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let workspace_id = WorkspaceId::new_v4();
        let store =
            DevEncryptedFileKeyStore::new(dir.path(), WorkspaceKeyEncryptionKey::generate());
        assert!(store.load_workspace_keys(workspace_id)?.is_none());

        let root_file = dir.path().join("not-a-directory");
        fs::write(&root_file, "not a directory")?;
        let inaccessible =
            DevEncryptedFileKeyStore::new(root_file, WorkspaceKeyEncryptionKey::generate());
        assert!(inaccessible.load_workspace_keys(workspace_id).is_err());
        Ok(())
    }

    fn env_aad(name: &str, environment: &str) -> EnvSecretAssociatedData {
        EnvSecretAssociatedData {
            workspace_id: WorkspaceId::new_v4(),
            env_var_id: EnvVarId::new_v4(),
            env_var_name: name.to_owned(),
            environment: environment.to_owned(),
        }
    }
}
