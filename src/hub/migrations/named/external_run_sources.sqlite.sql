-- Where each external run came from: `forge` (read from the forge), `workflow` (a run of the
-- repository's deploy workflow), `recorded` (written through the Hub) or `agent` (an agent's
-- report). The source is part of the key, so a workflow run and a deployment with the same id
-- stay apart.
CREATE TABLE hub_external_runs_sourced (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    commit_sha TEXT NOT NULL,
    branch TEXT,
    environment TEXT,
    name TEXT NOT NULL,
    outcome TEXT NOT NULL,
    provider_updated_at INTEGER NOT NULL,
    html_url TEXT,
    PRIMARY KEY (repository, provider, kind, source, run_id, attempt)
);
INSERT INTO hub_external_runs_sourced (repository, provider, kind, source, run_id, attempt,
    commit_sha, branch, environment, name, outcome, provider_updated_at, html_url)
SELECT repository, provider, kind, 'forge', run_id, attempt, commit_sha, branch, environment,
    name, outcome, provider_updated_at, html_url
FROM hub_external_runs;
DROP TABLE hub_external_runs;
ALTER TABLE hub_external_runs_sourced RENAME TO hub_external_runs;
CREATE INDEX hub_external_runs_commit ON hub_external_runs (repository, commit_sha);
