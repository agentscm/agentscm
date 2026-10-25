CREATE TABLE hub_catchup_repositories (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    next_merge_seq BIGINT NOT NULL DEFAULT 1,
    merge_floor_seq BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key),
    CHECK (repository_key = COALESCE(repository_id, -1))
);

CREATE TABLE hub_catchup_merges (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    seq BIGINT NOT NULL,
    id TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    occurred_at BIGINT,
    observed_at BIGINT NOT NULL,
    title TEXT,
    branch TEXT NOT NULL,
    base TEXT NOT NULL,
    head_commit TEXT NOT NULL,
    merge_commit TEXT NOT NULL,
    pull_number BIGINT NOT NULL,
    paths TEXT NOT NULL,
    paths_complete BOOLEAN NOT NULL,
    payload_bytes BIGINT NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, seq),
    UNIQUE (provider, host, repository, repository_key, pull_number, merge_commit),
    FOREIGN KEY (provider, host, repository, repository_key)
        REFERENCES hub_catchup_repositories (provider, host, repository, repository_key),
    CHECK (repository_key = COALESCE(repository_id, -1))
);
CREATE INDEX hub_catchup_merge_head_scope
    ON hub_catchup_merges(provider, host, repository, repository_key, head_commit, seq);

CREATE OR REPLACE FUNCTION hub_catchup_merges_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.seq <= COALESCE((SELECT merge_floor_seq
        FROM hub_catchup_repositories WHERE provider=OLD.provider AND host=OLD.host
          AND repository=OLD.repository AND repository_key=OLD.repository_key), 0) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'catch-up merge observations are immutable';
END
$$;
CREATE TRIGGER hub_catchup_merges_no_update BEFORE UPDATE OR DELETE ON hub_catchup_merges
FOR EACH ROW EXECUTE FUNCTION hub_catchup_merges_immutable();

CREATE TABLE hub_catchup_sources (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    source TEXT NOT NULL,
    state TEXT NOT NULL,
    checked_at BIGINT NOT NULL,
    unread_pages BIGINT NOT NULL,
    deferred BIGINT NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, source),
    CHECK (repository_key = COALESCE(repository_id, -1))
);

CREATE TABLE hub_catchup_acks (
    owner TEXT NOT NULL REFERENCES hub_users (id),
    subscriber_token TEXT REFERENCES hub_agent_tokens (id),
    subscriber TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    event_cursor BIGINT NOT NULL,
    merge_cursor BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE NULLS NOT DISTINCT
        (owner, subscriber_token, subscriber, provider, host, repository, repository_key),
    CHECK (repository_key = COALESCE(repository_id, -1))
);

CREATE TABLE hub_catchup_deliveries (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    subscriber_token TEXT REFERENCES hub_agent_tokens (id),
    subscriber TEXT NOT NULL,
    delivery_client TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    event_from BIGINT NOT NULL,
    event_through BIGINT NOT NULL,
    merge_from BIGINT NOT NULL,
    merge_through BIGINT NOT NULL,
    page_event_after BIGINT NOT NULL,
    page_merge_after BIGINT NOT NULL,
    next_event_after BIGINT NOT NULL,
    next_merge_after BIGINT NOT NULL,
    page TEXT NOT NULL,
    complete BOOLEAN NOT NULL,
    acknowledged BOOLEAN NOT NULL DEFAULT FALSE,
    ack_actor TEXT REFERENCES hub_agent_tokens (id),
    superseded BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CHECK (repository_key = COALESCE(repository_id, -1))
);
CREATE INDEX hub_catchup_delivery_scope ON hub_catchup_deliveries
    (owner, subscriber_token, subscriber, delivery_client, provider, host, repository, repository_key, created_at);

CREATE TABLE hub_catchup_backfills (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    after_publication TEXT NOT NULL,
    complete BOOLEAN NOT NULL,
    deferred BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key),
    CHECK (repository_key = COALESCE(repository_id, -1))
);

CREATE TABLE hub_catchup_publication_failures (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    repository_id BIGINT,
    publication TEXT NOT NULL,
    attempts BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, publication),
    CHECK (repository_key = COALESCE(repository_id, -1)),
    CHECK (attempts > 0 AND attempts <= 1000000)
);
