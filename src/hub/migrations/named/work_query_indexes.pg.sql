-- Unfinished Work is the hot set for overlap, overview and session lookups.
CREATE INDEX hub_work_unfinished ON hub_work_items
    (provider, host, repository, repository_id, id) WHERE state <> 'finished';
CREATE INDEX hub_work_unfinished_lead ON hub_work_items
    (lead_token, lead_session, provider, host, repository, repository_id, id)
    WHERE state <> 'finished';
-- Session lookups start from a token and session; the primary key leads with work.
CREATE INDEX hub_work_participant_session ON hub_work_participants (token, session, work);
