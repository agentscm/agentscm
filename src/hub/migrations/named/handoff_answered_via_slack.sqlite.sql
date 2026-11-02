-- requires: hub_answers
-- The answer was given by the agent's owner from a Slack button, not the Hub's web pages or the
-- agent. Older rows keep the default.
ALTER TABLE hub_handoffs ADD COLUMN answered_via_slack INTEGER NOT NULL DEFAULT 0;
