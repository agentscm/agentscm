-- A refused agent's request that the holder of a file hand it over; the holder answers once.
-- A pending request older than the hold cap counts as lapsed.
CREATE TABLE hub_handoffs (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    -- An exact file key.
    path TEXT NOT NULL,
    requester_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    requester_session TEXT NOT NULL,
    holder_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    holder_session TEXT NOT NULL,
    holder_intent TEXT NOT NULL REFERENCES hub_intents (id),
    -- Already filtered, at most 200 chars.
    note TEXT,
    -- The holder's decline reason, filtered, at most 200 chars.
    reply TEXT,
    -- pending | granted | declined | lapsed
    state TEXT NOT NULL,
    holder_told INTEGER NOT NULL DEFAULT 0,
    requester_told INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    decided_at INTEGER
);

CREATE INDEX hub_handoffs_holder ON hub_handoffs (holder_token, holder_session, state);
CREATE INDEX hub_handoffs_requester ON hub_handoffs (requester_token, requester_session, state);
