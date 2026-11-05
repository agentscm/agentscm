-- requires: work_reservation_leases
-- What keeps a session's leases live. tool_activity_at is the last tool call a hook reported, so
-- the operational projection can say whether the lead's agent renews on every tool call.
-- open_calls counts the session's shell calls that are still running, the last started at
-- open_call_at; a count older than the in-flight cap is stale.
ALTER TABLE hub_sessions ADD COLUMN tool_activity_at INTEGER;
ALTER TABLE hub_sessions ADD COLUMN open_calls INTEGER NOT NULL DEFAULT 0;
ALTER TABLE hub_sessions ADD COLUMN open_call_at INTEGER;
