-- requires: catchup
-- Whether the merged pull request's head branch lived in the repository it merged into. NULL is a
-- row recorded before this column existed; it is never backfilled and never settles or links Work.
ALTER TABLE hub_catchup_merges ADD COLUMN same_repository_head BOOLEAN;
CREATE INDEX hub_catchup_merges_by_branch
    ON hub_catchup_merges (provider, host, repository, repository_key, branch);
