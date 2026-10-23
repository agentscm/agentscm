-- Warn-once: files the session holds that no refusing pre-edit hook has checked yet (JSON array
-- of paths). A call that cannot refuse (MCP, a hook after the edit) adds its new files here, so
-- the first refusing edit of one still refuses it once if another agent holds it.
ALTER TABLE hub_sessions ADD COLUMN unchecked TEXT NOT NULL DEFAULT '[]';
