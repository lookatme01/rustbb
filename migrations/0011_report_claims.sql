-- Report claiming and resolution: who is handling a report, how it was resolved, and its history.
ALTER TABLE reportedcontent
    ADD COLUMN claimed_by  INT NOT NULL DEFAULT 0,
    ADD COLUMN claimed_at  BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN resolved_by INT NOT NULL DEFAULT 0,
    ADD COLUMN resolved_at BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN resolution  TEXT NOT NULL DEFAULT '';

CREATE TABLE report_events (
    id       BIGSERIAL PRIMARY KEY,
    rid      INT NOT NULL REFERENCES reportedcontent(rid) ON DELETE CASCADE,
    uid      INT NOT NULL,
    action   TEXT NOT NULL,          -- claimed | released | taken_over | resolved | reopened
    note     TEXT NOT NULL DEFAULT '',
    dateline BIGINT NOT NULL
);
CREATE INDEX report_events_rid ON report_events (rid, id);
