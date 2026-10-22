-- External CI and deployment observations; never read by checks, admission or publication updates.
CREATE TABLE hub_external_runs (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    run_id BIGINT NOT NULL,
    attempt BIGINT NOT NULL,
    commit_sha TEXT NOT NULL,
    branch TEXT,
    environment TEXT,
    name TEXT NOT NULL,
    outcome TEXT NOT NULL,
    provider_updated_at BIGINT NOT NULL,
    html_url TEXT,
    PRIMARY KEY (repository, provider, kind, run_id, attempt)
);
CREATE INDEX hub_external_runs_commit ON hub_external_runs (repository, commit_sha);
CREATE TABLE hub_external_reconciles (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    commit_sha TEXT NOT NULL,
    reconciled_at BIGINT NOT NULL,
    complete BOOLEAN NOT NULL,
    PRIMARY KEY (repository, commit_sha)
);
