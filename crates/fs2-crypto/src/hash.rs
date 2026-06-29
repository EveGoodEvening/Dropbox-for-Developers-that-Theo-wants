//! Content addressing: hash computation and blob ID generation.
//!
//! Uses SHA-256 via `ring` for hash computation. The blob ID format is
//! `sha256:<hex_digest>` where the digest is the hash of the ciphertext
//! (for encrypted blobs) or plaintext (for unencrypted dev mode).

use ring::digest;

/// Compute the SHA-256 hash of data and return a `sha256:<hex>` blob ID.
#[must_use]
pub fn compute_blob_id(data: &[u8]) -> String {
    let hash = digest::digest(&digest::SHA256, data);
    let hex: String = {
        use std::fmt::Write;
        let mut s = String::with_capacity(hash.as_ref().len() * 2);
        for b in hash.as_ref() {
            write!(s, "{b:02x}").unwrap();
        }
        s
    };
    format!("sha256:{hex}")
}

/// Verify that data matches the given blob ID.
///
/// # Errors
/// Returns `false` if the hash does not match.
#[must_use]
pub fn verify_blob_id(data: &[u8], expected_blob_id: &str) -> bool {
    let actual = compute_blob_id(data);
    actual == expected_blob_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_blob_id_is_deterministic() {
        let id1 = compute_blob_id(b"hello");
        let id2 = compute_blob_id(b"hello");
        assert_eq!(id1, id2);
        assert!(id1.starts_with("sha256:"));
    }

    #[test]
    fn different_data_different_id() {
        let id1 = compute_blob_id(b"hello");
        let id2 = compute_blob_id(b"world");
        assert_ne!(id1, id2);
    }

    #[test]
    fn verify_matching_blob_id() {
        let id = compute_blob_id(b"test data");
        assert!(verify_blob_id(b"test data", &id));
    }

    #[test]
    fn verify_mismatched_blob_id() {
        let id = compute_blob_id(b"test data");
        assert!(!verify_blob_id(b"wrong data", &id));
    }

    #[test]
    fn empty_data_has_valid_id() {
        let id = compute_blob_id(b"");
        assert!(id.starts_with("sha256:"));
        assert!(verify_blob_id(b"", &id));
    }
}
