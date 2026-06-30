#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Sync-side helpers that prepare local data for backend upload.

use fs2_core::{BlobId, RuleAction, WorkspacePath};
use fs2_crypto::{encrypt_blob, CryptoError, EncryptionHeader, WorkspaceContentKey};
use fs2_rules::{EvaluationPurpose, RuleEngine, RulePathKind};

#[derive(Debug)]
pub enum UploadPlanningError {
    Rules(fs2_rules::RuleError),
    Crypto(CryptoError),
}

impl std::fmt::Display for UploadPlanningError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rules(error) => write!(formatter, "rule evaluation failed: {error}"),
            Self::Crypto(error) => write!(formatter, "blob encryption failed: {error}"),
        }
    }
}

impl std::error::Error for UploadPlanningError {}

impl From<fs2_rules::RuleError> for UploadPlanningError {
    fn from(error: fs2_rules::RuleError) -> Self {
        Self::Rules(error)
    }
}

impl From<CryptoError> for UploadPlanningError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

pub fn plan_file_upload(
    path: &WorkspacePath,
    plaintext: &[u8],
    content_key: &WorkspaceContentKey,
    rules: &RuleEngine,
) -> Result<Option<BlobUploadPlan>, UploadPlanningError> {
    if contains_git_segment(path) {
        return Ok(None);
    }
    let resolution = rules.resolve(
        path,
        RulePathKind::File,
        EvaluationPurpose::NewLocalCreate,
        None,
    )?;
    if !is_uploadable_action(resolution.effective_rule.action) {
        return Ok(None);
    }
    BlobUploadPlan::from_plaintext(plaintext, content_key)
        .map(Some)
        .map_err(UploadPlanningError::from)
}

const fn is_uploadable_action(action: RuleAction) -> bool {
    matches!(
        action,
        RuleAction::Normal | RuleAction::Lazy | RuleAction::Pin
    )
}

fn contains_git_segment(path: &WorkspacePath) -> bool {
    path.segments().any(|segment| segment == ".git")
}
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

    #[test]
    fn upload_planning_skips_git_internals_by_default() -> Result<(), Box<dyn std::error::Error>> {
        let key = WorkspaceContentKey::from_bytes([9; 32]);
        let rules = RuleEngine::new(fs2_rules::Config::default(), Vec::new())?;
        for path in [
            ".git/index",
            ".git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack",
            "vendor/lib/.git/index",
            ".git",
            "vendor/lib/.git",
        ] {
            let path = WorkspacePath::parse(path)?;
            assert_eq!(plan_file_upload(&path, b"git bytes", &key, &rules)?, None);
        }
        Ok(())
    }

    #[test]
    fn upload_planning_allows_normal_files() -> Result<(), Box<dyn std::error::Error>> {
        let key = WorkspaceContentKey::from_bytes([10; 32]);
        let rules = RuleEngine::new(fs2_rules::Config::default(), Vec::new())?;
        let path = WorkspacePath::parse("src/lib.rs")?;

        let plan = plan_file_upload(&path, b"source bytes", &key, &rules)?;

        assert!(plan.is_some());
        Ok(())
    }
}
