ALTER TABLE hub_work_acks ADD COLUMN subscriber_session TEXT NOT NULL DEFAULT '';
DROP INDEX hub_work_ack_subscriber_scope;
CREATE UNIQUE INDEX hub_work_ack_subscriber_scope ON hub_work_acks
    (subscriber_token, subscriber_session, provider, host, repository,
     COALESCE(repository_id, -1))
    WHERE subscriber_token IS NOT NULL;

ALTER TABLE hub_work_deliveries ADD COLUMN subscriber_session TEXT NOT NULL DEFAULT '';
DROP INDEX hub_work_delivery_subscriber_scope;
CREATE INDEX hub_work_delivery_subscriber_scope ON hub_work_deliveries
    (subscriber_token, subscriber_session, provider, host, repository, repository_id, created_at)
    WHERE subscriber_token IS NOT NULL;
