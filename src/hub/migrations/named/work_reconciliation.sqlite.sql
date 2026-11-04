-- requires: work_reservations
-- Declarations are independent of checkpoint relation replacement. No evidence or success is inferred.
CREATE TABLE hub_work_declarations (
    work TEXT PRIMARY KEY REFERENCES hub_work_items (id) ON DELETE CASCADE,
    snapshot TEXT NOT NULL,
    event_seq BIGINT NOT NULL
);
INSERT INTO hub_work_declarations (work,snapshot,event_seq)
SELECT w.id, json_object('links',json('[]'),'prerequisites',json(COALESCE((
    SELECT json_group_array(json_object('id','legacy-' || related,'requirement',json_object('kind','legacy_work','work_id',related)))
    FROM (SELECT related FROM hub_work_relations WHERE work=w.id AND kind='depends_on' ORDER BY related)
), '[]'))),w.updated_seq FROM hub_work_items w;
UPDATE hub_work_usage SET charged_bytes=charged_bytes+COALESCE((
    SELECT SUM(length(CAST(d.snapshot AS BLOB))+256) FROM hub_work_declarations d
    JOIN hub_work_items w ON w.id=d.work WHERE w.provider=hub_work_usage.provider AND w.host=hub_work_usage.host
    AND w.repository=hub_work_usage.repository AND w.repository_id IS hub_work_usage.repository_id
),0);
