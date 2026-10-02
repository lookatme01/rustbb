-- Activation and email-change codes were stored as given; store their SHA-256 like password
-- reset codes, so a database leak does not hand out working links.
UPDATE awaitingactivation
   SET code = encode(sha256(convert_to(code, 'UTF8')), 'hex')
 WHERE type IN ('r', 'e', 'b') AND code !~ '^[0-9a-f]{64}$';

-- One pending code of each kind per member (the newest wins).
DELETE FROM awaitingactivation a
 USING awaitingactivation b
 WHERE a.uid = b.uid AND a.type = b.type AND a.aid < b.aid;
DROP INDEX IF EXISTS awaitingactivation_uid;
CREATE UNIQUE INDEX awaitingactivation_uid_type ON awaitingactivation (uid, type);

ALTER TABLE awaitingactivation
    ADD CONSTRAINT awaitingactivation_type_check CHECK (type IN ('r', 'p', 'e', 'b'));
