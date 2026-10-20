CREATE TABLE hub_coordination_retention (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id BIGINT,
    event_floor_seq BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX hub_coordination_retention_scope ON hub_coordination_retention
    (provider, host, repository, COALESCE(repository_id, -1));

CREATE TABLE hub_session_aliases (
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    observed_session TEXT NOT NULL,
    canonical_session TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (token, observed_session),
    CHECK (observed_session <> canonical_session)
);
CREATE UNIQUE INDEX hub_session_alias_canonical
    ON hub_session_aliases (token, canonical_session);

CREATE OR REPLACE FUNCTION hub_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.kind IN (
        'work_started', 'work_joined', 'work_takeover', 'work_checkpoint',
        'coordination_requested', 'coordination_bound', 'coordination_retargeted',
        'coordination_acknowledged', 'coordination_responded',
        'coordination_resolved', 'coordination_withdrawn'
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'hub_events is append-only';
END
$$;
