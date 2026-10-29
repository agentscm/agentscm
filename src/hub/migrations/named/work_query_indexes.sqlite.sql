-- Unfinished Work is the hot set for overlap, overview and session lookups.
CREATE INDEX hub_work_unfinished ON hub_work_items
    (provider, host, repository, repository_id, id) WHERE state <> 'finished';
