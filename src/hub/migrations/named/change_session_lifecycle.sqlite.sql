-- requires: change_records
-- Lifecycle counters let capture ingest update a session from its new batch alone.
-- A compaction restart continues the same session, so it is not counted as an origin.
ALTER TABLE hub_change_sessions
    ADD COLUMN start_count INTEGER NOT NULL DEFAULT 0 CHECK (start_count >= 0);
ALTER TABLE hub_change_sessions
    ADD COLUMN end_count INTEGER NOT NULL DEFAULT 0 CHECK (end_count >= 0);
UPDATE hub_change_sessions SET
    start_count = (
        SELECT COUNT(*) FROM hub_change_events e
        WHERE e.work = hub_change_sessions.work
          AND e.generation = hub_change_sessions.generation
          AND e.session_id = hub_change_sessions.session_id
          AND json_extract(e.event, '$.kind') = 'session_started'
          AND json_extract(e.event, '$.boundary') IS NOT 'compact'),
    end_count = (
        SELECT COUNT(*) FROM hub_change_events e
        WHERE e.work = hub_change_sessions.work
          AND e.generation = hub_change_sessions.generation
          AND e.session_id = hub_change_sessions.session_id
          AND json_extract(e.event, '$.kind') = 'session_ended');
