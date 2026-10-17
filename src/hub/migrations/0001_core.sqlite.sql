CREATE TABLE hub_users (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    forge_user_id INTEGER NOT NULL,
    login TEXT NOT NULL,
    disabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE (provider, host, forge_user_id)
);

CREATE TABLE hub_agent_tokens (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    name TEXT NOT NULL,
    repository TEXT NOT NULL,
    digest TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE INDEX hub_agent_tokens_owner ON hub_agent_tokens (owner);

CREATE TABLE hub_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    data TEXT NOT NULL
);

CREATE TRIGGER hub_events_no_update BEFORE UPDATE ON hub_events
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;

CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;

-- INSERT OR REPLACE deletes without firing the delete trigger.
CREATE TRIGGER hub_events_no_replace BEFORE INSERT ON hub_events
WHEN EXISTS (SELECT 1 FROM hub_events WHERE seq = NEW.seq OR id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;
