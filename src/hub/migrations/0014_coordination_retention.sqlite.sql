CREATE TABLE hub_coordination_retention (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id INTEGER,
    event_floor_seq INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX hub_coordination_retention_scope ON hub_coordination_retention
    (provider, host, repository, ifnull(repository_id, -1));

CREATE TABLE hub_session_aliases (
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    observed_session TEXT NOT NULL,
    canonical_session TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (token, observed_session),
    CHECK (observed_session <> canonical_session)
);
CREATE UNIQUE INDEX hub_session_alias_canonical
    ON hub_session_aliases (token, canonical_session);

DROP TRIGGER hub_events_no_delete;
CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
WHEN OLD.kind NOT IN (
    'work_started', 'work_joined', 'work_takeover', 'work_checkpoint',
    'coordination_requested', 'coordination_bound', 'coordination_retargeted',
    'coordination_acknowledged', 'coordination_responded',
    'coordination_resolved', 'coordination_withdrawn'
)
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;
