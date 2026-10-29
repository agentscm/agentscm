-- requires: change_records
-- Lifecycle counters let capture ingest update a session from its new batch alone.
-- A compaction restart continues the same session, so it is not counted as an origin.
ALTER TABLE hub_change_sessions
    ADD COLUMN start_count BIGINT NOT NULL DEFAULT 0 CHECK (start_count >= 0),
    ADD COLUMN end_count BIGINT NOT NULL DEFAULT 0 CHECK (end_count >= 0);
UPDATE hub_change_sessions s SET
    start_count = (
        SELECT COUNT(*) FROM hub_change_events e
        WHERE e.work = s.work AND e.generation = s.generation AND e.session_id = s.session_id
          AND e.event::jsonb->>'kind' = 'session_started'
          AND e.event::jsonb->>'boundary' IS DISTINCT FROM 'compact'),
    end_count = (
        SELECT COUNT(*) FROM hub_change_events e
        WHERE e.work = s.work AND e.generation = s.generation AND e.session_id = s.session_id
          AND e.event::jsonb->>'kind' = 'session_ended');
