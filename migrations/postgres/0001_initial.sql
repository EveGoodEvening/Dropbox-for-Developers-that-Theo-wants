CREATE TABLE users (
  id UUID PRIMARY KEY,
  email TEXT UNIQUE NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE devices (
  id UUID PRIMARY KEY,
  user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  public_key BYTEA NOT NULL,
  platform JSONB NOT NULL,
  revoked_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE workspaces (
  id UUID PRIMARY KEY,
  user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  root_node_id UUID NOT NULL,
  current_cursor BIGINT NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(user_id, name)
);

CREATE TABLE nodes (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  parent_id UUID NULL REFERENCES nodes(id),
  name TEXT NOT NULL,
  normalized_name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('directory', 'file', 'symlink')),
  current_revision_id UUID NULL,
  deleted_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX nodes_live_root_name_idx
ON nodes(workspace_id, normalized_name)
WHERE deleted_at IS NULL AND parent_id IS NULL;

CREATE UNIQUE INDEX nodes_live_name_idx
ON nodes(workspace_id, parent_id, normalized_name)
WHERE deleted_at IS NULL AND parent_id IS NOT NULL;

CREATE INDEX nodes_workspace_parent_idx
ON nodes(workspace_id, parent_id)
WHERE deleted_at IS NULL;

CREATE INDEX nodes_workspace_path_scan_idx
ON nodes(workspace_id, normalized_name)
WHERE deleted_at IS NULL;

CREATE TABLE node_revisions (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  node_id UUID NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  device_id UUID NOT NULL REFERENCES devices(id),
  base_revision_id UUID NULL REFERENCES node_revisions(id),
  kind TEXT NOT NULL CHECK (kind IN ('directory', 'file', 'symlink')),
  blob_id TEXT NULL,
  chunk_ids JSONB NULL,
  symlink_target TEXT NULL,
  size BIGINT NOT NULL DEFAULT 0 CHECK (size >= 0),
  content_hash TEXT NULL,
  encryption_header TEXT NULL,
  posix_mode INTEGER NOT NULL DEFAULT 420,
  mtime TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE nodes
ADD CONSTRAINT nodes_current_revision_fk
FOREIGN KEY (current_revision_id) REFERENCES node_revisions(id);

CREATE INDEX node_revisions_node_created_idx
ON node_revisions(node_id, created_at DESC);

CREATE INDEX node_revisions_workspace_node_idx
ON node_revisions(workspace_id, node_id);

CREATE TABLE operations (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  device_id UUID NOT NULL REFERENCES devices(id),
  cursor BIGINT NOT NULL CHECK (cursor > 0),
  kind TEXT NOT NULL,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(workspace_id, cursor),
  UNIQUE(workspace_id, id)
);

CREATE INDEX operations_workspace_cursor_idx
ON operations(workspace_id, cursor);

CREATE TABLE blobs (
  id TEXT PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  size BIGINT NOT NULL CHECK (size >= 0),
  encryption_header TEXT NULL,
  object_key TEXT NOT NULL,
  uploaded_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX blobs_workspace_idx
ON blobs(workspace_id);

CREATE TABLE env_vars (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  project_path TEXT NULL,
  environment TEXT NOT NULL,
  name TEXT NOT NULL,
  scope JSONB NOT NULL,
  secret_kind TEXT NOT NULL,
  encrypted_value TEXT NOT NULL,
  metadata JSONB NOT NULL,
  deleted_at TIMESTAMPTZ,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT env_vars_scope_shape CHECK (
    jsonb_typeof(scope) = 'object'
    AND COALESCE(
      (scope->>'type' = 'workspace' AND project_path IS NULL)
      OR (scope->>'type' = 'project' AND project_path IS NOT NULL AND scope->>'project_path' = project_path)
      OR (
        scope->>'type' = 'machine'
        AND project_path IS NULL
        AND NULLIF(scope->>'device_id', '') IS NOT NULL
      )
      OR (
        scope->>'type' = 'project_machine'
        AND project_path IS NOT NULL
        AND scope->>'project_path' = project_path
        AND NULLIF(scope->>'device_id', '') IS NOT NULL
      ),
      false
    )
  )
);

CREATE UNIQUE INDEX env_vars_live_workspace_name_idx
ON env_vars(workspace_id, environment, name)
WHERE deleted_at IS NULL AND scope->>'type' = 'workspace';

CREATE UNIQUE INDEX env_vars_live_project_name_idx
ON env_vars(workspace_id, project_path, environment, name)
WHERE deleted_at IS NULL AND scope->>'type' = 'project';

CREATE UNIQUE INDEX env_vars_live_machine_name_idx
ON env_vars(workspace_id, environment, name, (scope->>'device_id'))
WHERE deleted_at IS NULL AND scope->>'type' = 'machine';

CREATE UNIQUE INDEX env_vars_live_project_machine_name_idx
ON env_vars(workspace_id, project_path, environment, name, (scope->>'device_id'))
WHERE deleted_at IS NULL AND scope->>'type' = 'project_machine';

CREATE INDEX env_vars_workspace_idx
ON env_vars(workspace_id)
WHERE deleted_at IS NULL;

CREATE TABLE key_envelopes (
  id UUID PRIMARY KEY,
  workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  device_id UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  encrypted_workspace_key TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE(workspace_id, device_id)
);

CREATE INDEX key_envelopes_device_idx
ON key_envelopes(device_id);
