-- requires: work_reservation_leases
-- When the Hub last gave a held reservation one lease of grace after a restart or a forward clock
-- jump. NULL when the lead session's own activity, an admission or a decision set the lease since,
-- so the projection never reports the Hub's grace as the lead's renewal.
ALTER TABLE hub_work_items ADD COLUMN reservation_grace_at BIGINT;

-- Recovery previews list the unfinished Work that depends on the Work they would end.
CREATE INDEX hub_work_relations_related ON hub_work_relations (related, kind, work);
