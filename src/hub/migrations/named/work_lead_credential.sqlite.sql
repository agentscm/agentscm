-- requires: work_reservations
-- A credential carry records that the same owner and session kept the lead under
-- a new token or agent label. The event is eligible for compaction, like the
-- other work events. The trigger still rejects an update and every other delete.

DROP TRIGGER hub_events_no_delete;
CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
WHEN OLD.kind NOT IN (
    'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
    'work_transition', 'work_lead_credential',
    'coordination_requested', 'coordination_bound', 'coordination_retargeted',
    'coordination_acknowledged', 'coordination_responded',
    'coordination_resolved', 'coordination_withdrawn', 'coordination_target_released'
)
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;
