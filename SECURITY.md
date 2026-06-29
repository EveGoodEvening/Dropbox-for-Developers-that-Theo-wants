# FS2 security policy

## Secret handling

FS2 synchronizes developer files and environment variables across machines.
The following invariants are enforced by the implementation and verified by
tests.

### What is encrypted

- **File content:** encrypted client-side with a workspace content key before
  blob upload. Object store never receives plaintext file content in normal
  mode.
- **Env var values:** encrypted with a workspace secret key. The backend
  stores only encrypted envelopes and cannot decrypt values.
- **Auth tokens:** stored in the OS keychain (macOS Keychain, Linux Secret
  Service). An encrypted file fallback is allowed for development with an
  explicit warning.

### What is NOT encrypted in MVP

- **Metadata:** filenames, paths, sizes, mtimes, executable bits, symlink
  targets, and directory structure are stored in plaintext on the backend.
  The service operator can observe them. End-to-end encrypted metadata is a
  post-MVP goal (see `docs/non-goals.md`).

### Redaction rules

The following must never appear in logs, panic messages, telemetry,
`fs2 status`, conflict files, or backend request traces:

- secret env var values
- decrypted file content
- auth tokens
- private keys
- workspace content/secret keys

Secret values are displayed as `********` with metadata (set time, scope).
Redaction is enforced by tests, not convention.

### Reporting a security issue

This project is experimental and not yet distributed. Do not file public
security issues. Until a private disclosure channel is set up, contact the
maintainers directly.

## `.git` safety

`.git` internals are never synced as ordinary files. Blind sync of
`.git/index`, lockfiles, packfiles, and refs can corrupt repositories. The
MVP uses Git-aware metadata instead. `fs2 doctor` warns if a user overrides
this to normal sync.
