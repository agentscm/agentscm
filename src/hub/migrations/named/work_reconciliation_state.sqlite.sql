-- requires: work_reconciliation
CREATE TABLE hub_work_reconciliation_state (
    work TEXT PRIMARY KEY REFERENCES hub_work_items(id) ON DELETE CASCADE,
    snapshot TEXT NOT NULL
);
