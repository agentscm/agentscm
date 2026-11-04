-- requires: work_pr_links
-- A finished Work item records why it ended, and an unfinished one records whether it holds its
-- declared scope. Both columns are NULL when nothing is known or held. Existing unfinished Work
-- keeps blocking exactly as before, so it is backfilled as an implicit reservation. Existing
-- finished Work keeps a NULL outcome: the migration does not invent one.
ALTER TABLE hub_work_items ADD COLUMN terminal_outcome TEXT
    CHECK (terminal_outcome IS NULL OR terminal_outcome IN ('completed', 'abandoned', 'superseded'));
ALTER TABLE hub_work_items ADD COLUMN reservation_basis TEXT
    CHECK (reservation_basis IS NULL OR reservation_basis IN ('legacy_implicit', 'admitted', 'retained'));
UPDATE hub_work_items SET reservation_basis = 'legacy_implicit' WHERE state <> 'finished';

-- Overlap checks read the held reservations of one repository.
CREATE INDEX hub_work_reserved ON hub_work_items
    (provider, host, repository, repository_id, id) WHERE reservation_basis IS NOT NULL;

CREATE OR REPLACE FUNCTION hub_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.kind IN (
        'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
        'work_transition',
        'coordination_requested', 'coordination_bound', 'coordination_retargeted',
        'coordination_acknowledged', 'coordination_responded',
        'coordination_resolved', 'coordination_withdrawn', 'coordination_target_released'
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'hub_events is append-only';
END
$$;
