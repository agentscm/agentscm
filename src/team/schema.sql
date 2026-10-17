CREATE TABLE team_schema (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    version INTEGER NOT NULL CHECK (version = 1)
);
INSERT INTO team_schema VALUES (TRUE, 1);
CREATE TABLE team_repositories (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    max_bytes BIGINT NOT NULL CHECK (max_bytes > 0),
    max_objects BIGINT NOT NULL CHECK (max_objects > 0),
    allocated_bytes BIGINT NOT NULL DEFAULT 0 CHECK (allocated_bytes >= 0 AND allocated_bytes <= max_bytes),
    allocated_objects BIGINT NOT NULL DEFAULT 0 CHECK (allocated_objects >= 0 AND allocated_objects <= max_objects),
    sequence BIGINT NOT NULL DEFAULT 0 CHECK (sequence >= 0),
    PRIMARY KEY (trust_domain, repository)
);
CREATE TABLE team_publications (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    generation BIGINT NOT NULL,
    publication TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    request TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('reserved','uploading','qualifying','complete','abandoned')),
    expires_at TIMESTAMPTZ NOT NULL,
    manifest TEXT,
    manifest_digest TEXT,
    PRIMARY KEY (trust_domain, repository, generation, publication),
    FOREIGN KEY (trust_domain, repository) REFERENCES team_repositories
);
CREATE TABLE team_roots (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    generation BIGINT NOT NULL,
    publication TEXT NOT NULL,
    root TEXT NOT NULL,
    PRIMARY KEY (trust_domain, repository, generation, publication),
    FOREIGN KEY (trust_domain, repository, generation, publication) REFERENCES team_publications
);
CREATE TABLE team_outcomes (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    generation BIGINT NOT NULL,
    operation TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    outcome TEXT NOT NULL,
    PRIMARY KEY (trust_domain, repository, generation, operation),
    FOREIGN KEY (trust_domain, repository) REFERENCES team_repositories
);
CREATE TABLE team_events (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    outcome TEXT NOT NULL,
    PRIMARY KEY (trust_domain, repository, sequence),
    FOREIGN KEY (trust_domain, repository) REFERENCES team_repositories
);
CREATE TABLE team_outbox (
    trust_domain TEXT NOT NULL,
    repository TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (trust_domain, repository, sequence),
    FOREIGN KEY (trust_domain, repository, sequence) REFERENCES team_events
);
