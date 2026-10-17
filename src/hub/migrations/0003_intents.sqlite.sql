CREATE TABLE hub_intents (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    task TEXT NOT NULL,
    branch TEXT,
    base TEXT,
    machine TEXT,
    paths TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    released_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX hub_intents_token ON hub_intents (token);
CREATE INDEX hub_intents_live ON hub_intents (repository, expires_at);

-- One row per path spec of a live intent; key is the overlap index key.
CREATE TABLE hub_intent_paths (
    intent TEXT NOT NULL REFERENCES hub_intents (id),
    repository TEXT NOT NULL,
    key TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (intent, key)
);

CREATE INDEX hub_intent_paths_key ON hub_intent_paths (repository, key);
CREATE INDEX hub_intent_paths_expiry ON hub_intent_paths (expires_at);

-- Notices are found by recipient subject.
CREATE INDEX hub_events_subject ON hub_events (subject, seq);
