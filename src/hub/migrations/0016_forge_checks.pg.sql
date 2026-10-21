CREATE TABLE hub_forge_checks (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    number BIGINT NOT NULL,
    context TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    state TEXT NOT NULL,
    binding_digest TEXT,
    description TEXT NOT NULL,
    phase TEXT NOT NULL,
    generation BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (repository, number, context)
);
