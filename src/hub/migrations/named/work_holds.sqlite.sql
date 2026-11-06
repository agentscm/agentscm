-- requires: work_issue_links
-- A person's hold (pause or resume) on one Work item is a Work event with its own version, so
-- its bounded history compacts like the other Work events. Other event guards are unchanged.

DROP TRIGGER hub_events_no_delete;
CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
WHEN OLD.kind NOT IN (
    'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
    'work_transition', 'work_lead_credential', 'work_issue_link', 'work_hold',
    'coordination_requested', 'coordination_bound', 'coordination_retargeted',
    'coordination_acknowledged', 'coordination_responded',
    'coordination_resolved', 'coordination_withdrawn', 'coordination_target_released'
)
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;
