CREATE TABLE hub_connections (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    kind TEXT NOT NULL,
    app_id INTEGER,
    app_slug TEXT,
    acts_as TEXT,
    outbound INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    status_detail TEXT,
    next_poll_at INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE hub_repositories (
    id TEXT PRIMARY KEY,
    connection TEXT NOT NULL REFERENCES hub_connections (id),
    host TEXT NOT NULL,
    repository_id INTEGER NOT NULL,
    full_name TEXT NOT NULL,
    node_id TEXT,
    installation_id INTEGER,
    pulls_etag TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (host, repository_id)
);

CREATE INDEX hub_repositories_name ON hub_repositories (host, full_name);
CREATE INDEX hub_repositories_connection ON hub_repositories (connection);

-- Current state of what the forge showed; history is in hub_events. A closed
-- pull request or deleted branch stays as a tombstone (deleted = 1) so an older
-- read that finishes late cannot bring it back.
CREATE TABLE hub_observations (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    data TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repository, kind, key)
);

CREATE TABLE hub_webhook_deliveries (
    connection TEXT NOT NULL REFERENCES hub_connections (id),
    delivery TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (connection, delivery)
);

CREATE INDEX hub_webhook_deliveries_received ON hub_webhook_deliveries (received_at);

CREATE TABLE hub_outbound (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    number INTEGER NOT NULL,
    comment_id INTEGER,
    body_sha256 TEXT,
    status_sha TEXT,
    status_description TEXT,
    PRIMARY KEY (repository, number)
);
