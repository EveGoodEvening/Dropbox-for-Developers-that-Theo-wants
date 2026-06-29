# Security Policy

FS2 is experimental and not ready for production secrets or irreplaceable private code.

## MVP security model

- File blobs should be encrypted on the client before upload.
- Environment variable values must be encrypted before they leave the client.
- The backend must store encrypted secret envelopes only and must not log plaintext secrets, tokens, decrypted env values, or private keys.
- Auth tokens, refresh tokens, private keys, and workspace keys belong in the OS keychain or an encrypted development fallback that warns clearly.
- `.git` internals are not synced as ordinary files by default.

## Metadata visibility

MVP metadata is not end-to-end encrypted. Filenames, paths, sizes, mtimes, directory structure, device records, operation cursors, and conflict metadata may be visible to the service operator.

## Local data safety

Sync code must preserve dirty local bytes until the backend acknowledges the relevant operation. Conflict handling must preserve both local and remote versions. Cache pruning must never delete dirty, uploading, conflict, or pinned data.

## Reporting

Do not report real secrets in issues. Use redacted logs and generated test credentials only.
