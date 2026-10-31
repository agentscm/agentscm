-- requires: value_metrics
CREATE TABLE hub_value_delivery_pruning (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    first_pruned_at INTEGER NOT NULL,
    last_pruned_at INTEGER NOT NULL,
    PRIMARY KEY(provider,host,repository,repository_key)
) STRICT;

CREATE INDEX hub_value_request_identity ON hub_work_model_usage
    (model_provider,json_extract(request,'$.provider_request_id'));
