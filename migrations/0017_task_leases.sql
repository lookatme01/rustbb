-- Scheduled tasks are claimed with a lease instead of a session-level advisory lock (which
-- stayed held on a pooled connection if a task panicked). Claiming is one short transaction;
-- the lease keeps other nodes off the task while it runs and expires on its own if the node dies.
ALTER TABLE tasks
    ADD COLUMN locked_until TIMESTAMPTZ,
    ADD COLUMN locked_by    TEXT,
    ADD CONSTRAINT tasks_interval_check CHECK (interval_secs > 0);
