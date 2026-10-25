CREATE TABLE hub_catchup_repositories (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    next_merge_seq INTEGER NOT NULL DEFAULT 1,
    merge_floor_seq INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key),
    CHECK (repository_key = ifnull(repository_id, -1))
);

CREATE TABLE hub_catchup_merges (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    seq INTEGER NOT NULL,
    id TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    occurred_at INTEGER,
    observed_at INTEGER NOT NULL,
    title TEXT,
    branch TEXT NOT NULL,
    base TEXT NOT NULL,
    head_commit TEXT NOT NULL,
    merge_commit TEXT NOT NULL,
    pull_number INTEGER NOT NULL,
    paths TEXT NOT NULL,
    paths_complete INTEGER NOT NULL,
    payload_bytes INTEGER NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, seq),
    UNIQUE (provider, host, repository, repository_key, pull_number, merge_commit),
    FOREIGN KEY (provider, host, repository, repository_key)
        REFERENCES hub_catchup_repositories (provider, host, repository, repository_key),
    CHECK (repository_key = ifnull(repository_id, -1))
);
CREATE INDEX hub_catchup_merge_head_scope
    ON hub_catchup_merges(provider, host, repository, repository_key, head_commit, seq);

CREATE TRIGGER hub_catchup_merges_no_update BEFORE UPDATE ON hub_catchup_merges
BEGIN SELECT RAISE(ABORT, 'catch-up merge observations are immutable'); END;
CREATE TRIGGER hub_catchup_merges_no_delete BEFORE DELETE ON hub_catchup_merges
WHEN OLD.seq > COALESCE((SELECT merge_floor_seq FROM hub_catchup_repositories
    WHERE provider=OLD.provider AND host=OLD.host AND repository=OLD.repository
      AND repository_key=OLD.repository_key), 0)
BEGIN SELECT RAISE(ABORT, 'catch-up merge observations are retained'); END;

CREATE TABLE hub_catchup_sources (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    source TEXT NOT NULL,
    state TEXT NOT NULL,
    checked_at INTEGER NOT NULL,
    unread_pages INTEGER NOT NULL,
    deferred INTEGER NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, source),
    CHECK (repository_key = ifnull(repository_id, -1))
);

CREATE TABLE hub_catchup_acks (
    owner TEXT NOT NULL REFERENCES hub_users (id),
    subscriber_token TEXT REFERENCES hub_agent_tokens (id),
    subscriber TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    event_cursor INTEGER NOT NULL,
    merge_cursor INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (owner, subscriber_token, subscriber, provider, host, repository, repository_key),
    CHECK (repository_key = ifnull(repository_id, -1))
);

CREATE UNIQUE INDEX hub_catchup_human_ack_scope ON hub_catchup_acks
    (owner, subscriber, provider, host, repository, repository_key)
    WHERE subscriber_token IS NULL;

CREATE TABLE hub_catchup_deliveries (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    subscriber_token TEXT REFERENCES hub_agent_tokens (id),
    subscriber TEXT NOT NULL,
    delivery_client TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    event_from INTEGER NOT NULL,
    event_through INTEGER NOT NULL,
    merge_from INTEGER NOT NULL,
    merge_through INTEGER NOT NULL,
    page_event_after INTEGER NOT NULL,
    page_merge_after INTEGER NOT NULL,
    next_event_after INTEGER NOT NULL,
    next_merge_after INTEGER NOT NULL,
    page TEXT NOT NULL,
    complete INTEGER NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0,
    ack_actor TEXT REFERENCES hub_agent_tokens (id),
    superseded INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (repository_key = ifnull(repository_id, -1))
);
CREATE INDEX hub_catchup_delivery_scope ON hub_catchup_deliveries
    (owner, subscriber_token, subscriber, delivery_client, provider, host, repository, repository_key, created_at);

CREATE TABLE hub_catchup_backfills (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    after_publication TEXT NOT NULL,
    complete INTEGER NOT NULL,
    deferred INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key),
    CHECK (repository_key = ifnull(repository_id, -1))
);

CREATE TABLE hub_catchup_publication_failures (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    repository_id INTEGER,
    publication TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, publication),
    CHECK (repository_key = ifnull(repository_id, -1)),
    CHECK (attempts > 0 AND attempts <= 1000000)
);
