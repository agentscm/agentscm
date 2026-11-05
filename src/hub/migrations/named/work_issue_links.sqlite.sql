-- requires: work_lead_credential
-- Issue links are caller metadata, independent of Work reports and PR evidence.
-- Permit their bounded history compaction without relaxing other event guards.

DROP TRIGGER hub_events_no_delete;
CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
WHEN OLD.kind NOT IN (
    'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
    'work_transition', 'work_lead_credential', 'work_issue_link',
    'coordination_requested', 'coordination_bound', 'coordination_retargeted',
    'coordination_acknowledged', 'coordination_responded',
    'coordination_resolved', 'coordination_withdrawn', 'coordination_target_released'
)
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;
