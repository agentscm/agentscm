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
    observed_at INTEGER NOT NULL,
    resolved_at INTEGER,
    PRIMARY KEY (token, session, path, hold, kind)
) STRICT;
CREATE INDEX hub_contested_edits_scope ON hub_contested_edits (repository, observed_at);
CREATE INDEX hub_contested_edits_pending ON hub_contested_edits (token, session, outcome);
CREATE INDEX hub_contested_edits_token ON hub_contested_edits (token, observed_at);
CREATE TABLE hub_hook_overhead (
    repository TEXT NOT NULL,
    token TEXT NOT NULL,
    session TEXT NOT NULL,
    agent TEXT NOT NULL,
    hub_calls INTEGER NOT NULL,
    injected_start INTEGER NOT NULL,
    injected_refusal INTEGER NOT NULL,
    injected_other INTEGER NOT NULL,
    client_calls INTEGER NOT NULL,
    client_ms_total INTEGER NOT NULL,
    client_ms_max INTEGER NOT NULL,
    client_timeouts INTEGER NOT NULL,
    client_failed_open INTEGER NOT NULL,
    first_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (token, session, agent)
) STRICT;
CREATE INDEX hub_hook_overhead_scope ON hub_hook_overhead (repository, updated_at);
CREATE INDEX hub_hook_overhead_token ON hub_hook_overhead (token, updated_at);
