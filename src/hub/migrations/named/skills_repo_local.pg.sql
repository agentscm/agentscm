-- requires: skills
-- The repository skills a `repo_local` pin names, read with the lock: JSON `[LocalBundle]`,
-- at most 8 bundles of a 64 KiB manifest and a 64 KiB procedure each.
ALTER TABLE hub_skill_adoptions ADD COLUMN local_skills TEXT
    CHECK (local_skills IS NULL OR length(local_skills) <= 2097152);
