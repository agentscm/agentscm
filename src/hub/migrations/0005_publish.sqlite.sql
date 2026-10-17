-- A native revision published as a forge branch and pull request. `manifest` is the
-- canonical JSON the client sent; the branch and pull request columns are the
-- forge facts the hub has recorded, and history is in hub_events.
CREATE TABLE hub_publications (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    revision TEXT NOT NULL,
    event_seq INTEGER NOT NULL,
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

CREATE INDEX hub_publications_repository ON hub_publications (repository, created_at);
