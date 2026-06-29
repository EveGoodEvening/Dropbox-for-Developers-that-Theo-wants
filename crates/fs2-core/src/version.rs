//! Versioning: local DB schema, backend API, and operation payload versions.
//!
//! All three are versioned to support future migrations and compatibility
//! checks.

/// Local `SQLite` schema version.
pub const LOCAL_DB_SCHEMA_VERSION: u32 = 1;

/// Backend API version.
pub const API_VERSION: u32 = 1;

/// Operation payload version (serialized in the operation JSON).
pub const OPERATION_PAYLOAD_VERSION: u32 = 1;

/// Get the current version triple.
#[must_use]
pub fn versions() -> (u32, u32, u32) {
    (
        LOCAL_DB_SCHEMA_VERSION,
        API_VERSION,
        OPERATION_PAYLOAD_VERSION,
    )
}

/// Check if a local DB schema version is supported.
#[must_use]
pub fn is_local_db_supported(version: u32) -> bool {
    version <= LOCAL_DB_SCHEMA_VERSION
}

/// Check if an API version is supported.
#[must_use]
pub fn is_api_supported(version: u32) -> bool {
    version <= API_VERSION
}

/// Error returned when a version is unsupported.
#[derive(Debug, Clone, thiserror::Error)]
pub enum VersionError {
    /// Local DB schema version is too new.
    #[error("local DB schema version {0} is newer than supported ({LOCAL_DB_SCHEMA_VERSION})")]
    LocalDbTooNew(u32),
    /// API version is too new.
    #[error("API version {0} is newer than supported ({API_VERSION})")]
    ApiTooNew(u32),
    /// Operation payload version is too new.
    #[error("operation payload version {0} is newer than supported ({OPERATION_PAYLOAD_VERSION})")]
    PayloadTooNew(u32),
}

/// Check local DB schema version.
///
/// # Errors
/// Returns [`VersionError::LocalDbTooNew`] if the version is not supported.
pub fn check_local_db(version: u32) -> Result<(), VersionError> {
    if !is_local_db_supported(version) {
        return Err(VersionError::LocalDbTooNew(version));
    }
    Ok(())
}

/// Check API version.
///
/// # Errors
/// Returns [`VersionError::ApiTooNew`] if the version is not supported.
pub fn check_api(version: u32) -> Result<(), VersionError> {
    if !is_api_supported(version) {
        return Err(VersionError::ApiTooNew(version));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_set() {
        let (db, api, payload) = versions();
        assert_eq!(db, 1);
        assert_eq!(api, 1);
        assert_eq!(payload, 1);
    }

    #[test]
    fn supported_versions() {
        assert!(is_local_db_supported(1));
        assert!(is_local_db_supported(0));
        assert!(!is_local_db_supported(2));

        assert!(is_api_supported(1));
        assert!(!is_api_supported(2));
    }

    #[test]
    fn check_returns_error_for_too_new() {
        assert!(check_local_db(2).is_err());
        assert!(check_local_db(1).is_ok());
        assert!(check_api(2).is_err());
        assert!(check_api(1).is_ok());
    }
}
