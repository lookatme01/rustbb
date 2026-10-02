-- Privacy controls: IP shortening and a record of erasure requests (without personal data).

-- An IP address shortened to its network (IPv4 /24, IPv6 /48), so it no longer identifies a
-- person but still helps moderation. Empty or unparseable values become ''.
CREATE FUNCTION rbb_anon_ip(ip text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    IF ip IS NULL OR btrim(ip) = '' THEN
        RETURN '';
    END IF;
    RETURN host(network(set_masklen(ip::inet, CASE WHEN family(ip::inet) = 4 THEN 24 ELSE 48 END)));
EXCEPTION WHEN others THEN
    RETURN '';
END $$;

-- That an erasure happened, by whom and why. Deliberately no username, email or IP.
CREATE TABLE erasure_log (
    id           SERIAL PRIMARY KEY,
    former_uid   INT NOT NULL,
    performed_by INT NOT NULL,
    dateline     BIGINT NOT NULL,
    kept_posts   BOOLEAN NOT NULL,
    reference    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX erasure_log_recent ON erasure_log (dateline DESC);
