-- Link recipient copies to the exact sent copy, not the second in which they were sent.
ALTER TABLE privatemessages ADD COLUMN sent_pmid INT
    REFERENCES privatemessages(pmid) ON DELETE SET NULL;
CREATE INDEX privatemessages_sent_copy ON privatemessages (sent_pmid)
    WHERE sent_pmid IS NOT NULL;

-- Recover historical links only where a single sent copy matches. Ambiguous old messages
-- remain unlinked so a cancellation can never remove a different delivery.
WITH matches AS (
    SELECT r.pmid, MIN(s.pmid) AS sent_pmid
    FROM privatemessages r JOIN privatemessages s
      ON s.folder = 2 AND s.uid = r.fromid AND s.fromid = r.fromid
     AND s.dateline = r.dateline AND s.subject = r.subject AND s.message = r.message
     AND (s.recipients->'to' @> jsonb_build_array(r.uid)
          OR s.recipients->'bcc' @> jsonb_build_array(r.uid)
          OR s.toid = r.uid)
    WHERE r.folder NOT IN (2, 3) AND r.pmid <> s.pmid
    GROUP BY r.pmid HAVING COUNT(*) = 1
)
UPDATE privatemessages p SET sent_pmid = m.sent_pmid FROM matches m WHERE p.pmid = m.pmid;
