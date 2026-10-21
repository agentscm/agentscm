CREATE TABLE hub_forge_checks (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    number INTEGER NOT NULL,
    context TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    state TEXT NOT NULL,
    binding_digest TEXT,
    description TEXT NOT NULL,
    phase TEXT NOT NULL,
    generation INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (repository, number, context)
);
CREATE INDEX hub_publications_pull ON hub_publications (lower(repository), pr_number, created_at);
