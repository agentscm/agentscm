-- requires: work_recovery
-- The epoch prevents release/re-acquisition or scope changes away and back from reviving consent.
ALTER TABLE hub_work_items ADD COLUMN overlap_epoch BIGINT NOT NULL DEFAULT 0;
CREATE TABLE hub_work_overlap_consents (
    id TEXT PRIMARY KEY,
    request TEXT NOT NULL REFERENCES hub_coordination_requests(id) ON DELETE CASCADE,
    target_work TEXT NOT NULL REFERENCES hub_work_items(id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    UNIQUE(request, target_work, operation_id)
);
CREATE INDEX hub_work_overlap_target ON hub_work_overlap_consents(target_work);
CREATE TRIGGER hub_work_overlap_epoch AFTER UPDATE ON hub_work_items
WHEN OLD.scope IS DISTINCT FROM NEW.scope
        OR OLD.lead_token IS DISTINCT FROM NEW.lead_token
        OR OLD.lead_session IS DISTINCT FROM NEW.lead_session
        OR OLD.generation IS DISTINCT FROM NEW.generation
        OR (OLD.reservation_basis IS NULL) <> (NEW.reservation_basis IS NULL)
        OR (NEW.reservation_expires_at > OLD.reservation_expires_at AND
            (OLD.reservation_expires_at <= NEW.reservation_renewed_at
             OR OLD.reservation_expires_at <= NEW.reservation_grace_at))
BEGIN
    UPDATE hub_work_items SET overlap_epoch = OLD.overlap_epoch + 1 WHERE id = NEW.id;
END;
