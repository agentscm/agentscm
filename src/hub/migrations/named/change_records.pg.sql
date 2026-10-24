CREATE TABLE hub_change_operations (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    operation_id TEXT NOT NULL,
    author_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    kind TEXT NOT NULL CHECK (kind IN ('event_batch', 'action_start', 'action_finish')),
    request TEXT NOT NULL CHECK (octet_length(request) <= 262144),
    response TEXT NOT NULL CHECK (octet_length(response) <= 16384),
    received_at BIGINT NOT NULL,
    PRIMARY KEY (work, operation_id)
);

CREATE TABLE hub_change_sessions (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    session_id TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    identity TEXT NOT NULL CHECK (octet_length(identity) <= 4096),
    watermark BIGINT NOT NULL CHECK (watermark >= 0),
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    gap_count BIGINT NOT NULL CHECK (gap_count >= 0),
    started BOOLEAN NOT NULL,
    ended BOOLEAN NOT NULL,
    start_sequence BIGINT CHECK (start_sequence IS NULL OR start_sequence > 0),
    end_sequence BIGINT CHECK (end_sequence IS NULL OR end_sequence > 0),
    lifecycle_valid BOOLEAN NOT NULL,
    first_observed_at BIGINT NOT NULL CHECK (first_observed_at >= 0),
    last_observed_at BIGINT NOT NULL CHECK (last_observed_at >= first_observed_at),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (work, generation, session_id)
);

CREATE TABLE hub_change_events (
    work TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    session_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    event TEXT NOT NULL CHECK (octet_length(event) <= 8192),
    observed_at BIGINT NOT NULL CHECK (observed_at >= 0),
    PRIMARY KEY (work, generation, session_id, event_id),
    UNIQUE (work, generation, session_id, sequence),
    FOREIGN KEY (work, generation, session_id) REFERENCES hub_change_sessions (work, generation, session_id)
);
CREATE INDEX hub_change_events_page ON hub_change_events (work, generation, session_id, sequence);

CREATE TABLE hub_change_actions (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    action_id TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    session_id TEXT NOT NULL,
    author_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    source_id TEXT NOT NULL CHECK (octet_length(source_id) = 64),
    source_kind TEXT NOT NULL CHECK (source_kind IN ('native', 'git')),
    start_request TEXT NOT NULL CHECK (octet_length(start_request) <= 32768),
    finish_request TEXT CHECK (finish_request IS NULL OR octet_length(finish_request) <= 32768),
    started_at BIGINT NOT NULL CHECK (started_at >= 0),
    received_at BIGINT NOT NULL CHECK (received_at >= 0),
    received_order BIGINT NOT NULL CHECK (received_order > 0),
    finished_at BIGINT CHECK (finished_at IS NULL OR finished_at >= started_at),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (work, action_id)
);
CREATE INDEX hub_change_actions_latest ON hub_change_actions (work, received_order DESC);
