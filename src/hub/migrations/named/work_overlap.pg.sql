-- requires: work_recovery
-- The epoch prevents release/re-acquisition or scope changes away and back from reviving consent.
ALTER TABLE hub_work_items ADD COLUMN overlap_epoch BIGINT NOT NULL DEFAULT 0;
CREATE TABLE hub_work_overlap_consents (
    id TEXT PRIMARY KEY,
    request TEXT NOT NULL REFERENCES hub_coordination_requests(id) ON DELETE CASCADE,
    target_work TEXT NOT NULL REFERENCES hub_work_items(id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL,
    accepted_work TEXT REFERENCES hub_work_items(id) ON DELETE CASCADE,
    payload TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    UNIQUE(request, target_work, operation_id)
);
CREATE INDEX hub_work_overlap_accepted ON hub_work_overlap_consents(accepted_work);
CREATE INDEX hub_work_overlap_target ON hub_work_overlap_consents(target_work);
CREATE FUNCTION hub_work_overlap_epoch_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.scope IS DISTINCT FROM NEW.scope
        OR OLD.lead_token IS DISTINCT FROM NEW.lead_token
        OR OLD.lead_session IS DISTINCT FROM NEW.lead_session
        OR OLD.generation IS DISTINCT FROM NEW.generation
        OR (OLD.reservation_basis IS NULL) <> (NEW.reservation_basis IS NULL)
        OR (NEW.reservation_expires_at > OLD.reservation_expires_at AND
            (OLD.reservation_expires_at <= NEW.reservation_renewed_at
             OR OLD.reservation_expires_at <= NEW.reservation_grace_at)) THEN
        NEW.overlap_epoch := OLD.overlap_epoch + 1;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER hub_work_overlap_epoch BEFORE UPDATE ON hub_work_items
FOR EACH ROW EXECUTE FUNCTION hub_work_overlap_epoch_update();
