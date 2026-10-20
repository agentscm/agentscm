ALTER TABLE hub_work_acks ADD COLUMN subscriber_token TEXT REFERENCES hub_agent_tokens (id);
DROP INDEX hub_work_ack_scope;
CREATE UNIQUE INDEX hub_work_ack_subscriber_scope ON hub_work_acks
    (subscriber_token, provider, host, repository, COALESCE(repository_id, -1))
    WHERE subscriber_token IS NOT NULL;

ALTER TABLE hub_work_deliveries ADD COLUMN subscriber_token TEXT REFERENCES hub_agent_tokens (id);
CREATE INDEX hub_work_delivery_subscriber_scope ON hub_work_deliveries
    (subscriber_token, provider, host, repository, repository_id, created_at)
    WHERE subscriber_token IS NOT NULL;
