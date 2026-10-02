-- Mail delivery as a leased job queue. A worker claims due messages in one short statement
-- (setting locked_until), commits, sends them with no database locks held, then deletes them or
-- reschedules them with exponential backoff. A message whose worker died is reclaimed when its
-- lease runs out; one that keeps failing (or fails permanently) is 'dead' until an
-- administrator retries it. The idempotency key also becomes the Message-ID, so a message sent
-- twice (worker died after sending) can be recognized as a duplicate downstream.
ALTER TABLE mailqueue
    ADD COLUMN status          TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN available_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN locked_until    TIMESTAMPTZ,
    ADD COLUMN idempotency_key TEXT NOT NULL DEFAULT md5(random()::text || clock_timestamp()::text),
    ADD CONSTRAINT mailqueue_status_check CHECK (status IN ('pending', 'dead')),
    ADD CONSTRAINT mailqueue_attempts_check CHECK (attempts >= 0);
UPDATE mailqueue SET status = 'dead' WHERE attempts >= 5;
CREATE UNIQUE INDEX mailqueue_idempotency ON mailqueue (idempotency_key);
CREATE INDEX mailqueue_due ON mailqueue (available_at, mid) WHERE status = 'pending';
