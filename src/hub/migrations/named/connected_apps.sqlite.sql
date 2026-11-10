-- requires: devices
-- The connected app a credential was issued to, when it is not a computer.
ALTER TABLE hub_devices ADD COLUMN app_id TEXT;
