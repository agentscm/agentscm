-- The decision checkpoints of each Work, kept for `why` and search after compaction removes the
-- Work versions that first recorded them. `checkpoint` is the stored Checkpoint JSON; `summary`
-- is its text for search. At most the newest 100 per Work are kept, here and by writers.
CREATE TABLE hub_work_decisions (
    work TEXT NOT NULL REFERENCES hub_work_items (id) ON DELETE CASCADE,
    event_seq INTEGER NOT NULL,
    summary TEXT NOT NULL,
    checkpoint TEXT NOT NULL,
    PRIMARY KEY (work, event_seq)
);

INSERT INTO hub_work_decisions (work, event_seq, summary, checkpoint)
SELECT work, event_seq, summary, checkpoint FROM (
    SELECT v.work, v.event_seq, json_extract(v.snapshot, '$.current.summary') AS summary,
           json_extract(v.snapshot, '$.current') AS checkpoint,
           ROW_NUMBER() OVER (PARTITION BY v.work ORDER BY v.event_seq DESC) AS newest
    FROM hub_work_versions v JOIN hub_events e ON e.seq = v.event_seq
    WHERE e.kind = 'work_checkpoint'
      AND json_extract(v.snapshot, '$.current.kind') = 'decision'
) WHERE newest <= 100;
