-- Warn-once: files whose first contested edit was refused (JSON array of paths), and the unix
-- seconds of the session's recent refusals (JSON array, for the rate limit).
ALTER TABLE hub_sessions ADD COLUMN denied TEXT NOT NULL DEFAULT '[]';
ALTER TABLE hub_sessions ADD COLUMN deny_times TEXT NOT NULL DEFAULT '[]';
