-- requires: work_lead_credential
-- Issue links are caller metadata, independent of Work reports and PR evidence.
-- Permit their bounded history compaction without relaxing other event guards.

CREATE OR REPLACE FUNCTION hub_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.kind IN (
        'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
        'work_transition', 'work_lead_credential', 'work_issue_link',
        'coordination_requested', 'coordination_bound', 'coordination_retargeted',
        'coordination_acknowledged', 'coordination_responded',
        'coordination_resolved', 'coordination_withdrawn', 'coordination_target_released'
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'hub_events is append-only';
END
$$;
