CREATE TABLE hub_skill_adoptions (
    repository TEXT NOT NULL PRIMARY KEY,
    source TEXT NOT NULL CHECK (source IN ('default_branch', 'reported_by_checkout')),
    lock_json TEXT NOT NULL CHECK (length(lock_json) <= 262144),
    default_branch TEXT,
    observed_at INTEGER NOT NULL,
    reported_by TEXT
) STRICT;

CREATE TABLE hub_skill_evidence (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
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
    pull_request INTEGER,
    detail TEXT,
    token_id TEXT NOT NULL,
    session TEXT NOT NULL,
    agent TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
) STRICT;
CREATE INDEX hub_skill_evidence_branch ON hub_skill_evidence (repository, branch);
CREATE INDEX hub_skill_evidence_run ON hub_skill_evidence (run_id);
