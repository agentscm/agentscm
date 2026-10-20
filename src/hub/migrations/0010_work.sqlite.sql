CREATE TABLE hub_work_items (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id INTEGER,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    lead_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    lead_agent TEXT NOT NULL,
    lead_session TEXT NOT NULL,
    generation INTEGER NOT NULL,
    source_provider TEXT,
    source_host TEXT,
    source_repository TEXT,
    source_issue TEXT,
    request_key TEXT,
    title TEXT NOT NULL,
    state TEXT NOT NULL,
    scope TEXT NOT NULL,
    branch TEXT,
    worktree TEXT,
    next_action TEXT,
    current_checkpoint TEXT,
    snapshot TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    created_seq INTEGER,
    updated_seq INTEGER,
    CHECK ((request_key IS NOT NULL) <> (source_issue IS NOT NULL))
);

CREATE UNIQUE INDEX hub_work_native_key ON hub_work_items
    (provider, host, repository, ifnull(repository_id, -1), owner, request_key)
    WHERE request_key IS NOT NULL;
CREATE UNIQUE INDEX hub_work_forge_key ON hub_work_items
    (provider, host, repository, ifnull(repository_id, -1),
     source_provider, source_host, source_repository, source_issue)
    WHERE source_issue IS NOT NULL;
CREATE INDEX hub_work_scope ON hub_work_items
    (provider, host, repository, repository_id, id);

CREATE TABLE hub_work_participants (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    owner TEXT NOT NULL REFERENCES hub_users (id),
    agent TEXT NOT NULL,
    session TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (work, token, session)
);

CREATE TABLE hub_work_relations (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    related TEXT NOT NULL REFERENCES hub_work_items (id),
    kind TEXT NOT NULL,
    PRIMARY KEY (work, related)
);

CREATE TABLE hub_work_versions (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    event_seq INTEGER NOT NULL REFERENCES hub_events (seq),
    state TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    PRIMARY KEY (work, event_seq)
);
CREATE INDEX hub_work_versions_event ON hub_work_versions (event_seq, work);

CREATE TABLE hub_work_checkpoint_ops (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    operation_id TEXT NOT NULL,
    request TEXT NOT NULL,
    event_seq INTEGER NOT NULL REFERENCES hub_events (seq),
    PRIMARY KEY (work, operation_id)
);

CREATE TABLE hub_work_acks (
    owner TEXT NOT NULL REFERENCES hub_users (id),
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id INTEGER,
    cursor INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX hub_work_ack_scope ON hub_work_acks
    (owner, provider, host, repository, ifnull(repository_id, -1));

CREATE TABLE hub_work_retention (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id INTEGER,
    event_floor_seq INTEGER NOT NULL,
    snapshot_floor_seq INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX hub_work_retention_scope ON hub_work_retention
    (provider, host, repository, ifnull(repository_id, -1));

CREATE TABLE hub_work_usage (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id INTEGER,
    charged_bytes INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX hub_work_usage_scope ON hub_work_usage
    (provider, host, repository, ifnull(repository_id, -1));

-- Work history has an explicit, repository-scoped compaction floor. Other hub
-- events remain append-only.
DROP TRIGGER hub_events_no_delete;
CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
WHEN OLD.kind NOT IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;

CREATE TABLE hub_work_deliveries (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id INTEGER,
    from_seq INTEGER NOT NULL,
    through_seq INTEGER NOT NULL,
    page_after INTEGER NOT NULL,
    page_work_after TEXT,
    page_work_done INTEGER NOT NULL,
    next_after INTEGER NOT NULL,
    next_work_after TEXT,
    next_work_done INTEGER NOT NULL,
    page TEXT NOT NULL,
    complete INTEGER NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX hub_work_delivery_scope ON hub_work_deliveries
    (owner, provider, host, repository, repository_id, created_at);
