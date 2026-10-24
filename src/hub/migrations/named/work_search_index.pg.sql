CREATE INDEX hub_work_creation ON hub_work_items
    (provider, host, repository, repository_id, created_at DESC, id DESC);
