-- Hub-wide settings as opaque key/value text; the owning module validates the value.
CREATE TABLE hub_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    updated_by TEXT NOT NULL
);
