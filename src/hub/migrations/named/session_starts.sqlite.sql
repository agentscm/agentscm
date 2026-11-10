-- Which session started which, as the started session declared it. Per owner, not per agent
-- token, so the link holds in every repository both sessions work in.
CREATE TABLE hub_session_starts (
    owner TEXT NOT NULL REFERENCES hub_users (id),
    session TEXT NOT NULL,
    started_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (owner, session)
);
