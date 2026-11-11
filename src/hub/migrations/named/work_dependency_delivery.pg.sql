-- requires: work_dependencies
-- An attempted advisory summary is not receipt or acknowledgement of the retained notice.
ALTER TABLE hub_sessions ADD COLUMN dependency_notice_cursor BIGINT NOT NULL DEFAULT 0;
