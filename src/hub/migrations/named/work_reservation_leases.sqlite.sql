-- requires: work_reservations
-- A reservation holds its scope only while its lead session is live. It expires at a server time
-- that the lead session's own activity renews; a lapsed reservation stops blocking and is cleared
-- by the next admission over it. Reservations that exist now start their clock here: 1200 is
-- LEASE_SECONDS and 7200 is RETAIN_CAP_SECONDS.
ALTER TABLE hub_work_items ADD COLUMN reservation_renewed_at INTEGER;
ALTER TABLE hub_work_items ADD COLUMN reservation_expires_at INTEGER;
UPDATE hub_work_items
SET reservation_renewed_at = CAST(strftime('%s', 'now') AS INTEGER),
    reservation_expires_at = CAST(strftime('%s', 'now') AS INTEGER)
        + CASE reservation_basis WHEN 'retained' THEN 7200 ELSE 1200 END
WHERE reservation_basis IS NOT NULL;

-- The last server time the Hub observed. A longer gap (a restart or a forward clock jump) grants
-- the reservations that were live then one lease of grace instead of lapsing them all.
CREATE TABLE hub_lease_clock (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    observed_at INTEGER NOT NULL
);
INSERT INTO hub_lease_clock (id, observed_at) VALUES (1, CAST(strftime('%s', 'now') AS INTEGER));
