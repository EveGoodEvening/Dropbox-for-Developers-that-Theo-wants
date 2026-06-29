-- fs2-devsync Postgres schema migration
-- Creates all tables from design.md section 15.1

CREATE TABLE IF NOT EXISTS users (
  id UUID PRIMARY KEY,
  email TEXT UNIQUE NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS devices (
  id UUID PRIMARY KEY,
  user_id UUID NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  public_key BYTEA NOT NULL,
  platform JSONB NOT NULL,
  revoked_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS workspaces (
  id UUID PRIMARY KEY,
  user_id UUID NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  root_node_id UUID NOT NULL,
  current_cursor BIGINT NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS nodes (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  parent_id UUID NULL,
  name TEXT NOT NULL,
  normalized_name TEXT NOT NULL,
  kind TEXT NOT NULL,
  current_revision_id UUID NULL,
  deleted_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS nodes_live_name_idx
ON nodes(workspace_id, parent_id, normalized_name)
WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS nodes_workspace_idx ON nodes(workspace_id);
CREATE INDEX IF NOT EXISTS nodes_parent_idx ON nodes(workspace_id, parent_id);

CREATE TABLE IF NOT EXISTS node_revisions (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  node_id UUID NOT NULL REFERENCES nodes(id),
  device_id UUID NOT NULL REFERENCES devices(id),
  base_revision_id UUID NULL,
  kind TEXT NOT NULL,
  blob_id TEXT NULL,
  chunk_ids JSONB NULL,
  symlink_target TEXT NULL,
  size BIGINT NOT NULL DEFAULT 0,
  content_hash TEXT NULL,
  encryption_header TEXT NULL,
  posix_mode INTEGER NOT NULL DEFAULT 420,
  mtime TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS node_revisions_node_idx ON node_revisions(node_id);

CREATE TABLE IF NOT EXISTS operations (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  device_id UUID NOT NULL REFERENCES devices(id),
  cursor BIGINT NOT NULL,
  kind TEXT NOT NULL,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(workspace_id, cursor),
  UNIQUE(workspace_id, id)
);

CREATE INDEX IF NOT EXISTS operations_workspace_cursor_idx
ON operations(workspace_id, cursor);

CREATE TABLE IF NOT EXISTS blobs (
  id TEXT PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  size BIGINT NOT NULL,
  encryption_header TEXT NULL,
  object_key TEXT NOT NULL,
  uploaded_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS env_vars (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  project_path TEXT NULL,
  environment TEXT NOT NULL,
  name TEXT NOT NULL,
  scope JSONB NOT NULL,
  secret_kind TEXT NOT NULL,
  encrypted_value TEXT NOT NULL,
  metadata JSONB NOT NULL,
  deleted_at TIMESTAMPTZ,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS env_vars_workspace_idx
ON env_vars(workspace_id, project_path, environment)
WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS key_envelopes (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id),
  device_id UUID NOT NULL REFERENCES devices(id),
  encrypted_workspace_key TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(workspace_id, device_id)
);
