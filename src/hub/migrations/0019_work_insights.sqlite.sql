CREATE TABLE hub_work_model_usage (
    work TEXT NOT NULL REFERENCES hub_work_items (id),
    observation_id TEXT NOT NULL,
    author_token TEXT NOT NULL REFERENCES hub_agent_tokens (id),
    request TEXT NOT NULL,
    generation INTEGER NOT NULL,
    session TEXT NOT NULL,
    phase TEXT NOT NULL,
    outcome TEXT NOT NULL,
    agent TEXT NOT NULL,
    model_provider TEXT NOT NULL,
    model TEXT NOT NULL,
    usage_basis TEXT NOT NULL,
    source_commit TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cost_kind TEXT,
    cost_currency TEXT,
    cost_amount_micros INTEGER,
    cost_basis TEXT,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (work, observation_id)
);
CREATE INDEX hub_work_model_usage_work ON hub_work_model_usage (work, received_at, observation_id);
CREATE INDEX hub_work_insight_scope ON hub_work_items
    (provider, host, repository, repository_id, created_at, id);
CREATE INDEX hub_publications_commit ON hub_publications (repository, commit_sha);
