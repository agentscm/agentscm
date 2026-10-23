CREATE TABLE hub_merge_groups (
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    head_sha TEXT NOT NULL,
    head_ref TEXT NOT NULL,
    base_ref TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    pr_number BIGINT NOT NULL,
    pr_head_sha TEXT NOT NULL,
    context TEXT NOT NULL,
    ended_reason TEXT,
    observed_at BIGINT NOT NULL,
    ended_at BIGINT,
    state TEXT,
    binding_digest TEXT,
    description TEXT,
    phase TEXT,
    generation BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL,
    ruleset_requirement TEXT,
    status_source TEXT,
    PRIMARY KEY (repository, head_sha)
);
CREATE INDEX hub_merge_groups_active ON hub_merge_groups (repository, observed_at)
    WHERE ended_reason IS NULL;
