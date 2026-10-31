CREATE TABLE hub_value_observations (
    seq BIGSERIAL PRIMARY KEY,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    observation_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (length(payload) <= 4096),
    observed_at BIGINT NOT NULL,
    UNIQUE (provider, host, repository, repository_key, observation_id)
);
CREATE INDEX hub_value_observations_scope ON hub_value_observations
    (provider, host, repository, repository_key, seq);
CREATE TABLE hub_value_capture_gaps (
    work TEXT NOT NULL REFERENCES hub_work_items(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    rejected BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (work, kind)
);
CREATE TABLE hub_value_receipt_work (
    token TEXT NOT NULL REFERENCES hub_agent_tokens(id) ON DELETE CASCADE,
    session_key TEXT NOT NULL,
    work TEXT NOT NULL REFERENCES hub_work_items(id) ON DELETE CASCADE,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY(token,session_key,work)
);
CREATE TABLE hub_value_receipt_announcements (
    token TEXT NOT NULL,
    session_key TEXT NOT NULL,
    work TEXT NOT NULL,
    generation BIGINT NOT NULL,
    version TEXT NOT NULL,
    announced_at BIGINT NOT NULL,
    PRIMARY KEY(token,session_key,work,generation,version),
    FOREIGN KEY(token,session_key,work) REFERENCES hub_value_receipt_work(token,session_key,work) ON DELETE CASCADE
);
CREATE TABLE hub_value_receipt_preferences (
    token TEXT NOT NULL REFERENCES hub_agent_tokens(id) ON DELETE CASCADE,
    session_key TEXT NOT NULL,
    mode TEXT NOT NULL CHECK(mode IN ('automatic','requested_only','disabled')),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY(token,session_key)
);
