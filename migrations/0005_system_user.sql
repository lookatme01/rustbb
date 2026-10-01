-- "System": a built-in bot account that owns automated work (reports, tasks, and later autonomous
-- moderation). The rows are created by `system::ensure` at startup; this migration only adds the
-- markers and the guards that keep the account intact whatever code path touches it.

ALTER TABLE users ADD COLUMN is_system BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE usergroups ADD COLUMN is_system BOOLEAN NOT NULL DEFAULT FALSE;
CREATE UNIQUE INDEX users_one_system ON users ((is_system)) WHERE is_system;
CREATE UNIQUE INDEX usergroups_one_system ON usergroups ((is_system)) WHERE is_system;

-- Errors raised here use SQLSTATE RBSYS; the app turns them into a user-facing message.
CREATE FUNCTION rbb_system_user_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'The System account cannot be deleted.' USING ERRCODE = 'RBSYS';
    END IF;
    IF NOT OLD.is_system THEN
        RAISE EXCEPTION 'Only the built-in System account can be a system account.' USING ERRCODE = 'RBSYS';
    END IF;
    IF NEW.is_system IS DISTINCT FROM OLD.is_system
        OR NEW.password IS DISTINCT FROM OLD.password
        OR NEW.email IS DISTINCT FROM OLD.email
        OR NEW.usergroup IS DISTINCT FROM OLD.usergroup
        OR NEW.additionalgroups IS DISTINCT FROM OLD.additionalgroups THEN
        RAISE EXCEPTION 'The System account''s password, email and groups cannot be changed.' USING ERRCODE = 'RBSYS';
    END IF;
    RETURN NEW;
END $$;

-- WHEN clauses keep the hot path (batched lastactive updates etc.) free of any plpgsql call.
CREATE TRIGGER users_system_update BEFORE UPDATE ON users
    FOR EACH ROW WHEN (OLD.is_system OR NEW.is_system) EXECUTE FUNCTION rbb_system_user_guard();
CREATE TRIGGER users_system_delete BEFORE DELETE ON users
    FOR EACH ROW WHEN (OLD.is_system) EXECUTE FUNCTION rbb_system_user_guard();

CREATE FUNCTION rbb_system_group_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'The System group cannot be deleted.' USING ERRCODE = 'RBSYS';
    END IF;
    IF NEW.is_system IS DISTINCT FROM OLD.is_system THEN
        RAISE EXCEPTION 'The System group marker cannot be changed.' USING ERRCODE = 'RBSYS';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER usergroups_system_update BEFORE UPDATE ON usergroups
    FOR EACH ROW WHEN (OLD.is_system OR NEW.is_system) EXECUTE FUNCTION rbb_system_group_guard();
CREATE TRIGGER usergroups_system_delete BEFORE DELETE ON usergroups
    FOR EACH ROW WHEN (OLD.is_system) EXECUTE FUNCTION rbb_system_group_guard();

-- System never signs in (no login tokens, hence no member sessions) and is never banned.
CREATE FUNCTION rbb_system_uid_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM users WHERE uid = NEW.uid AND is_system) THEN
        RAISE EXCEPTION '%', CASE TG_TABLE_NAME
            WHEN 'banned' THEN 'The System account cannot be banned.'
            ELSE 'The System account cannot sign in.' END
            USING ERRCODE = 'RBSYS';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER banned_system BEFORE INSERT OR UPDATE ON banned
    FOR EACH ROW EXECUTE FUNCTION rbb_system_uid_guard();
CREATE TRIGGER logins_system BEFORE INSERT OR UPDATE OF uid ON logins
    FOR EACH ROW EXECUTE FUNCTION rbb_system_uid_guard();
