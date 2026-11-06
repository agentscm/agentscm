-- Hand-off pickups: a person hands a task or a Work item to their own agent with one line, and
-- the Hub marks the record picked up when that person's agent starts with its key.
CREATE TABLE hub_work_pickups (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    repository TEXT NOT NULL,
    work_id TEXT,
    title TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    abandoned_at INTEGER,
    picked_at INTEGER,
    picked_agent TEXT,
    picked_session TEXT,
    picked_work TEXT
);
CREATE INDEX hub_work_pickups_owner ON hub_work_pickups (owner, repository, created_at);
