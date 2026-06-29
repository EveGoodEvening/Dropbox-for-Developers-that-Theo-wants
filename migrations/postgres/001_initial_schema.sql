-- fs2-devsync schema migration 001: initial schema
-- Creates users, devices, workspaces, nodes, revisions, operations, and env tables.

-- Users table: one row per user account.
CREATE TABLE IF NOT EXISTS users (
    user_id      UUID PRIMARY KEY,
    email        TEXT NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Devices table: one row per enrolled device.
CREATE TABLE IF NOT EXISTS devices (
    device_id    UUID PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    public_key   BYTEA NOT NULL,
    platform     JSONB NOT NULL DEFAULT '{}'::jsonb,
    revoked_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_devices_user_id ON devices(user_id);

-- Workspaces table: one row per workspace.
CREATE TABLE IF NOT EXISTS workspaces (
    workspace_id   UUID PRIMARY KEY,
    user_id        UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    root_node_id   UUID NOT NULL,
    current_cursor BIGINT NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_workspaces_user_id ON workspaces(user_id);

-- Nodes table: one row per node (file/directory/symlink) in a workspace.
CREATE TABLE IF NOT EXISTS nodes (
    node_id            UUID PRIMARY KEY,
    workspace_id       UUID NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    parent_id          UUID REFERENCES nodes(node_id) ON DELETE CASCADE,
    name               TEXT NOT NULL,
    kind               TEXT NOT NULL CHECK (kind IN ('file', 'directory', 'symlink')),
    current_rev        UUID,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at         TIMESTAMPTZ,
    tombstone_version  BIGINT
);

CREATE INDEX IF NOT EXISTS idx_nodes_workspace_id ON nodes(workspace_id);
CREATE INDEX IF NOT EXISTS idx_nodes_parent_id ON nodes(parent_id);
CREATE INDEX IF NOT EXISTS idx_nodes_workspace_parent_name ON nodes(workspace_id, parent_id, name)
    WHERE deleted_at IS NULL;

-- Revisions table: immutable revisions of node content.
CREATE TABLE IF NOT EXISTS revisions (
    revision_id        UUID PRIMARY KEY,
    node_id            UUID NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
    workspace_id       UUID NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    device_id          UUID NOT NULL REFERENCES devices(device_id),
    base_revision_id   UUID REFERENCES revisions(revision_id),
    content            JSONB NOT NULL,
    posix_mode         INTEGER NOT NULL DEFAULT 644,
    mtime              TIMESTAMPTZ NOT NULL,
    size               BIGINT NOT NULL DEFAULT 0,
    executable         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_revisions_node_id ON revisions(node_id);

-- Operations table: append-only operation log per workspace.
CREATE TABLE IF NOT EXISTS operations (
    workspace_id   UUID NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    cursor         BIGINT NOT NULL,
    op_id          UUID NOT NULL,
    device_id      UUID NOT NULL REFERENCES devices(device_id),
    base_cursor    BIGINT NOT NULL DEFAULT 0,
    kind           JSONB NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (workspace_id, cursor)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_operations_dedup ON operations(workspace_id, op_id);
CREATE INDEX IF NOT EXISTS idx_operations_workspace_cursor ON operations(workspace_id, cursor);

-- Env vars table: encrypted environment variables per workspace.
CREATE TABLE IF NOT EXISTS env_vars (
    workspace_id   UUID NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    key            TEXT NOT NULL,
    encrypted_value BYTEA NOT NULL,
    nonce          BYTEA NOT NULL,
    secret_kind    TEXT NOT NULL DEFAULT 'plain_config' CHECK (secret_kind IN ('secret', 'plain_config')),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (workspace_id, key)
);

-- Rules table: structured rules per workspace (from .fs2/config.toml).
CREATE TABLE IF NOT EXISTS workspace_rules (
    workspace_id   UUID NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    pattern        TEXT NOT NULL,
    action         TEXT NOT NULL,
    manager        TEXT,
    scope          TEXT,
    PRIMARY KEY (workspace_id, pattern)
);
