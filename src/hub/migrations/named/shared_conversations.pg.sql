-- requires: change_records
CREATE TABLE hub_change_shared_bodies (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    digest TEXT NOT NULL CHECK (octet_length(digest) = 64 AND digest ~ '^[0-9a-f]+$'),
    size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0 AND size_bytes <= 1048576),
    media TEXT NOT NULL CHECK (media = 'json'),
    session_id TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    event_id TEXT NOT NULL,
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    author TEXT NOT NULL REFERENCES hub_users (id),
    declared_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    declared_at BIGINT NOT NULL CHECK (declared_at >= 0),
    state TEXT NOT NULL CHECK (state IN ('declared', 'available', 'expired', 'deleted')),
    uploaded_at BIGINT CHECK (uploaded_at IS NULL OR uploaded_at >= 0),
    expires_at BIGINT CHECK (expires_at IS NULL OR expires_at >= 0),
    deleted_at BIGINT CHECK (deleted_at IS NULL OR deleted_at >= 0),
    deleted_by TEXT,
    physical_deleted_at BIGINT CHECK (physical_deleted_at IS NULL OR physical_deleted_at >= 0),
    delete_attempts BIGINT NOT NULL DEFAULT 0 CHECK (delete_attempts >= 0),
    next_delete_at BIGINT CHECK (next_delete_at IS NULL OR next_delete_at >= 0),
    CHECK (state = 'declared' OR state = 'deleted' OR (uploaded_at IS NOT NULL AND expires_at IS NOT NULL)),
    CHECK ((state = 'deleted') = (deleted_at IS NOT NULL AND deleted_by IS NOT NULL)),
    PRIMARY KEY (work, digest, author)
);
CREATE INDEX hub_change_shared_bodies_expiry ON hub_change_shared_bodies (expires_at) WHERE state = 'available';
CREATE INDEX hub_change_shared_bodies_sweep ON hub_change_shared_bodies (COALESCE(deleted_at, expires_at)) WHERE physical_deleted_at IS NULL AND state IN ('expired', 'deleted');
