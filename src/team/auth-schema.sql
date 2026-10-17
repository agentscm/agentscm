ALTER TABLE team_schema DROP CONSTRAINT team_schema_version_check;
ALTER TABLE team_schema
    ADD CONSTRAINT team_schema_version_check CHECK (version IN (1, 2));

ALTER TABLE team_repositories ADD COLUMN retired BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE team_principals (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    principal TEXT NOT NULL,
    credential_digest TEXT NOT NULL CHECK (
        length(credential_digest) = 64
        AND credential_digest ~ '^[0-9a-f]{64}$'
    ),
    version BIGINT NOT NULL CHECK (version > 0),
    enabled BOOLEAN NOT NULL,
    PRIMARY KEY (trust_domain, repository, principal),
    UNIQUE (trust_domain, repository, credential_digest),
    FOREIGN KEY (trust_domain, repository)
        REFERENCES team_repositories(trust_domain, repository)
);

CREATE TABLE team_roles (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    principal TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN (
        'reader',
        'contributor',
        'reviewer',
        'integrator',
        'administrator'
    )),
    PRIMARY KEY (trust_domain, repository, principal, role),
    FOREIGN KEY (trust_domain, repository, principal)
        REFERENCES team_principals(trust_domain, repository, principal)
);

CREATE TABLE team_sessions (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    session_digest TEXT NOT NULL CHECK (
        length(session_digest) = 64
        AND session_digest ~ '^[0-9a-f]{64}$'
    ),
    csrf_digest TEXT NOT NULL CHECK (
        length(csrf_digest) = 64
        AND csrf_digest ~ '^[0-9a-f]{64}$'
    ),
    principal TEXT NOT NULL,
    principal_version BIGINT NOT NULL CHECK (principal_version > 0),
    authority_generation BIGINT NOT NULL CHECK (authority_generation > 0),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (trust_domain, repository, session_digest),
    FOREIGN KEY (trust_domain, repository, principal)
        REFERENCES team_principals(trust_domain, repository, principal)
);

CREATE INDEX team_sessions_principal_active_idx
    ON team_sessions(trust_domain, repository, principal, revoked, expires_at);
CREATE INDEX team_sessions_expiry_idx
    ON team_sessions(trust_domain, repository, expires_at);

ALTER TABLE team_outcomes
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'publication'
    CHECK (kind IN ('publication', 'identity', 'retirement'));
ALTER TABLE team_events
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'publication'
    CHECK (kind IN ('publication', 'identity', 'retirement'));
ALTER TABLE team_outbox
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'publication'
    CHECK (kind IN ('publication', 'identity', 'retirement'));

UPDATE team_schema SET version=2 WHERE singleton AND version=1;
