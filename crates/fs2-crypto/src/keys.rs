//! Workspace key types.
//!
//! Keys are generated with a CSPRNG and stored in the OS keychain or an
//! encrypted file fallback. They are never stored in plaintext config files.

use rand::RngCore;

/// Workspace content key (WCK) — used for file blob encryption.
///
/// 256-bit AES-GCM key.
#[derive(Clone)]
pub struct WorkspaceContentKey(pub [u8; 32]);

impl WorkspaceContentKey {
    /// Generate a new random content key.
    #[must_use]
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self(key)
    }

    /// Create from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for WorkspaceContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WorkspaceContentKey")
            .field(&"[redacted]")
            .finish()
    }
}

/// Workspace secret key (WSK) — used for env var value encryption.
///
/// 256-bit AES-GCM key.
#[derive(Clone)]
pub struct WorkspaceSecretKey(pub [u8; 32]);

impl WorkspaceSecretKey {
    /// Generate a new random secret key.
    #[must_use]
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self(key)
    }

    /// Create from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for WorkspaceSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WorkspaceSecretKey")
            .field(&"[redacted]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_random() {
        let k1 = WorkspaceContentKey::generate();
        let k2 = WorkspaceContentKey::generate();
        assert_ne!(k1.0, k2.0);
    }

    #[test]
    fn secret_keys_are_random() {
        let k1 = WorkspaceSecretKey::generate();
        let k2 = WorkspaceSecretKey::generate();
        assert_ne!(k1.0, k2.0);
    }

    #[test]
    fn debug_does_not_leak_key() {
        let key = WorkspaceContentKey::generate();
        let debug = format!("{key:?}");
        assert!(!debug.contains("redacted") || debug.contains("[redacted]"));
        assert!(!debug.contains(&hex::encode(&key.0)));
    }

    #[test]
    fn from_bytes_roundtrip() {
        let bytes = [42u8; 32];
        let key = WorkspaceContentKey::from_bytes(bytes);
        assert_eq!(key.as_bytes(), &bytes);
    }
}

// Minimal hex encoding for tests (avoids adding a hex dependency).
#[cfg(test)]
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            write!(s, "{b:02x}").unwrap();
        }
        s
    }
}
