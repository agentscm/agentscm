-- A session is one agent conversation of a token; it owns at most one intent.
CREATE TABLE hub_sessions (
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    session TEXT NOT NULL,
    intent TEXT REFERENCES hub_intents (id),
    task TEXT,
    -- Canonical JSON array of connection modes, sorted and unique.
    via TEXT NOT NULL,
    notice_cursor BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (token, session)
);

CREATE INDEX hub_sessions_intent ON hub_sessions (intent);
CREATE INDEX hub_sessions_recent ON hub_sessions (token, updated_at);
