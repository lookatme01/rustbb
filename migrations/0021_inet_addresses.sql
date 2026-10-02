-- IP addresses become native inet values: validated on write, compact, and searchable by
-- network (`ip <<= '10.0.0.0/8'`). "No address" (content published as System, imported rows
-- without one) is NULL instead of an empty string. Values that are not valid addresses become
-- NULL.
CREATE FUNCTION pg_temp.to_inet(v text) RETURNS inet LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    IF v IS NULL OR btrim(v) = '' THEN
        RETURN NULL;
    END IF;
    RETURN btrim(v)::inet;
EXCEPTION WHEN others THEN
    RETURN NULL;
END $$;

DO $$
DECLARE
    c record;
BEGIN
    FOR c IN
        SELECT * FROM (VALUES
            ('adminlog', 'ipaddress'), ('logins', 'ip'), ('maillogs', 'ipaddress'),
            ('moderatorlog', 'ipaddress'), ('pollvotes', 'ipaddress'), ('posts', 'ipaddress'),
            ('privatemessages', 'ipaddress'), ('searchlog', 'ipaddress'), ('sessions', 'ip'),
            ('spamlog', 'ipaddress'), ('system_authorship', 'ipaddress'),
            ('threadratings', 'ipaddress'), ('user_audit', 'ipaddress'), ('users', 'lastip'),
            ('users', 'regip')
        ) AS t(tab, col)
    LOOP
        EXECUTE format('ALTER TABLE %I ALTER COLUMN %I DROP DEFAULT, ALTER COLUMN %I DROP NOT NULL', c.tab, c.col, c.col);
        EXECUTE format('ALTER TABLE %I ALTER COLUMN %I TYPE inet USING pg_temp.to_inet(%I)', c.tab, c.col, c.col);
    END LOOP;
END $$;

-- Privacy shortening for inet values: the /24 (IPv4) or /48 (IPv6) network, kept as a single
-- address so it reads like one (203.0.113.0). The text version from 0013 stays for callers that
-- pass text.
CREATE FUNCTION rbb_anon_ip(ip inet) RETURNS inet LANGUAGE sql IMMUTABLE AS $$
    SELECT set_masklen(network(set_masklen(ip, CASE WHEN family(ip) = 4 THEN 24 ELSE 48 END)),
                       CASE WHEN family(ip) = 4 THEN 32 ELSE 128 END)
$$;
