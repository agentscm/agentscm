-- Agent conversations are shared by default on a Hub with no one in it yet. A Hub that already
-- has people was running with the default off unless a Hub admin had set it, so record that
-- choice: an upgrade must not start sharing what people had not agreed to share.
INSERT INTO hub_settings (key, value, updated_at, updated_by)
SELECT 'conversation_sharing:default', 'off', CAST(strftime('%s', 'now') AS INTEGER), 'migration'
 WHERE EXISTS (SELECT 1 FROM hub_users)
   AND NOT EXISTS (SELECT 1 FROM hub_settings WHERE key = 'conversation_sharing:default');
