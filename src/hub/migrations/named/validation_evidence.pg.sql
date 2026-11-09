-- Indefinite audit retention; independent of shared-conversation TTL.
CREATE TABLE hub_validation_summary_config (
 repository TEXT PRIMARY KEY REFERENCES hub_repositories(id),
 connection TEXT NOT NULL REFERENCES hub_connections(id),
 enabled BIGINT NOT NULL CHECK(enabled IN (0,1))
);
CREATE TABLE hub_validation_proposals (
 repository TEXT NOT NULL REFERENCES hub_repositories(id),
 proposal BIGINT NOT NULL CHECK(proposal > 0),
 generation BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY(repository,proposal)
);
CREATE TABLE hub_validation_evidence (
 repository TEXT NOT NULL,
 proposal BIGINT NOT NULL,
 generation BIGINT NOT NULL,
 bundle_sha256 TEXT NOT NULL,
 received_at BIGINT NOT NULL,
 received_by TEXT NOT NULL REFERENCES hub_users(id),
 PRIMARY KEY(repository,proposal,generation),
 UNIQUE(repository,proposal,bundle_sha256),
 FOREIGN KEY(repository,proposal) REFERENCES hub_validation_proposals(repository,proposal)
);
