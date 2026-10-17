CREATE TABLE hub_users (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    forge_user_id BIGINT NOT NULL,
    login TEXT NOT NULL,
    disabled BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    UNIQUE (provider, host, forge_user_id)
);

CREATE TABLE hub_agent_tokens (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    name TEXT NOT NULL,
    repository TEXT NOT NULL,
    digest TEXT NOT NULL UNIQUE,
    expires_at BIGINT NOT NULL,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL
);

CREATE INDEX hub_agent_tokens_owner ON hub_agent_tokens (owner);

CREATE TABLE hub_events (
    seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    at BIGINT NOT NULL,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    data TEXT NOT NULL
);

CREATE FUNCTION hub_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'hub_events is append-only';
END
$$;

CREATE TRIGGER hub_events_append_only BEFORE UPDATE OR DELETE ON hub_events
FOR EACH ROW EXECUTE FUNCTION hub_events_append_only();

CREATE TRIGGER hub_events_no_truncate BEFORE TRUNCATE ON hub_events
FOR EACH STATEMENT EXECUTE FUNCTION hub_events_append_only();
