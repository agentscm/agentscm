ALTER TABLE team_schema DROP CONSTRAINT team_schema_version_check;
ALTER TABLE team_schema
    ADD CONSTRAINT team_schema_version_check CHECK (version IN (1, 2, 3, 4));

-- NULL for operator reservations and for rows created before this version.
ALTER TABLE team_publications ADD COLUMN owner TEXT;

UPDATE team_schema SET version=4 WHERE singleton AND version=3;
