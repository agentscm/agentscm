CREATE TABLE hub_publications_v2 (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    revision TEXT NOT NULL,
    event_seq INTEGER,
    commit_sha TEXT NOT NULL,
    branch TEXT NOT NULL,
    base TEXT NOT NULL,
    manifest TEXT NOT NULL,
    branch_state TEXT NOT NULL DEFAULT 'pending',
    branch_message TEXT,
    pr_number INTEGER,
    pr_node_id TEXT,
    pr_url TEXT,
    merge_commit TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

INSERT INTO hub_publications_v2 SELECT * FROM hub_publications;
DROP TABLE hub_publications;
ALTER TABLE hub_publications_v2 RENAME TO hub_publications;
CREATE INDEX hub_publications_repository ON hub_publications (repository, created_at);
CREATE TABLE hub_candidate_heads (repository TEXT NOT NULL, branch TEXT NOT NULL, publication TEXT NOT NULL, PRIMARY KEY(repository, branch));
