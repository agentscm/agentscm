-- The decision checkpoints of each Work, kept for `why` and search after compaction removes the
-- Work versions that first recorded them. `checkpoint` is the stored Checkpoint JSON; `summary`
-- is its text for search. Writers keep at most the newest 100 per Work.
CREATE TABLE hub_work_decisions (
    work TEXT NOT NULL REFERENCES hub_work_items (id) ON DELETE CASCADE,
    event_seq INTEGER NOT NULL,
    summary TEXT NOT NULL,
    checkpoint TEXT NOT NULL,
    PRIMARY KEY (work, event_seq)
);

INSERT INTO hub_work_decisions (work, event_seq, summary, checkpoint)
SELECT v.work, v.event_seq, json_extract(v.snapshot, '$.current.summary'),
       json_extract(v.snapshot, '$.current')
FROM hub_work_versions v JOIN hub_events e ON e.seq = v.event_seq
WHERE e.kind = 'work_checkpoint' AND json_extract(v.snapshot, '$.current.kind') = 'decision';
