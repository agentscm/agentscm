-- One explicit relationship per Work. Forge identity comes from the owning Work;
-- no mutable repository name or inferred branch association is stored here.
CREATE TABLE hub_work_pr_links (
    work TEXT PRIMARY KEY REFERENCES hub_work_items (id) ON DELETE CASCADE,
    snapshot TEXT NOT NULL
);

CREATE OR REPLACE FUNCTION hub_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.kind IN (
        'work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link',
        'coordination_requested', 'coordination_bound', 'coordination_retargeted',
        'coordination_acknowledged', 'coordination_responded',
        'coordination_resolved', 'coordination_withdrawn'
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'hub_events is append-only';
END
$$;
