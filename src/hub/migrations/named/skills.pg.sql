CREATE TABLE hub_skill_adoptions (
    repository TEXT NOT NULL PRIMARY KEY,
    source TEXT NOT NULL CHECK (source IN ('default_branch', 'reported_by_checkout')),
    lock_json TEXT NOT NULL CHECK (octet_length(lock_json) <= 262144),
    default_branch TEXT,
    observed_at BIGINT NOT NULL,
    reported_by TEXT
);

CREATE TABLE hub_skill_evidence (
    seq BIGSERIAL PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    repository TEXT NOT NULL,
    branch TEXT NOT NULL,
    run_id TEXT NOT NULL,
    skill TEXT NOT NULL,
    skill_sha256 TEXT NOT NULL,
    gate TEXT NOT NULL,
    kind TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    assurance TEXT NOT NULL,
    summary TEXT NOT NULL,
    url TEXT,
    pull_request BIGINT,
    detail TEXT,
    token_id TEXT NOT NULL,
    session TEXT NOT NULL,
    agent TEXT NOT NULL,
    recorded_at BIGINT NOT NULL
);
CREATE INDEX hub_skill_evidence_branch ON hub_skill_evidence (repository, branch);
CREATE INDEX hub_skill_evidence_run ON hub_skill_evidence (run_id);
