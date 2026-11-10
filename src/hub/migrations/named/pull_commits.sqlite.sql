-- requires: catchup
-- Derived from catch-up merge observations, which compaction prunes: which pull request a merge
-- or head commit belongs to, kept so `why` still resolves a commit after its observation is gone.
-- It is never an authority for admission, conflicts or settling Work.
CREATE TABLE hub_catchup_pull_commits (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    pull_number INTEGER NOT NULL,
    head_commit TEXT NOT NULL,
    merge_commit TEXT NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, pull_number, merge_commit)
);
CREATE INDEX hub_catchup_pull_merge_commit
    ON hub_catchup_pull_commits (provider, host, repository, repository_key, merge_commit);
CREATE INDEX hub_catchup_pull_head_commit
    ON hub_catchup_pull_commits (provider, host, repository, repository_key, head_commit);

INSERT INTO hub_catchup_pull_commits
    (provider, host, repository, repository_key, pull_number, head_commit, merge_commit)
SELECT DISTINCT provider, host, repository, repository_key, pull_number, head_commit, merge_commit
FROM hub_catchup_merges;
