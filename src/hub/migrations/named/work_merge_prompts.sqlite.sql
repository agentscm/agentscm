-- requires: work_reservations
-- One row per Work and audience told that its merged pull request closed its issue. The primary
-- key makes the claim atomic, so a notice is delivered once however many hooks race.
CREATE TABLE hub_work_merge_prompts (
    work TEXT NOT NULL REFERENCES hub_work_items (id) ON DELETE CASCADE,
    audience TEXT NOT NULL,
    delivered_at BIGINT NOT NULL,
    PRIMARY KEY (work, audience)
);
