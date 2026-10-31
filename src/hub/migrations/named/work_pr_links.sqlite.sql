-- One explicit relationship per Work. Forge identity comes from the owning Work;
-- no mutable repository name or inferred branch association is stored here.
CREATE TABLE hub_work_pr_links (
    work TEXT PRIMARY KEY REFERENCES hub_work_items (id) ON DELETE CASCADE,
    snapshot TEXT NOT NULL
);

DROP TRIGGER hub_events_no_delete;
CREATE TRIGGER hub_events_no_delete BEFORE DELETE ON hub_events
WHEN OLD.kind NOT IN (
    'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
    'coordination_requested', 'coordination_bound', 'coordination_retargeted',
    'coordination_acknowledged', 'coordination_responded',
    'coordination_resolved', 'coordination_withdrawn'
)
BEGIN SELECT RAISE(ABORT, 'hub_events is append-only'); END;
