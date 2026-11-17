-- requires: hub_answers
-- The requested paths a coordination reply released from the target Work, as a JSON array of
-- display paths. NULL when the reply released none, so older rows read as empty.
ALTER TABLE hub_coordination_responses ADD COLUMN released_paths TEXT;
