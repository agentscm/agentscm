-- requires: catchup
ALTER TABLE hub_catchup_sources ADD COLUMN window_from INTEGER CHECK (window_from IS NULL OR window_from >= 0);
