-- A person's sign-in to the Hub's web pages. Only the cookie secret's digest is stored, so a
-- restart or deploy keeps people signed in.
CREATE TABLE hub_browser_sessions (
    digest TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX hub_browser_sessions_owner ON hub_browser_sessions (owner);

CREATE INDEX hub_browser_sessions_expires ON hub_browser_sessions (expires_at);
