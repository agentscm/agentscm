-- requires: notice_delivery
-- Owner identity is immutable and independent of agent token, session and Work lifetimes.
CREATE TABLE hub_owner_notices (
    id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    sender_owner TEXT NOT NULL,
    sender_token TEXT NOT NULL,
    recipient_owner TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id BIGINT NOT NULL,
    source_kind TEXT NOT NULL,
    source_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    snapshot TEXT NOT NULL,
    UNIQUE(sender_owner, provider, host, repository, repository_id, idempotency_key),
    UNIQUE(recipient_owner, provider, host, repository, repository_id, source_kind, source_id)
);
CREATE INDEX hub_owner_notice_mailbox ON hub_owner_notices(recipient_owner,provider,host,repository,repository_id,id);
CREATE INDEX hub_owner_notice_rate ON hub_owner_notices(sender_token,recipient_owner,provider,host,repository,created_at);
