-- A computer signed in to the Hub. Only the credential's digest is stored.
CREATE TABLE hub_devices (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES hub_users (id),
    name TEXT NOT NULL,
    digest TEXT NOT NULL UNIQUE,
    created_at BIGINT NOT NULL,
    last_used_at BIGINT,
    revoked_at BIGINT,
    revoked_by TEXT
);

CREATE INDEX hub_devices_owner ON hub_devices (owner);

-- The computer that minted a token; revoking the computer revokes its tokens.
ALTER TABLE hub_agent_tokens ADD COLUMN device_id TEXT REFERENCES hub_devices (id);

CREATE INDEX hub_agent_tokens_device ON hub_agent_tokens (device_id);

-- Refused sign-ins and computer mints for administrators: one row per person and reason.
CREATE TABLE hub_sign_in_refusals (
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    forge_user_id BIGINT NOT NULL,
    login TEXT NOT NULL,
    reason TEXT NOT NULL,
    via TEXT NOT NULL,
    at BIGINT NOT NULL,
    count BIGINT NOT NULL,
    PRIMARY KEY (provider, host, forge_user_id, reason)
);

CREATE INDEX hub_sign_in_refusals_at ON hub_sign_in_refusals (at);
