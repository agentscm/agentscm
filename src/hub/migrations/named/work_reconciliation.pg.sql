-- requires: work_reservations
-- Declarations are independent of checkpoint relation replacement. No evidence or success is inferred.
CREATE TABLE hub_work_declarations (
    work TEXT PRIMARY KEY REFERENCES hub_work_items (id) ON DELETE CASCADE,
    snapshot TEXT NOT NULL,
    event_seq BIGINT NOT NULL
);
INSERT INTO hub_work_declarations (work,snapshot,event_seq)
SELECT w.id, jsonb_build_object('links','[]'::jsonb,'prerequisites',COALESCE((
    SELECT jsonb_agg(jsonb_build_object('id','legacy-' || related,'requirement',jsonb_build_object('kind','legacy_work','work_id',related)) ORDER BY related COLLATE "C")
    FROM hub_work_relations WHERE work=w.id AND kind='depends_on'
), '[]'::jsonb))::text,w.updated_seq FROM hub_work_items w;
UPDATE hub_work_usage SET charged_bytes=charged_bytes+COALESCE((
    SELECT SUM(octet_length(d.snapshot)::bigint+256) FROM hub_work_declarations d
    JOIN hub_work_items w ON w.id=d.work WHERE w.provider=hub_work_usage.provider AND w.host=hub_work_usage.host
    AND w.repository=hub_work_usage.repository AND w.repository_id IS NOT DISTINCT FROM hub_work_usage.repository_id
),0);
