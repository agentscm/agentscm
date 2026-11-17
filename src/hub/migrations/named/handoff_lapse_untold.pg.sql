-- A lapsed ask recorded before lapses were reported carries no reason, so it is never delivered
-- as a holder's release.
UPDATE hub_handoffs SET requester_told = TRUE WHERE state = 'lapsed';
