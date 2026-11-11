-- requires: work_reconciliation
-- A bounded latest-version mailbox, derived exclusively from existing prerequisite declarations.
CREATE TABLE hub_work_dependency_notices (
    work TEXT PRIMARY KEY REFERENCES hub_work_items(id) ON DELETE CASCADE,
    event_seq BIGINT NOT NULL,
    snapshot TEXT NOT NULL
);
CREATE INDEX hub_work_dependency_notice_sequence ON hub_work_dependency_notices(event_seq);
