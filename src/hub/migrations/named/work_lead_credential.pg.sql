-- requires: work_reservations
-- A credential carry records that the same owner and session kept the lead under
-- a new token or agent label. The event is eligible for compaction, like the
-- other work events. The function still rejects an update and every other delete.

CREATE OR REPLACE FUNCTION hub_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.kind IN (
        'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
        'work_transition', 'work_lead_credential',
        'coordination_requested', 'coordination_bound', 'coordination_retargeted',
        'coordination_acknowledged', 'coordination_responded',
        'coordination_resolved', 'coordination_withdrawn', 'coordination_target_released'
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'hub_events is append-only';
END
$$;
