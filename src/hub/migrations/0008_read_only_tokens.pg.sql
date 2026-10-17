-- A read-only token may only call the read routes (what the status page needs).
ALTER TABLE hub_agent_tokens ADD COLUMN read_only BOOLEAN NOT NULL DEFAULT FALSE;

-- The status page finds a repository's tokens by name.
CREATE INDEX hub_agent_tokens_repository ON hub_agent_tokens (repository);
