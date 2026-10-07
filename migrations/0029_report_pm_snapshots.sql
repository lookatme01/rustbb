-- A frozen copy of a private message, taken when it is reported. Reports point at the
-- reporter's own copy, which they can delete (or take with their account); the snapshot keeps
-- the evidence for moderators and cannot change afterwards. Encrypted messages keep no body:
-- the server cannot read the ciphertext.
CREATE TABLE report_pm_snapshots (
    rid       INT PRIMARY KEY REFERENCES reportedcontent(rid) ON DELETE CASCADE,
    fromid    INT NOT NULL,
    subject   TEXT NOT NULL,
    message   TEXT NOT NULL,
    sent      BIGINT NOT NULL,
    smilieoff BOOLEAN NOT NULL DEFAULT FALSE,
    encrypted BOOLEAN NOT NULL DEFAULT FALSE
);
