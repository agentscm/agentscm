-- Repository memory: short notes agents write for later readers of the same repository. Rows are
-- keyed by repository scope, so a renamed repository keeps its notes. Author fields are copied at
-- write time; promotion is set only by a signed-in person.
CREATE TABLE hub_memory_notes (
    id TEXT PRIMARY KEY,
    repository_scope TEXT NOT NULL,
    body TEXT NOT NULL,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    owner_login TEXT NOT NULL,
    token TEXT NOT NULL,
    token_name TEXT NOT NULL,
    session TEXT,
    created_at BIGINT NOT NULL,
    promoted_by TEXT REFERENCES hub_users (id),
    promoted_by_login TEXT,
    promoted_at BIGINT
);
CREATE INDEX hub_memory_notes_scope ON hub_memory_notes (repository_scope, created_at, id);
