ALTER TABLE team_schema DROP CONSTRAINT team_schema_version_check;
ALTER TABLE team_schema
    ADD CONSTRAINT team_schema_version_check CHECK (version IN (1, 2, 3));

ALTER TABLE team_publications
    ADD COLUMN source_profile TEXT NOT NULL DEFAULT 'agentscm-source-v1'
    CHECK (source_profile IN ('agentscm-source-v1', 'agentscm-source-v2'));

CREATE TABLE team_source_cursors (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    principal TEXT NOT NULL,
    principal_version BIGINT NOT NULL CHECK (principal_version > 0),
    authority_generation BIGINT NOT NULL CHECK (authority_generation > 0),
    operation TEXT NOT NULL CHECK (
        octet_length(operation) BETWEEN 1 AND 128
        AND operation ~ '^[A-Za-z0-9_-]+$'
    ),
    binding_digest TEXT NOT NULL CHECK (
        length(binding_digest) = 64
        AND binding_digest ~ '^[0-9a-f]{64}$'
    ),
    binding TEXT NOT NULL CHECK (octet_length(binding) <= 8192),
    state TEXT NOT NULL CHECK (state IN ('pending', 'active', 'terminal', 'cancelled')),
    version BIGINT NOT NULL CHECK (version >= 0),
    current_token_digest TEXT CHECK (
        current_token_digest IS NULL OR (
            length(current_token_digest) = 64
            AND current_token_digest ~ '^[0-9a-f]{64}$'
        )
    ),
    current_continuation TEXT CHECK (
        current_continuation IS NULL
        OR octet_length(current_continuation) <= 16777216
    ),
    initial_reply TEXT CHECK (
        initial_reply IS NULL OR octet_length(initial_reply) <= 1048576
    ),
    initial_reply_digest TEXT CHECK (
        initial_reply_digest IS NULL OR (
            length(initial_reply_digest) = 64
            AND initial_reply_digest ~ '^[0-9a-f]{64}$'
        )
    ),
    previous_token_digest TEXT CHECK (
        previous_token_digest IS NULL OR (
            length(previous_token_digest) = 64
            AND previous_token_digest ~ '^[0-9a-f]{64}$'
        )
    ),
    previous_reply TEXT CHECK (
        previous_reply IS NULL OR octet_length(previous_reply) <= 1048576
    ),
    previous_reply_digest TEXT CHECK (
        previous_reply_digest IS NULL OR (
            length(previous_reply_digest) = 64
            AND previous_reply_digest ~ '^[0-9a-f]{64}$'
        )
    ),
    reserved_bytes BIGINT NOT NULL CHECK (
        reserved_bytes BETWEEN 1 AND 19922944
    ),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (
        trust_domain,
        repository,
        principal,
        principal_version,
        authority_generation,
        operation
    ),
    FOREIGN KEY (trust_domain, repository)
        REFERENCES team_repositories(trust_domain, repository),
    FOREIGN KEY (trust_domain, repository, principal)
        REFERENCES team_principals(trust_domain, repository, principal),
    CHECK (
        octet_length(trust_domain)
        + octet_length(repository)
        + octet_length(principal)
        + octet_length(operation)
        + octet_length(binding_digest)
        + octet_length(binding)
        + octet_length(state)
        + COALESCE(octet_length(current_token_digest), 0)
        + COALESCE(octet_length(current_continuation), 0)
        + COALESCE(octet_length(initial_reply), 0)
        + COALESCE(octet_length(initial_reply_digest), 0)
        + COALESCE(octet_length(previous_token_digest), 0)
        + COALESCE(octet_length(previous_reply), 0)
        + COALESCE(octet_length(previous_reply_digest), 0)
        <= reserved_bytes
    ),
    CHECK (
        (state = 'pending'
            AND version = 0
            AND current_token_digest IS NULL
            AND current_continuation IS NULL
            AND initial_reply IS NULL
            AND initial_reply_digest IS NULL
            AND previous_token_digest IS NULL
            AND previous_reply IS NULL
            AND previous_reply_digest IS NULL)
        OR
        (state = 'active'
            AND version >= 1
            AND current_token_digest IS NOT NULL
            AND current_continuation IS NOT NULL
            AND initial_reply IS NOT NULL
            AND initial_reply_digest IS NOT NULL
            AND ((version = 1
                    AND previous_token_digest IS NULL
                    AND previous_reply IS NULL
                    AND previous_reply_digest IS NULL)
                OR (version >= 2
                    AND previous_token_digest IS NOT NULL
                    AND previous_reply IS NOT NULL
                    AND previous_reply_digest IS NOT NULL)))
        OR
        (state = 'terminal'
            AND version >= 1
            AND current_token_digest IS NULL
            AND current_continuation IS NULL
            AND initial_reply IS NOT NULL
            AND initial_reply_digest IS NOT NULL
            AND ((version = 1
                    AND previous_token_digest IS NULL
                    AND previous_reply IS NULL
                    AND previous_reply_digest IS NULL)
                OR (version >= 2
                    AND previous_token_digest IS NOT NULL
                    AND previous_reply IS NOT NULL
                    AND previous_reply_digest IS NOT NULL)))
        OR
        (state = 'cancelled'
            AND current_token_digest IS NULL
            AND current_continuation IS NULL
            AND initial_reply IS NULL
            AND initial_reply_digest IS NULL
            AND previous_token_digest IS NULL
            AND previous_reply IS NULL
            AND previous_reply_digest IS NULL)
    )
);

CREATE INDEX team_source_cursors_current_token_idx
    ON team_source_cursors(trust_domain, repository, current_token_digest)
    WHERE current_token_digest IS NOT NULL;
CREATE INDEX team_source_cursors_previous_token_idx
    ON team_source_cursors(trust_domain, repository, previous_token_digest)
    WHERE previous_token_digest IS NOT NULL;

UPDATE team_schema SET version=3 WHERE singleton AND version=2;
