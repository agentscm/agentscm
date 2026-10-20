ALTER TABLE hub_work_participants ADD COLUMN generation BIGINT NOT NULL DEFAULT 1;

-- Preserve only the current lead's authority. Historical leads and helpers
-- cannot be assigned a generation reliably and must explicitly rejoin.
UPDATE hub_work_participants p
SET generation = w.generation
FROM hub_work_items w
WHERE w.id = p.work
  AND w.lead_token = p.token
  AND w.lead_session = p.session;

CREATE TABLE hub_coordination_requests (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id BIGINT,
    requester_owner TEXT NOT NULL REFERENCES hub_users (id),
    requester_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    requester_agent TEXT NOT NULL,
    requester_session TEXT NOT NULL,
    proposed_work TEXT REFERENCES hub_work_items (id),
    source_provider TEXT,
    source_host TEXT,
    source_repository TEXT,
    source_issue TEXT,
    request_key TEXT,
    proposed_title TEXT NOT NULL,
    proposed_scope TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    created_seq BIGINT NOT NULL REFERENCES hub_events (seq),
    updated_seq BIGINT NOT NULL REFERENCES hub_events (seq),
    CHECK ((source_issue IS NOT NULL)::int + (request_key IS NOT NULL)::int + (proposed_work IS NOT NULL)::int = 1)
);
CREATE INDEX hub_coordination_scope ON hub_coordination_requests
    (provider, host, repository, repository_id, updated_seq, id);
CREATE UNIQUE INDEX hub_coordination_active_fingerprint ON hub_coordination_requests
    (provider, host, repository, COALESCE(repository_id, -1), fingerprint)
    WHERE status IN ('pending', 'responded');

CREATE TABLE hub_coordination_targets (
    request TEXT NOT NULL REFERENCES hub_coordination_requests (id),
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    original_generation BIGINT NOT NULL,
    reasons TEXT NOT NULL,
    reasons_complete BOOLEAN NOT NULL,
    PRIMARY KEY (request, work)
);

CREATE TABLE hub_coordination_acknowledgements (
    request TEXT NOT NULL REFERENCES hub_coordination_requests (id),
    target_work TEXT NOT NULL,
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    agent TEXT NOT NULL,
    session TEXT NOT NULL,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    login TEXT NOT NULL,
    generation BIGINT NOT NULL,
    through_seq BIGINT NOT NULL,
    acknowledged_at BIGINT NOT NULL,
    event_seq BIGINT PRIMARY KEY REFERENCES hub_events (seq)
);
CREATE INDEX hub_coordination_ack_recipient ON hub_coordination_acknowledgements
    (request, target_work, token, session, generation, event_seq);

CREATE TABLE hub_coordination_responses (
    id TEXT PRIMARY KEY,
    request TEXT NOT NULL REFERENCES hub_coordination_requests (id),
    operation_id TEXT NOT NULL,
    target_work TEXT NOT NULL REFERENCES hub_work_items (id),
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    agent TEXT NOT NULL,
    session TEXT NOT NULL,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    login TEXT NOT NULL,
    target_generation BIGINT NOT NULL,
    kind TEXT NOT NULL,
    note TEXT,
    created_at BIGINT NOT NULL,
    event_seq BIGINT NOT NULL UNIQUE REFERENCES hub_events (seq),
    UNIQUE (request, operation_id)
);
CREATE INDEX hub_coordination_response_target ON hub_coordination_responses
    (request, target_work, token, session, target_generation, event_seq);

CREATE TABLE hub_coordination_operations (
    request TEXT NOT NULL REFERENCES hub_coordination_requests (id),
    operation_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    event_seq BIGINT NOT NULL REFERENCES hub_events (seq),
    PRIMARY KEY (request, operation_id)
);

CREATE TABLE hub_coordination_versions (
    request TEXT NOT NULL REFERENCES hub_coordination_requests (id),
    event_seq BIGINT NOT NULL REFERENCES hub_events (seq),
    status TEXT NOT NULL,
    proposed_work TEXT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (request, event_seq)
);
CREATE INDEX hub_coordination_versions_event ON hub_coordination_versions (event_seq, request);
