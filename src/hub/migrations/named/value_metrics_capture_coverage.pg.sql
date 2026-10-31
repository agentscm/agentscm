-- requires: value_metrics
CREATE TABLE hub_value_delivery_pruning (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key BIGINT NOT NULL,
    first_pruned_at BIGINT NOT NULL,
    last_pruned_at BIGINT NOT NULL,
    PRIMARY KEY(provider,host,repository,repository_key)
);

CREATE INDEX hub_value_request_identity ON hub_work_model_usage
    (model_provider,(request::jsonb->>'provider_request_id'));
