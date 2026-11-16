-- requires: shared_conversations
CREATE TABLE hub_change_conversation_occurrences (
    work TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    session_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    digest TEXT NOT NULL,
    author TEXT NOT NULL,
    PRIMARY KEY (work, generation, session_id, event_id),
    FOREIGN KEY (work, generation, session_id, event_id)
        REFERENCES hub_change_events (work, generation, session_id, event_id),
    FOREIGN KEY (work, digest, author)
        REFERENCES hub_change_shared_bodies (work, digest, author)
) STRICT;
CREATE INDEX hub_change_conversation_page ON hub_change_conversation_occurrences
    (work, session_id, generation, sequence, digest);

CREATE INDEX hub_change_conversation_native ON hub_change_conversation_occurrences
    (work, session_id, event_id, author, digest, generation);

-- Original declarations retain their exact event attribution.
INSERT OR IGNORE INTO hub_change_conversation_occurrences
    (work, generation, session_id, event_id, sequence, digest, author)
SELECT work, generation, session_id, event_id, sequence, digest, author
FROM hub_change_shared_bodies;

-- Duplicate-only batches can come from another owner. Recover only unambiguous attribution.
-- CROSS JOIN fixes the join order so each batch item finds its event, token and body
-- by primary key instead of repeatedly scanning events and bodies within a work item.
INSERT OR IGNORE INTO hub_change_conversation_occurrences
    (work, generation, session_id, event_id, sequence, digest, author)
SELECT e.work, e.generation, e.session_id, e.event_id, e.sequence, b.digest, b.author
FROM hub_change_operations o
CROSS JOIN json_each(o.request, '$.events') item
CROSS JOIN hub_change_events e
CROSS JOIN hub_agent_tokens t
CROSS JOIN hub_change_shared_bodies b
WHERE o.kind='event_batch'
    AND e.work=o.work
    AND e.generation=json_extract(o.request, '$.generation')
    AND e.session_id=json_extract(o.request, '$.session.session_id')
    AND e.event_id=json_extract(item.value, '$.event_id')
    AND t.id=o.author_token
    AND b.work=e.work AND b.author=t.owner
    AND b.digest=json_extract(item.value, '$.artifact.digest')
    AND json_extract(item.value, '$.artifact.retention')='shared_approved'
    AND json(e.event)=json(item.value)
GROUP BY e.work, e.generation, e.session_id, e.event_id, e.sequence, b.digest
HAVING COUNT(DISTINCT b.author)=1;
