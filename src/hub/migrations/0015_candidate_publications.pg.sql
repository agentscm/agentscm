ALTER TABLE hub_publications ALTER COLUMN event_seq DROP NOT NULL;
CREATE TABLE hub_candidate_heads (repository TEXT NOT NULL, branch TEXT NOT NULL, publication TEXT NOT NULL, PRIMARY KEY(repository, branch));
