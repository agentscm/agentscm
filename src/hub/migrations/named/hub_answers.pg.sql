-- A person answering for their own agent from the Hub's web pages. A response or handoff the
-- agent made itself keeps the defaults, so older readers and rows are unchanged.
-- `via` is 'hub' when the responding agent's owner answered a coordination request there.
ALTER TABLE hub_coordination_responses ADD COLUMN via TEXT;
-- The request was asked, and the answer given, by the agent's owner from the Hub's web pages.
ALTER TABLE hub_handoffs ADD COLUMN asked_via_hub BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE hub_handoffs ADD COLUMN answered_via_hub BOOLEAN NOT NULL DEFAULT FALSE;
