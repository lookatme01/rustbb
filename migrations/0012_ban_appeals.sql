-- Ban appeals: a banned member asks for a review; staff with ban rights accept or reject it.
CREATE TABLE ban_appeals (
    id           SERIAL PRIMARY KEY,
    uid          INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    ban_dateline BIGINT NOT NULL,             -- the ban appealed against (banned.dateline)
    ban_reason   TEXT NOT NULL DEFAULT '',
    banned_by    INT NOT NULL DEFAULT 0,
    statement    TEXT NOT NULL,
    status       SMALLINT NOT NULL DEFAULT 0, -- 0 pending, 1 accepted, 2 rejected
    created      BIGINT NOT NULL,
    decided_by   INT NOT NULL DEFAULT 0,
    decided_at   BIGINT NOT NULL DEFAULT 0,
    response     TEXT NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX ban_appeals_one_pending ON ban_appeals (uid) WHERE status = 0;
CREATE INDEX ban_appeals_status ON ban_appeals (status, created);
CREATE INDEX ban_appeals_uid ON ban_appeals (uid, id DESC);
