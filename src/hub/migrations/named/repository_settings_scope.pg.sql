-- requires: repository_scope
-- requires: conversation_sharing_default
-- Copy only settings whose legacy numeric ID names exactly one retained forge host.
-- Originals stay intact. An unresolved identity or conflicting scoped value stops the
-- transaction: the operator must map the setting to an explicit canonical scope and
-- archive its original key before retrying. Never fan out a sharing choice or inherit
-- a potentially broader default after discarding a legacy value.
CREATE TEMP TABLE hub_legacy_repository_settings ON COMMIT DROP AS
WITH prefixes(prefix, per_person) AS (VALUES
    ('conversation_sharing:repo:', 0),
    ('conversation_sharing:opt_out:', 1),
    ('conversation_sharing:notified:', 1),
    ('work_digest:repo:', 0),
    ('work_digest:opt_out:', 1),
    ('skill_forge_status:repo:', 0),
    ('merged_release:repo:', 0)
), matched AS (
    SELECT s.*, p.prefix, p.per_person, substr(s.key, length(p.prefix) + 1) AS suffix
    FROM hub_settings s JOIN prefixes p ON substr(s.key, 1, length(p.prefix)) = p.prefix
), parsed AS (
    SELECT *, CASE WHEN per_person = 1 THEN substr(suffix, 1, CASE WHEN strpos(suffix, ':') > 0 THEN strpos(suffix, ':') - 1 ELSE 0 END) END AS person,
        CASE WHEN per_person = 1 THEN substr(suffix, strpos(suffix, ':') + 1) ELSE suffix END AS id_text
    FROM matched
), identified AS (
    SELECT *, CASE WHEN id_text ~ '^[1-9][0-9]{0,18}$' AND (length(id_text) < 19 OR id_text <= '9223372036854775807') THEN CAST(id_text AS BIGINT) END AS repository_id FROM parsed
), hosts AS (
    SELECT repository_id, host FROM hub_repositories
    UNION
    SELECT t.repository_id, u.host FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
    WHERE t.repository_id IS NOT NULL
), resolved AS (
    SELECT i.*, (SELECT COUNT(*) FROM hosts h WHERE h.repository_id = i.repository_id) AS host_count,
        (SELECT MIN(host) FROM hosts h WHERE h.repository_id = i.repository_id) AS host
    FROM identified i WHERE i.repository_id IS NOT NULL
)
SELECT *, prefix || COALESCE(person || ':', '') || host || '/@repo-id:' || CAST(repository_id AS TEXT) AS scoped_key
FROM resolved;

CREATE TEMP TABLE hub_repository_settings_scope_guard (
    resolved INTEGER CONSTRAINT "Map legacy settings to one forge host before upgrading" CHECK (resolved = 0)
) ON COMMIT DROP;
INSERT INTO hub_repository_settings_scope_guard (resolved)
SELECT 1 FROM hub_legacy_repository_settings old
WHERE old.host_count <> 1
    OR NOT EXISTS (SELECT 1 FROM hub_users writer
        WHERE writer.id = old.updated_by AND writer.host = old.host)
    OR (old.per_person = 1 AND NOT EXISTS (
        SELECT 1 FROM hub_users u WHERE u.id = old.person AND u.host = old.host))
    OR EXISTS (SELECT 1 FROM hub_settings current
        WHERE current.key = old.scoped_key AND current.value <> old.value);

INSERT INTO hub_settings (key, value, updated_at, updated_by)
SELECT scoped_key, value, updated_at, updated_by FROM hub_legacy_repository_settings WHERE 1 = 1
ON CONFLICT (key) DO NOTHING;
DROP TABLE hub_repository_settings_scope_guard;
DROP TABLE hub_legacy_repository_settings;
