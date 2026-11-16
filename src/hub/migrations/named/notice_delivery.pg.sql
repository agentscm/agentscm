-- requires: work_dependencies
-- requires: session_lifecycle
-- References and transport facts only; Work and source domains retain authority.
CREATE TABLE hub_notice_deliveries (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_id BIGINT,
    token TEXT NOT NULL,
    session TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_id TEXT NOT NULL,
    source_version TEXT NOT NULL,
    snapshot TEXT NOT NULL
);
CREATE INDEX hub_notice_recipient ON hub_notice_deliveries(token,session,id);
CREATE INDEX hub_notice_source ON hub_notice_deliveries(owner,provider,host,repository,source_kind,source_id,source_version,session);
