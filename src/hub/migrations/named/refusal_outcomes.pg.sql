-- One row per contested-edit refusal or overlap notice and the outcome the Hub observed
-- afterwards; hook overhead per session. Both keep rows 30 days and at most 1,000 per token.
CREATE TABLE hub_contested_edits (
    repository TEXT NOT NULL,
    token TEXT NOT NULL,
    session TEXT NOT NULL,
    path TEXT NOT NULL,
    hold TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('refusal', 'notice')),
    outcome TEXT NOT NULL
        CHECK (outcome IN ('pending', 'pushed_through', 'asked', 'handed_off', 'proceeded')),
    observed_at BIGINT NOT NULL,
    resolved_at BIGINT,
    PRIMARY KEY (token, session, path, hold, kind)
);
CREATE INDEX hub_contested_edits_scope ON hub_contested_edits (repository, observed_at);
CREATE INDEX hub_contested_edits_pending ON hub_contested_edits (token, session, outcome);
CREATE INDEX hub_contested_edits_token ON hub_contested_edits (token, observed_at);
CREATE TABLE hub_hook_overhead (
    repository TEXT NOT NULL,
    token TEXT NOT NULL,
    session TEXT NOT NULL,
    agent TEXT NOT NULL,
    hub_calls BIGINT NOT NULL,
    injected_start BIGINT NOT NULL,
    injected_refusal BIGINT NOT NULL,
    injected_other BIGINT NOT NULL,
    client_calls BIGINT NOT NULL,
    client_ms_total BIGINT NOT NULL,
    client_ms_max BIGINT NOT NULL,
    client_timeouts BIGINT NOT NULL,
    client_failed_open BIGINT NOT NULL,
    first_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (token, session, agent)
);
CREATE INDEX hub_hook_overhead_scope ON hub_hook_overhead (repository, updated_at);
CREATE INDEX hub_hook_overhead_token ON hub_hook_overhead (token, updated_at);
