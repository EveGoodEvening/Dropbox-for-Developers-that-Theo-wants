# Safety Guide — fs2-devsync

This guide explains the security model, what is protected, and what is not.

## What is protected

### File content encryption
File content is encrypted client-side before upload using AES-256-GCM with a
workspace content key. The object store never receives plaintext file content
in normal mode.

### Secret encryption
Environment variable values are encrypted client-side using AES-256-GCM with
a workspace secret key. The encryption includes associated data (workspace ID,
env var ID, name, environment) to prevent substitution attacks. The backend
stores only encrypted envelopes and cannot decrypt values.

### Auth tokens
Access tokens are short-lived JWTs. Refresh tokens are device-bound. Tokens
are stored in `~/.fs2/config.json` for development convenience. In production,
tokens should be stored in the OS keychain.

### Blob hash verification
Downloaded blobs are verified against their content hash (SHA-256) before
being served. Corrupt or tampered blobs are rejected.

## What is NOT protected in MVP

### Metadata is plaintext
The backend stores plaintext metadata: filenames, paths, sizes, mtimes,
directory structure. Content and secrets are encrypted, but **metadata is not
private from the service operator** in MVP.

### No E2EE metadata
End-to-end encrypted metadata (filenames, paths) is a future feature. It is
not implemented in MVP because it complicates conflict detection, listing,
and web management.

### Local root user
A malicious local root user can read cached files, materialized secrets, and
keys. Treat local root as trusted.

### Compromised device
A compromised enrolled device can read previously cached files and old keys.
Device revocation stops new key envelopes but does not guarantee a revoked
device cannot read previously cached data.

### Package install scripts
Materialized `.env` files are `0600` but a process running as the same user
can read them. Malicious package install scripts could read materialized
secrets.

## Secret handling rules

Secrets, tokens, decrypted env values, and private keys must **never** appear
in:
- Logs
- Panic messages
- Telemetry
- `fs2 status` output
- Conflict files
- Backend request traces
- Debug bundles

Secret values are displayed as `********` with metadata (set, updated
timestamp, scope).

## Device revocation

When a device is revoked:
- The server stops accepting its tokens.
- The server stops sending new key envelopes to it.
- Future key rotation should re-encrypt workspace keys excluding revoked
  devices.

MVP does not guarantee revoked devices cannot read previously cached files or
old keys.

## Recovery

### Lost access
If you lose access to all enrolled devices, use the account recovery key or
passphrase to re-enroll a new device.

### Conflict recovery
Conflicts preserve both versions. Use `fs2 conflicts list` to see conflicts
and `fs2 conflicts resolve` to resolve them.

### Cache corruption
If the local cache is corrupted, delete `~/.fs2/workspaces/<id>/blobs/` and
re-hydrate. Metadata is in the SQLite DB and can be rebuilt from the backend.

## Best practices

1. **Mark `.env` as secret:** Add `:secret .env` to `.fs2ignore`.
2. **Don't sync `.git`:** The default rule excludes `.git/**`. Don't override
   this unless you understand the risks.
3. **Use `fs2 doctor` regularly:** It checks for common safety issues.
4. **Pin critical files:** Pin lockfiles and config files for offline access.
5. **Review conflicts promptly:** Don't let conflict files accumulate.
