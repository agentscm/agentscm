-- The forge profile name, a cached display value refreshed at each sign-in.
ALTER TABLE hub_users ADD COLUMN display_name TEXT;
