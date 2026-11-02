-- requires: catchup
-- Derived from catch-up merge observations, which compaction prunes. A row says the branch had a
-- merged pull request at merged_at, or was the base of one (is_base). The Hub uses it to stop
-- listing Work whose branch merged; it is never an authority for admission or conflicts.
CREATE TABLE hub_catchup_branch_merges (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    branch TEXT NOT NULL,
    merged_at INTEGER NOT NULL,
    is_base INTEGER NOT NULL DEFAULT 0 CHECK (is_base IN (0, 1)),
    PRIMARY KEY (provider, host, repository, repository_key, branch)
);

INSERT INTO hub_catchup_branch_merges
    (provider, host, repository, repository_key, branch, merged_at, is_base)
SELECT provider, host, repository, repository_key, branch,
       MAX(COALESCE(occurred_at, observed_at)), 0
FROM hub_catchup_merges
GROUP BY provider, host, repository, repository_key, branch;

INSERT INTO hub_catchup_branch_merges
    (provider, host, repository, repository_key, branch, merged_at, is_base)
SELECT provider, host, repository, repository_key, base,
       MAX(COALESCE(occurred_at, observed_at)), 1
FROM hub_catchup_merges
WHERE true
GROUP BY provider, host, repository, repository_key, base
ON CONFLICT (provider, host, repository, repository_key, branch)
DO UPDATE SET is_base = 1, merged_at = MAX(merged_at, excluded.merged_at);
