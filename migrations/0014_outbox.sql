-- Transactional outbox: background work recorded in the same transaction as the change that
-- caused it, so it happens exactly when the change commits (and never when it rolls back).
-- Workers lease rows with FOR UPDATE SKIP LOCKED; a lease that runs out (crashed worker) makes
-- the row claimable again. Finished jobs are deleted; jobs that keep failing become 'dead'.
CREATE TABLE outbox (
    id              BIGSERIAL PRIMARY KEY,
    kind            TEXT NOT NULL,
    payload         JSONB NOT NULL DEFAULT '{}',
    idempotency_key TEXT,
    status          TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'dead')),
    attempts        INT NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    available_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until    TIMESTAMPTZ,
    last_error      TEXT
);
CREATE INDEX outbox_due ON outbox (available_at, id) WHERE status = 'pending';
CREATE UNIQUE INDEX outbox_idempotency ON outbox (idempotency_key) WHERE idempotency_key IS NOT NULL;

-- Cluster change log: cache invalidations every node must apply. NOTIFY on `rbb_cluster` only
-- wakes nodes up; they read the rows, so a notification lost while a node was disconnected or
-- busy is picked up on its next poll instead of leaving stale caches behind.
CREATE TABLE cluster_events (
    id         BIGSERIAL PRIMARY KEY,
    origin     TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('cache', 'pagetags', 'pagecache')),
    payload    JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX cluster_events_created ON cluster_events (created_at);

-- Buffered counter batches already applied (thread views), so a retried flush whose first
-- attempt did commit is not counted twice. Pruned after a day.
CREATE TABLE applied_batches (
    id         UUID PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
