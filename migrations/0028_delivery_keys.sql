-- Leases carry a token, so a worker whose lease ran out (and whose job or message another worker
-- reclaimed) cannot delete or reschedule it when it finally finishes.
ALTER TABLE outbox ADD COLUMN lease UUID;
ALTER TABLE mailqueue ADD COLUMN lease UUID;

-- Alerts, private messages and emails an outbox job already delivered, keyed by job and
-- recipient. A retried job skips them, even if the alert, message or email has since been
-- read, deleted or sent. Pruned after 30 days, when dead jobs are.
CREATE TABLE deliveries (
    key        TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX deliveries_created ON deliveries (created_at);
