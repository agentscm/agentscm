-- Where each external run came from: `forge` (read from the forge), `workflow` (a run of the
-- repository's deploy workflow), `recorded` (written through the Hub) or `agent` (an agent's
-- report). The source is part of the key, so a workflow run and a deployment with the same id
-- stay apart.
ALTER TABLE hub_external_runs ADD COLUMN source TEXT NOT NULL DEFAULT 'forge';
ALTER TABLE hub_external_runs ALTER COLUMN source DROP DEFAULT;
ALTER TABLE hub_external_runs DROP CONSTRAINT hub_external_runs_pkey;
ALTER TABLE hub_external_runs ADD PRIMARY KEY (repository, provider, kind, source, run_id, attempt);
