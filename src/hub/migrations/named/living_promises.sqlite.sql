CREATE TABLE hub_promises (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL CHECK (length(workspace_id) BETWEEN 1 AND 128),
    steward_user TEXT NOT NULL REFERENCES hub_users (id),
    current_version INTEGER NOT NULL CHECK (current_version > 0),
    current_digest TEXT NOT NULL CHECK (length(current_digest) = 64),
    lifetime_scope TEXT NOT NULL CHECK (json_valid(lifetime_scope) AND json_type(lifetime_scope) = 'array' AND length(lifetime_scope) <= 524288),
    state TEXT NOT NULL CHECK (state IN ('active', 'retired')),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    UNIQUE (workspace_id, id)
) STRICT;
CREATE INDEX hub_promises_page ON hub_promises (workspace_id, id);

CREATE TABLE hub_promise_versions (
    promise_id TEXT NOT NULL REFERENCES hub_promises (id),
    version INTEGER NOT NULL CHECK (version > 0),
    digest TEXT NOT NULL CHECK (length(digest) = 64),
    prior_digest TEXT CHECK (prior_digest IS NULL OR length(prior_digest) = 64),
    kind TEXT NOT NULL CHECK (kind IN ('create','adopt','amend','correct','retire','transfer','recovery_transfer')),
    actor_user TEXT NOT NULL REFERENCES hub_users (id),
    payload TEXT NOT NULL CHECK (length(payload) <= 131072),
    current_scope TEXT NOT NULL CHECK (json_valid(current_scope) AND json_type(current_scope) = 'array' AND length(current_scope) <= 65536),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (promise_id, version),
    UNIQUE (promise_id, digest)
) STRICT;

CREATE TABLE hub_promise_version_repositories (
    workspace_id TEXT NOT NULL CHECK (length(workspace_id) BETWEEN 1 AND 128),
    promise_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    repository_id TEXT NOT NULL CHECK (length(repository_id) BETWEEN 1 AND 128),
    admission TEXT NOT NULL CHECK (length(admission) <= 8192),
    PRIMARY KEY (promise_id, version, repository_id),
    FOREIGN KEY (workspace_id, promise_id) REFERENCES hub_promises (workspace_id, id),
    FOREIGN KEY (promise_id, version) REFERENCES hub_promise_versions (promise_id, version)
) STRICT;
CREATE INDEX hub_promise_repository_scope
    ON hub_promise_version_repositories (workspace_id, repository_id, promise_id, version);

CREATE TABLE hub_promise_inputs (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL CHECK (length(workspace_id) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (kind IN ('proposal', 'report')),
    repository_id TEXT NOT NULL CHECK (length(repository_id) BETWEEN 1 AND 128),
    admission TEXT NOT NULL CHECK (length(admission) <= 8192),
    actor_user TEXT NOT NULL REFERENCES hub_users (id),
    actor_token TEXT REFERENCES hub_agent_tokens (id),
    promise_id TEXT REFERENCES hub_promises (id),
    promise_version INTEGER CHECK (promise_version IS NULL OR promise_version > 0),
    promise_digest TEXT CHECK (promise_digest IS NULL OR length(promise_digest) = 64),
    payload TEXT NOT NULL CHECK (length(payload) <= 65536),
    adopted_promise TEXT REFERENCES hub_promises (id),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    CHECK ((kind = 'proposal' AND promise_id IS NULL AND promise_version IS NULL AND promise_digest IS NULL)
        OR (kind = 'report' AND promise_id IS NOT NULL AND promise_version IS NOT NULL AND promise_digest IS NOT NULL))
) STRICT;
CREATE INDEX hub_promise_inputs_scope
    ON hub_promise_inputs (workspace_id, repository_id, created_at, id);
CREATE INDEX hub_promise_inputs_report_scope
    ON hub_promise_inputs (workspace_id, promise_id, created_at, id);

CREATE TABLE hub_promise_operations (
    workspace_id TEXT NOT NULL CHECK (length(workspace_id) BETWEEN 1 AND 128),
    operation_id TEXT NOT NULL CHECK (length(operation_id) BETWEEN 1 AND 128),
    actor_user TEXT NOT NULL REFERENCES hub_users (id),
    actor_token TEXT REFERENCES hub_agent_tokens (id),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64),
    response TEXT NOT NULL CHECK (length(response) <= 16384),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (workspace_id, operation_id)
) STRICT;

CREATE TRIGGER hub_promise_versions_no_update
BEFORE UPDATE ON hub_promise_versions BEGIN
    SELECT RAISE(ABORT, 'promise versions are immutable');
END;
CREATE TRIGGER hub_promise_versions_no_delete
BEFORE DELETE ON hub_promise_versions BEGIN
    SELECT RAISE(ABORT, 'promise versions are immutable');
END;
CREATE TRIGGER hub_promise_repositories_no_update
BEFORE UPDATE ON hub_promise_version_repositories BEGIN
    SELECT RAISE(ABORT, 'promise version repositories are immutable');
END;
CREATE TRIGGER hub_promise_repositories_no_delete
BEFORE DELETE ON hub_promise_version_repositories BEGIN
    SELECT RAISE(ABORT, 'promise version repositories are immutable');
END;
