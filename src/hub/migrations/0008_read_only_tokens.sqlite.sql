-- A read-only token may only call the read routes (what the status page needs).
ALTER TABLE hub_agent_tokens ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;

-- The status page finds a repository's tokens by name.
CREATE INDEX hub_agent_tokens_repository ON hub_agent_tokens (repository);
