-- One row the first time a session's lead start named a forge issue whose unfinished Work another
-- person leads. Outcomes are read later from Work, participant and contested-edit rows.
-- Kept 30 days and at most 1,000 per token.
CREATE TABLE hub_duplicate_starts (
    repository TEXT NOT NULL,
    token TEXT NOT NULL,
    session TEXT NOT NULL,
    owner TEXT NOT NULL,
    holder_work TEXT NOT NULL,
    holder_owner TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (token, session, holder_work),
    CHECK (owner <> holder_owner)
) STRICT;
CREATE INDEX hub_duplicate_starts_scope ON hub_duplicate_starts (repository, observed_at);
CREATE INDEX hub_duplicate_starts_token ON hub_duplicate_starts (token, observed_at);
