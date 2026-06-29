//! Blob store abstraction.
//!
//! Defines a trait for object storage and provides a local filesystem
//! implementation for development and testing.

use std::path::PathBuf;

use anyhow::Result;

/// Blob store abstraction.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Put bytes at the given key.
    async fn put(&self, key: &str, bytes: bytes::Bytes) -> Result<()>;

    /// Get bytes at the given key.
    async fn get(&self, key: &str) -> Result<bytes::Bytes>;

    /// Check if a key exists.
    async fn exists(&self, key: &str) -> Result<bool>;
}

/// Local filesystem blob store.
///
/// Blobs are stored as files under a root directory. The key is used as a
/// relative path (with `/` separators), sharded by the first two characters
/// to avoid huge flat directories.
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    /// Create a new local blob store rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Compute the filesystem path for a blob key.
    fn key_path(&self, key: &str) -> PathBuf {
        // Shard by first 2 chars: "sha256:abcd..." -> "sh/abcd..."
        let sanitized = key.replace(':', "/");
        let (prefix, rest) = if sanitized.len() >= 2 {
            (&sanitized[..2], &sanitized[2..])
        } else {
            ("_", sanitized.as_str())
        };
        self.root.join(prefix).join(rest)
    }
}

#[async_trait::async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, key: &str, bytes: bytes::Bytes) -> Result<()> {
        let path = self.key_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, &bytes).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes> {
        let path = self.key_path(key);
        let data = tokio::fs::read(&path).await?;
        Ok(bytes::Bytes::from(data))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.key_path(key);
        Ok(path.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn put_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = LocalBlobStore::new(tmp.path());
        let key = "sha256:abcd1234";
        let data = bytes::Bytes::from(b"hello blob".to_vec());
        store.put(key, data.clone()).await.unwrap();
        assert!(store.exists(key).await.unwrap());
        let retrieved = store.get(key).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn get_nonexistent_fails() {
        let tmp = TempDir::new().unwrap();
        let store = LocalBlobStore::new(tmp.path());
        assert!(store.get("sha256:nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn exists_nonexistent_returns_false() {
        let tmp = TempDir::new().unwrap();
        let store = LocalBlobStore::new(tmp.path());
        assert!(!store.exists("sha256:nonexistent").await.unwrap());
    }

    #[tokio::test]
    async fn multiple_blobs() {
        let tmp = TempDir::new().unwrap();
        let store = LocalBlobStore::new(tmp.path());
        for i in 0..10 {
            let key = format!("sha256:{i:064x}");
            let data = bytes::Bytes::from(vec![i as u8; 100]);
            store.put(&key, data.clone()).await.unwrap();
            let retrieved = store.get(&key).await.unwrap();
            assert_eq!(retrieved, data);
        }
    }
}
