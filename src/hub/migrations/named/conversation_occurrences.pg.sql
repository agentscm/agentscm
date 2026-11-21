-- requires: shared_conversations
CREATE TABLE hub_change_conversation_occurrences (
    work TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    session_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    digest TEXT NOT NULL,
    author TEXT NOT NULL,
    PRIMARY KEY (work, generation, session_id, event_id),
    FOREIGN KEY (work, generation, session_id, event_id)
        REFERENCES hub_change_events (work, generation, session_id, event_id),
    FOREIGN KEY (work, digest, author)
        REFERENCES hub_change_shared_bodies (work, digest, author)
);
CREATE INDEX hub_change_conversation_page ON hub_change_conversation_occurrences
    (work, session_id, generation, sequence, digest);

CREATE INDEX hub_change_conversation_native ON hub_change_conversation_occurrences
    (work, session_id, event_id, author, digest, generation);

-- Original declarations retain their exact event attribution.
INSERT INTO hub_change_conversation_occurrences
    (work, generation, session_id, event_id, sequence, digest, author)
SELECT work, generation, session_id, event_id, sequence, digest, author
FROM hub_change_shared_bodies
ON CONFLICT DO NOTHING;

-- Duplicate-only batches can come from another owner. Recover only unambiguous attribution.
-- Materialize batch items so the planner can look up each event by its primary key.
-- Joining them directly can repeatedly scan every event in a session for each item.
INSERT INTO hub_change_conversation_occurrences
    (work, generation, session_id, event_id, sequence, digest, author)
WITH batch_item AS MATERIALIZED (
    SELECT o.work, o.author_token, (o.request::jsonb->>'generation')::bigint AS generation,
        o.request::jsonb->'session'->>'session_id' AS session_id, item
    FROM hub_change_operations o
    CROSS JOIN LATERAL jsonb_array_elements(COALESCE(o.request::jsonb->'events', '[]'::jsonb)) item
    WHERE o.kind='event_batch' AND item->'artifact'->>'retention'='shared_approved'
)
SELECT e.work, e.generation, e.session_id, e.event_id, e.sequence, b.digest, MIN(b.author)
FROM batch_item i
JOIN hub_agent_tokens t ON t.id=i.author_token
JOIN hub_change_events e ON e.work=i.work
    AND e.generation=i.generation
    AND e.session_id=i.session_id
    AND e.event_id=i.item->>'event_id'
JOIN hub_change_shared_bodies b ON b.work=e.work AND b.author=t.owner
    AND b.digest=i.item->'artifact'->>'digest'
WHERE e.event::jsonb=i.item
GROUP BY e.work, e.generation, e.session_id, e.event_id, e.sequence, b.digest
HAVING COUNT(DISTINCT b.author)=1
ON CONFLICT DO NOTHING;
