-- requires: refusal_outcomes
-- requires: duplicate_starts
-- requires: handoff_answered_via_slack
-- requires: publication_binding
-- requires: skills_repo_local
-- Rows these tables kept by repository name alone are now kept by `repository_scope`: the
-- writer's forge host and stable repository id, or its host and name when no id is known (see
-- `store::principal_scope`). Two repositories with one name, on two hosts or one after the
-- other, no longer read each other's rows. Each row takes the scope of the token that wrote it.
-- A row no token or connected repository accounts for keeps its bare name, which no caller's
-- scope equals: it is kept, and no repository reads it.

-- Each token's scope: a token with no repository id takes the id of the repository connected
-- under its name on its owner's host, as the Hub reads it (`store::principal_scope`).
CREATE VIEW hub_token_scopes AS
SELECT t.id AS token, u.host || '/' || COALESCE('@repo-id:' || COALESCE(t.repository_id, (
        SELECT r.repository_id FROM hub_repositories r
        WHERE r.host = u.host AND lower(r.full_name) = lower(t.repository)
        ORDER BY r.created_at, r.id LIMIT 1)), t.repository) AS scope
FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner;

ALTER TABLE hub_intents ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_intents SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_intents.token), repository);
ALTER TABLE hub_intents ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_intents_live;
CREATE INDEX hub_intents_live ON hub_intents (repository_scope, expires_at);

ALTER TABLE hub_intent_paths ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_intent_paths SET repository_scope = COALESCE((
    SELECT i.repository_scope FROM hub_intents i WHERE i.id = hub_intent_paths.intent), repository);
ALTER TABLE hub_intent_paths ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_intent_paths_key;
CREATE INDEX hub_intent_paths_key ON hub_intent_paths (repository_scope, key);

ALTER TABLE hub_handoffs ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_handoffs SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_handoffs.requester_token), repository);
ALTER TABLE hub_handoffs ALTER COLUMN repository_scope DROP DEFAULT;
CREATE INDEX hub_handoffs_scope ON hub_handoffs (repository_scope, path, state);

ALTER TABLE hub_contested_edits ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_contested_edits SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_contested_edits.token), repository);
ALTER TABLE hub_contested_edits ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_contested_edits_scope;
CREATE INDEX hub_contested_edits_scope ON hub_contested_edits (repository_scope, observed_at);

ALTER TABLE hub_duplicate_starts ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_duplicate_starts SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_duplicate_starts.token), repository);
ALTER TABLE hub_duplicate_starts ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_duplicate_starts_scope;
CREATE INDEX hub_duplicate_starts_scope ON hub_duplicate_starts (repository_scope, observed_at);

ALTER TABLE hub_hook_overhead ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_hook_overhead SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_hook_overhead.token), repository);
ALTER TABLE hub_hook_overhead ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_hook_overhead_scope;
CREATE INDEX hub_hook_overhead_scope ON hub_hook_overhead (repository_scope, updated_at);

ALTER TABLE hub_publications ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_publications SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_publications.token), repository);
ALTER TABLE hub_publications ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_publications_repository;
DROP INDEX hub_publications_pull;
DROP INDEX hub_publications_commit;
CREATE INDEX hub_publications_repository ON hub_publications (repository_scope, created_at);
CREATE INDEX hub_publications_pull ON hub_publications (repository_scope, pr_number, created_at);
CREATE INDEX hub_publications_commit ON hub_publications (repository_scope, commit_sha);

-- A branch's candidate head follows its publication. Two old names of one repository can name
-- the same branch; the newer publication's head is kept, as a new publication would replace it.
CREATE TABLE hub_candidate_heads_scoped (
    repository_scope TEXT NOT NULL,
    branch TEXT NOT NULL,
    publication TEXT NOT NULL,
    PRIMARY KEY (repository_scope, branch)
);
INSERT INTO hub_candidate_heads_scoped (repository_scope, branch, publication)
SELECT DISTINCT ON (COALESCE(p.repository_scope, h.repository), h.branch)
    COALESCE(p.repository_scope, h.repository), h.branch, h.publication
FROM hub_candidate_heads h LEFT JOIN hub_publications p ON p.id = h.publication
ORDER BY COALESCE(p.repository_scope, h.repository), h.branch,
    p.updated_at DESC NULLS LAST, h.publication DESC;
DROP TABLE hub_candidate_heads;
ALTER TABLE hub_candidate_heads_scoped RENAME TO hub_candidate_heads;
ALTER TABLE hub_candidate_heads RENAME CONSTRAINT hub_candidate_heads_scoped_pkey
    TO hub_candidate_heads_pkey;

ALTER TABLE hub_skill_evidence ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_skill_evidence SET repository_scope = COALESCE((
    SELECT s.scope FROM hub_token_scopes s WHERE s.token = hub_skill_evidence.token_id), repository);
ALTER TABLE hub_skill_evidence ALTER COLUMN repository_scope DROP DEFAULT;
DROP INDEX hub_skill_evidence_branch;
CREATE INDEX hub_skill_evidence_branch ON hub_skill_evidence (repository_scope, branch);

-- A default-branch lock is the connected repository's, when exactly one connected repository has
-- the name; a reported lock is its reporting token's. Where two rows land on one repository the
-- newer read is kept, as the next read would replace the other.
ALTER TABLE hub_skill_adoptions ADD COLUMN repository_scope TEXT NOT NULL DEFAULT '';
UPDATE hub_skill_adoptions a SET repository_scope = COALESCE(
    CASE a.source
        WHEN 'reported_by_checkout' THEN (
            SELECT s.scope FROM hub_token_scopes s WHERE s.token = a.reported_by)
        ELSE (
            SELECT MIN(r.host) || '/@repo-id:' || MIN(r.repository_id)
            FROM hub_repositories r WHERE lower(r.full_name) = lower(a.repository)
            HAVING COUNT(*) = 1)
    END, a.repository);
ALTER TABLE hub_skill_adoptions ALTER COLUMN repository_scope DROP DEFAULT;
DELETE FROM hub_skill_adoptions a USING hub_skill_adoptions b
WHERE a.repository_scope = b.repository_scope
    AND (a.observed_at, a.repository) < (b.observed_at, b.repository);
ALTER TABLE hub_skill_adoptions DROP CONSTRAINT hub_skill_adoptions_pkey;
ALTER TABLE hub_skill_adoptions ADD PRIMARY KEY (repository_scope);

DROP VIEW hub_token_scopes;
