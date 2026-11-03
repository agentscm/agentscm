-- Trusted local CI device authorizations. Public keys only. The repository is the stable
-- hub_repositories row, so a rename never moves an authorization. No reference to
-- hub_devices (Hub computers): re-signing in there must not touch these rows.
CREATE TABLE local_ci_devices (
    device_id TEXT PRIMARY KEY,
    repository TEXT NOT NULL REFERENCES hub_repositories (id),
    owner TEXT NOT NULL REFERENCES hub_users (id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    document TEXT NOT NULL,
    digest TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX local_ci_devices_repository ON local_ci_devices (repository, device_id);

-- A key id is enrolled once, ever: rotated, dropped or revoked keys stay here.
CREATE TABLE local_ci_device_keys (
    key_id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES local_ci_devices (device_id)
);

CREATE INDEX local_ci_device_keys_device ON local_ci_device_keys (device_id);

CREATE TABLE local_ci_device_requests (
    request_id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES local_ci_devices (device_id),
    fingerprint TEXT NOT NULL,
    generation INTEGER NOT NULL
);
