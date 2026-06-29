# Dropbox for Developers that Theo wants

> **WARNING:** This project is experimental and not yet distributed. Do not use
> it for production data. See `docs/mvp.md` for scope and `SECURITY.md` for the
> security model.

Idea from Theo (https://x.com/theo/status/2069621429189161350 / https://www.youtube.com/watch?v=wEAb0x3wTRc). Note that Theo has no endorsement on this project (yet).

## Rationale - **Dropbox for Developers (Cross-Machine Code Sync)**

### Pain Point
Theo develops on multiple machines (Mac Mini × 2, GMK Tech Box Linux), and managing code synchronization is a nightmare:

-  Forget to run `git pull` on one machine, and the worktree gets stale
-  Environment variables are set on one machine but not on another
-  Project directory structures are inconsistent across machines
-  Git submodule hell — nobody wants to deal with it

**Dropbox doesn’t have these problems** — the structure is exactly the same on every machine, and everything syncs automatically.

### Theo wants:

-  Code folders that sync automatically, like Dropbox
-  Environment variable synchronization
-  On-demand downloading: sync the structure first, and only fetch file contents when a specific file is accessed
-  `node_modules` and other platform-specific things need special handling
-  Something like Google Drive/Dropbox’s own equivalent of `.gitignore`
-  Theo has started a project called **FS2** (File System 2), but it’s nowhere near enough

> “Building something like this doesn’t require your ability or knowledge. It requires your **token budget and patience**.”

