-- Writes a bounded store limit refused, for the operator: one row per repository and limit.
-- repository_key is the stable repository ID, or -1 when the caller had none.
CREATE TABLE hub_capacity_refusals (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    repository_key INTEGER NOT NULL,
    limit_kind TEXT NOT NULL,
    at INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (provider, host, repository, repository_key, limit_kind)
);

CREATE INDEX hub_capacity_refusals_at ON hub_capacity_refusals (at);
