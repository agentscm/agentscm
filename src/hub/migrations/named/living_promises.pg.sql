CREATE TABLE hub_promises (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL CHECK (octet_length(workspace_id) BETWEEN 1 AND 128),
    steward_user TEXT NOT NULL REFERENCES hub_users (id),
    current_version BIGINT NOT NULL CHECK (current_version > 0),
    current_digest TEXT NOT NULL CHECK (octet_length(current_digest) = 64),
    lifetime_scope JSONB NOT NULL CHECK (jsonb_typeof(lifetime_scope) = 'array' AND octet_length(lifetime_scope::text) <= 524288),
    state TEXT NOT NULL CHECK (state IN ('active', 'retired')),
    created_at BIGINT NOT NULL CHECK (created_at >= 0),
    UNIQUE (workspace_id, id)
);
CREATE INDEX hub_promises_page ON hub_promises (workspace_id, id);

CREATE TABLE hub_promise_versions (
    promise_id TEXT NOT NULL REFERENCES hub_promises (id),
    version BIGINT NOT NULL CHECK (version > 0),
    digest TEXT NOT NULL CHECK (octet_length(digest) = 64),
    prior_digest TEXT CHECK (prior_digest IS NULL OR octet_length(prior_digest) = 64),
    kind TEXT NOT NULL CHECK (kind IN ('create','adopt','amend','correct','retire','transfer','recovery_transfer')),
    actor_user TEXT NOT NULL REFERENCES hub_users (id),
    payload TEXT NOT NULL CHECK (octet_length(payload) <= 131072),
    current_scope JSONB NOT NULL CHECK (jsonb_typeof(current_scope) = 'array' AND octet_length(current_scope::text) <= 65536),
    created_at BIGINT NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (promise_id, version),
    UNIQUE (promise_id, digest)
);

CREATE TABLE hub_promise_version_repositories (
    workspace_id TEXT NOT NULL CHECK (octet_length(workspace_id) BETWEEN 1 AND 128),
    promise_id TEXT NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0),
    repository_id TEXT NOT NULL CHECK (octet_length(repository_id) BETWEEN 1 AND 128),
    admission TEXT NOT NULL CHECK (octet_length(admission) <= 8192),
    PRIMARY KEY (promise_id, version, repository_id),
    FOREIGN KEY (workspace_id, promise_id) REFERENCES hub_promises (workspace_id, id),
    FOREIGN KEY (promise_id, version) REFERENCES hub_promise_versions (promise_id, version)
);
CREATE INDEX hub_promise_repository_scope
    ON hub_promise_version_repositories (workspace_id, repository_id, promise_id, version);

CREATE TABLE hub_promise_inputs (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL CHECK (octet_length(workspace_id) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (kind IN ('proposal', 'report')),
    repository_id TEXT NOT NULL CHECK (octet_length(repository_id) BETWEEN 1 AND 128),
    admission TEXT NOT NULL CHECK (octet_length(admission) <= 8192),
    actor_user TEXT NOT NULL REFERENCES hub_users (id),
    actor_token TEXT REFERENCES hub_agent_tokens (id),
    promise_id TEXT REFERENCES hub_promises (id),
    promise_version BIGINT CHECK (promise_version IS NULL OR promise_version > 0),
    promise_digest TEXT CHECK (promise_digest IS NULL OR octet_length(promise_digest) = 64),
    payload TEXT NOT NULL CHECK (octet_length(payload) <= 65536),
    adopted_promise TEXT REFERENCES hub_promises (id),
    created_at BIGINT NOT NULL CHECK (created_at >= 0),
    CHECK ((kind = 'proposal' AND promise_id IS NULL AND promise_version IS NULL AND promise_digest IS NULL)
        OR (kind = 'report' AND promise_id IS NOT NULL AND promise_version IS NOT NULL AND promise_digest IS NOT NULL))
);
CREATE INDEX hub_promise_inputs_scope
    ON hub_promise_inputs (workspace_id, repository_id, created_at, id);
CREATE INDEX hub_promise_inputs_report_scope
    ON hub_promise_inputs (workspace_id, promise_id, created_at, id);

CREATE TABLE hub_promise_operations (
    workspace_id TEXT NOT NULL CHECK (octet_length(workspace_id) BETWEEN 1 AND 128),
    operation_id TEXT NOT NULL CHECK (octet_length(operation_id) BETWEEN 1 AND 128),
    actor_user TEXT NOT NULL REFERENCES hub_users (id),
    actor_token TEXT REFERENCES hub_agent_tokens (id),
    fingerprint TEXT NOT NULL CHECK (octet_length(fingerprint) = 64),
    response TEXT NOT NULL CHECK (octet_length(response) <= 16384),
    created_at BIGINT NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (workspace_id, operation_id)
);

CREATE FUNCTION hub_promise_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'promise versions are immutable';
END;
$$;
CREATE TRIGGER hub_promise_versions_no_change
BEFORE UPDATE OR DELETE ON hub_promise_versions
FOR EACH ROW EXECUTE FUNCTION hub_promise_immutable();
CREATE TRIGGER hub_promise_repositories_no_change
BEFORE UPDATE OR DELETE ON hub_promise_version_repositories
FOR EACH ROW EXECUTE FUNCTION hub_promise_immutable();
