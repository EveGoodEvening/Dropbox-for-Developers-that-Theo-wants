# Security Policy — fs2-devsync

## Status

Experimental. Do not use with sensitive production data yet.

## Secret handling

- **File content** is encrypted client-side before upload to the blob store, using a workspace content key. The object store never receives plaintext file content in normal mode.
- **Environment variable values** are encrypted client-side with a workspace secret key. The backend stores only encrypted envelopes and never sees plaintext values.
- **Auth tokens** are short-lived access tokens plus device-bound refresh tokens, stored in the OS keychain (macOS Keychain / Linux Secret Service). A development fallback to an encrypted file with a clear warning is allowed.
- **Workspace keys** are stored in the OS keychain. A development encrypted-file fallback with an explicit warning is allowed. Keys are never stored in plaintext config files.

## Metadata is NOT private in MVP

The backend stores plaintext metadata: filenames, paths, sizes, mtimes, directory structure. Content and secrets are protected; metadata is **not** private from the service operator in MVP. See `docs/non-goals.md`.

## Redaction

Secrets, tokens, decrypted env values, and private keys must never appear in:
- logs
- panic messages
- telemetry
- `fs2 status` output
- conflict files
- backend request traces
- debug bundles

Secret values are displayed as `********` with metadata (set, updated timestamp, scope).

## Threat model (MVP)

Protects against:
- accidental secret leakage through Git
- server-side plaintext secret storage
- lost access tokens
- network interception (TLS)
- unauthorized device enrollment
- accidental logs containing secrets
- object-store compromise revealing file bytes (content encryption)

Not fully solved in MVP:
- hiding filenames/structure from the server
- malicious local root user
- compromised enrolled device
- malicious package install scripts reading materialized secrets

## Reporting

This is an experimental project. Report issues via the repository issue tracker.
