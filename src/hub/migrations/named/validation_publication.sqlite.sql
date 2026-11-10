-- requires: validation_evidence
-- Nonexpiring publication fence: an uncertain remote attempt must be recovered by readback.
CREATE TABLE hub_validation_publication (
 repository TEXT NOT NULL REFERENCES hub_repositories(id),
 proposal INTEGER NOT NULL CHECK(proposal > 0),
 revision INTEGER NOT NULL CHECK(revision > 0),
 state TEXT NOT NULL,
 PRIMARY KEY(repository,proposal)
);
