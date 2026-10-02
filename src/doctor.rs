//! `rbb doctor`: preflight checks for configuration, database, board and plugins. Read-only.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub group: &'static str,
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

/// One row of `_sqlx_migrations`.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: i64,
    pub checksum: Vec<u8>,
    pub success: bool,
}

fn check(
    group: &'static str,
    name: &str,
    status: Status,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> Check {
    Check {
        group,
        name: name.into(),
        status,
        detail: detail.into(),
        fix: fix.map(str::to_string),
    }
}

/// A plain-language fix for a known database error message.
pub fn db_error_hint(msg: &str) -> Option<&'static str> {
    let m = msg.to_ascii_lowercase();
    Some(
        if m.contains("ident authentication failed") || m.contains("peer authentication failed") {
            "PostgreSQL is checking your operating-system user instead of the password (`ident`/`peer` in pg_hba.conf). \
         Change the matching `host`/`local` lines in pg_hba.conf to `scram-sha-256`, then reload PostgreSQL."
        } else if m.contains("password authentication failed") {
            "The database password in DATABASE_URL is wrong, or the role doesn't exist. Check the URL, or reset it with \
         `ALTER ROLE <name> PASSWORD '...'`."
        } else if m.contains("connection refused") {
            "Nothing is listening at the host and port in DATABASE_URL. Check that PostgreSQL is running, `listen_addresses` \
         includes this address, and no firewall is in the way."
        } else if m.contains("does not exist") && m.contains("database") {
            "The database in DATABASE_URL doesn't exist. Create it with `createdb -O <role> <name>`."
        } else if m.contains("pg_trgm") && m.contains("not available") {
            "The pg_trgm extension isn't installed on the database server. Install your distribution's PostgreSQL contrib \
         package (for example `postgresql17-contrib` or `postgresql-contrib`)."
        } else if m.contains("permission denied for schema")
            || m.contains("permission denied for database")
        {
            "The database role can't create tables. Grant it with `GRANT CREATE ON SCHEMA public TO <role>` \
         (or make the role own the database)."
        } else {
            return None;
        },
    )
}

/// Attach the plain-language fix to a database error from `serve`/`migrate`, when one is known.
pub fn hinted(e: anyhow::Error) -> anyhow::Error {
    let Some(fix) = db_error_hint(&format!("{e:#}")) else {
        return e;
    };
    // The error chain, skipping causes a message already includes (sqlx repeats its cause).
    let mut msg = e.to_string();
    for cause in e.chain().skip(1) {
        let c = cause.to_string();
        if !msg.contains(&c) {
            msg = format!("{msg}: {c}");
        }
    }
    anyhow::anyhow!("{msg}\n  How to fix: {fix}\n  Run `rbb doctor` to check the whole setup.")
}

pub fn check_secret(secret: Option<&str>) -> Check {
    const NAME: &str = "RBB_SECRET";
    const FIX: &str = "Set RBB_SECRET to a long random value, e.g. `openssl rand -hex 32`, identical on every node.";
    let s = secret.unwrap_or("");
    if s.is_empty() {
        return check("Configuration", NAME, Status::Fail, "not set", Some(FIX));
    }
    if s.len() < 32 {
        return check(
            "Configuration",
            NAME,
            Status::Fail,
            format!("only {} characters (32 or more required)", s.len()),
            Some(FIX),
        );
    }
    if crate::config::KNOWN_SECRETS.contains(&s) {
        return check(
            "Configuration",
            NAME,
            Status::Fail,
            "still the example value from the docs",
            Some(FIX),
        );
    }
    let distinct = s.chars().collect::<std::collections::HashSet<_>>().len();
    if distinct < 8 {
        return check(
            "Configuration",
            NAME,
            Status::Warn,
            format!("long enough but uses only {distinct} different characters"),
            Some(FIX),
        );
    }
    check(
        "Configuration",
        NAME,
        Status::Ok,
        format!("set ({} characters)", s.len()),
        None,
    )
}

/// Warnings about cookie and proxy settings for the board URL (empty when consistent).
pub fn check_proxy_cookies(bburl: &str, secure_cookies: bool, trust_proxy: bool) -> Vec<Check> {
    let mut out = vec![];
    if bburl.starts_with("https://") {
        if !secure_cookies {
            out.push(check(
                "Configuration",
                "RBB_SECURE_COOKIES",
                Status::Warn,
                "the board URL uses https but cookies aren't marked Secure (and no HSTS)",
                Some("Set RBB_SECURE_COOKIES=true."),
            ));
        }
        if !trust_proxy {
            out.push(check("Configuration", "RBB_TRUST_PROXY", Status::Warn, "the board URL uses https, so rbb is probably behind a TLS proxy, but client IPs are not read from X-Forwarded-For",
                Some("Set RBB_TRUST_PROXY=true if a reverse proxy or load balancer terminates TLS; otherwise every visitor appears to come from the proxy.")));
        }
    }
    out
}

/// `server_version_num`, e.g. 170004 for 17.4.
pub fn check_server_version(num: i32) -> Check {
    let (major, minor) = (num / 10000, num % 10000);
    let v = format!("PostgreSQL {major}.{minor}");
    match major {
        m if m < 12 => check(
            "Database",
            "PostgreSQL version",
            Status::Fail,
            format!("{v}; 12 or newer is required"),
            Some("Upgrade PostgreSQL (17 recommended)."),
        ),
        m if m < 17 => check(
            "Database",
            "PostgreSQL version",
            Status::Warn,
            format!("{v}; supported but untested (rbb is tested on 17)"),
            None,
        ),
        _ => check("Database", "PostgreSQL version", Status::Ok, v, None),
    }
}

/// Compare the migrations built into this binary with the ones recorded in the database.
pub fn compare_migrations(embedded: &[(i64, Vec<u8>)], applied: &[AppliedMigration]) -> Vec<Check> {
    const NAME: &str = "Migrations";
    let mut out = vec![];
    for a in applied {
        match embedded.iter().find(|(v, _)| *v == a.version) {
            None => out.push(check("Database", NAME, Status::Fail,
                format!("migration {} was applied by a newer rbb; this binary is older than the database", a.version),
                Some("Run the rbb version that matches the database (downgrades aren't supported)."))),
            Some(_) if !a.success => out.push(check("Database", NAME, Status::Fail,
                format!("migration {} failed part-way", a.version),
                Some("Fix the cause shown when it ran, then remove its row from _sqlx_migrations and run `rbb migrate`."))),
            Some((_, sum)) if *sum != a.checksum => out.push(check("Database", NAME, Status::Fail,
                format!("migration {} has changed since it was applied", a.version),
                Some("Applied migrations must never be edited; restore the original file."))),
            Some(_) => {}
        }
    }
    let pending = embedded
        .iter()
        .filter(|(v, _)| !applied.iter().any(|a| a.version == *v))
        .count();
    if pending > 0 {
        out.push(check(
            "Database",
            NAME,
            Status::Warn,
            format!("{pending} pending"),
            Some("They're applied automatically by `rbb serve`, or run `rbb migrate`."),
        ));
    }
    if out.is_empty() {
        out.push(check(
            "Database",
            NAME,
            Status::Ok,
            format!("all {} applied", embedded.len()),
            None,
        ));
    }
    out
}

/// The node's pool size against the server's connection limit.
pub fn check_pool(pool: u32, max_connections: i32, reserved: i32) -> Check {
    const NAME: &str = "Connection limit";
    let usable = (max_connections - reserved).max(0) as u32;
    let nodes = usable.checked_div(pool).unwrap_or(0);
    let detail =
        format!("RBB_DB_MAX_CONNECTIONS={pool}, server allows {usable}: room for {nodes} nodes");
    if pool > usable {
        check(
            "Database",
            NAME,
            Status::Fail,
            detail,
            Some("Lower RBB_DB_MAX_CONNECTIONS or raise max_connections (or add PgBouncer)."),
        )
    } else if nodes < 2 {
        check(
            "Database",
            NAME,
            Status::Warn,
            detail,
            Some(
                "Every app node opens its own pool; leave room for a second node, migrations and admin tools.",
            ),
        )
    } else {
        check("Database", NAME, Status::Ok, detail, None)
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// DATABASE_URL without its password, for display.
fn redacted(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("***"));
            }
            u.to_string()
        }
        Err(_) => "(not a valid URL)".into(),
    }
}

fn config_checks(out: &mut Vec<Check>) {
    use crate::config::*;
    out.push(check_secret(env("RBB_SECRET").as_deref()));

    let listen = env("RBB_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.into());
    out.push(match listen.parse::<std::net::SocketAddr>() {
        Err(e) => check(
            "Configuration",
            "RBB_LISTEN",
            Status::Fail,
            format!("“{listen}” is not an address: {e}"),
            Some("Use host:port, e.g. 127.0.0.1:8080 or 0.0.0.0:8080."),
        ),
        Ok(addr) => match std::net::TcpListener::bind(addr) {
            Ok(_) => check(
                "Configuration",
                "RBB_LISTEN",
                Status::Ok,
                format!("{addr} is free"),
                None,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => check(
                "Configuration",
                "RBB_LISTEN",
                Status::Warn,
                format!("{addr} is already in use (is rbb already running?)"),
                Some("Stop the other process or choose another port."),
            ),
            Err(e) => check(
                "Configuration",
                "RBB_LISTEN",
                Status::Fail,
                format!("can't listen on {addr}: {e}"),
                Some("Ports below 1024 need extra privileges; use 8080 behind a proxy."),
            ),
        },
    });

    let dir = env("RBB_UPLOAD_DIR").unwrap_or_else(|| DEFAULT_UPLOAD_DIR.into());
    let path = std::path::Path::new(&dir);
    out.push(if !path.is_dir() {
        check("Configuration", "RBB_UPLOAD_DIR", Status::Fail, format!("{dir} doesn't exist"),
            Some("Create it and make it writable by the user rbb runs as. With several nodes, it must be the same shared folder on each."))
    } else {
        let probe = path.join(format!(".rbb-doctor-{}", crate::util::random_token(8)));
        match std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe)) {
            Ok(()) => check("Configuration", "RBB_UPLOAD_DIR", Status::Ok, format!("{dir} is writable"), None),
            Err(e) => check("Configuration", "RBB_UPLOAD_DIR", Status::Fail, format!("{dir} isn't writable: {e}"), Some("Give the user rbb runs as write access to it.")),
        }
    });

    for (key, default) in [
        ("RBB_DB_MAX_CONNECTIONS", DEFAULT_DB_MAX_CONNECTIONS),
        ("RBB_PAGE_CACHE_MB", DEFAULT_PAGE_CACHE_MB),
    ] {
        let v = env(key).unwrap_or_else(|| default.into());
        if v.parse::<u64>().is_err() {
            out.push(check(
                "Configuration",
                key,
                Status::Fail,
                format!("“{v}” is not a number"),
                None,
            ));
        }
    }
    if let Some(dir) = env("RBB_DEV_TEMPLATES") {
        out.push(check("Configuration", "RBB_DEV_TEMPLATES", Status::Warn, format!("templates are read from {dir} on every request"),
            Some("Development only: unset it in production (it also turns off the guest page cache).")));
    }

    let pdir = env("RBB_PLUGINS_DIR").unwrap_or_else(|| DEFAULT_PLUGINS_DIR.into());
    let plugins = crate::plugins::Plugins::check_dir(&pdir);
    let broken: Vec<_> = plugins.iter().filter(|(_, e)| e.is_some()).collect();
    for (file, err) in &broken {
        out.push(check(
            "Plugins",
            "Plugin",
            Status::Warn,
            format!("{file} doesn't compile: {}", err.as_deref().unwrap_or("")),
            Some("Fix the script or rename it (e.g. add .disabled); rbb skips it until then."),
        ));
    }
    if broken.is_empty() {
        out.push(check(
            "Plugins",
            "Plugins",
            Status::Ok,
            format!("{} loaded from {pdir}", plugins.len()),
            None,
        ));
    }
}

async fn database_checks(out: &mut Vec<Check>) {
    let url = env("DATABASE_URL").unwrap_or_else(|| crate::config::DEFAULT_DATABASE_URL.into());
    let connect = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url);
    let db = match tokio::time::timeout(std::time::Duration::from_secs(8), connect).await {
        Ok(Ok(db)) => {
            out.push(check(
                "Database",
                "Connection",
                Status::Ok,
                redacted(&url),
                None,
            ));
            db
        }
        Ok(Err(e)) => {
            let msg = format!("{e}");
            out.push(check(
                "Database",
                "Connection",
                Status::Fail,
                format!("{}: {msg}", redacted(&url)),
                db_error_hint(&msg),
            ));
            return;
        }
        Err(_) => {
            out.push(check("Database", "Connection", Status::Fail, format!("{}: no answer within 8 seconds", redacted(&url)),
                Some("Check the host, port and any firewall or security group between rbb and PostgreSQL.")));
            return;
        }
    };

    if let Ok(num) = sqlx::query_scalar::<_, String>("SHOW server_version_num")
        .fetch_one(&db)
        .await
    {
        out.push(check_server_version(num.parse().unwrap_or(0)));
    }
    let (available, installed): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_trgm'),
                EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
    )
    .fetch_one(&db)
    .await
    .unwrap_or((false, false));
    out.push(match (available, installed) {
        (_, true) => check(
            "Database",
            "Extension pg_trgm",
            Status::Ok,
            "installed",
            None,
        ),
        (true, false) => check(
            "Database",
            "Extension pg_trgm",
            Status::Ok,
            "available (the first migration installs it)",
            None,
        ),
        (false, false) => check(
            "Database",
            "Extension pg_trgm",
            Status::Fail,
            "not available on the server",
            db_error_hint("extension \"pg_trgm\" is not available"),
        ),
    });
    let can_create: bool = sqlx::query_scalar("SELECT has_schema_privilege('public', 'CREATE')")
        .fetch_one(&db)
        .await
        .unwrap_or(false);
    out.push(if can_create {
        check(
            "Database",
            "Privileges",
            Status::Ok,
            "the role can create tables",
            None,
        )
    } else {
        check(
            "Database",
            "Privileges",
            Status::Fail,
            "the role can't create tables in schema public",
            db_error_hint("permission denied for schema public"),
        )
    });

    let embedded: Vec<(i64, Vec<u8>)> = sqlx::migrate!("./migrations")
        .iter()
        .map(|m| (m.version, m.checksum.to_vec()))
        .collect();
    let has_table: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(&db)
        .await
        .unwrap_or(false);
    let applied: Vec<AppliedMigration> = if has_table {
        sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
            "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(version, checksum, success)| AppliedMigration {
            version,
            checksum,
            success,
        })
        .collect()
    } else {
        vec![]
    };
    out.extend(compare_migrations(&embedded, &applied));

    if let Ok((max, reserved)) = sqlx::query_as::<_, (String, String)>(
        "SELECT current_setting('max_connections'), current_setting('superuser_reserved_connections')",
    )
    .fetch_one(&db)
    .await
    {
        let pool = env("RBB_DB_MAX_CONNECTIONS").and_then(|v| v.parse().ok()).unwrap_or(32);
        out.push(check_pool(pool, max.parse().unwrap_or(100), reserved.parse().unwrap_or(3)));
    }

    board_checks(out, &db).await;
}

async fn board_checks(out: &mut Vec<Check>, db: &sqlx::PgPool) {
    let installed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usergroups")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    if installed == 0 {
        out.push(check("Board", "Installation", Status::Warn, "no board data yet",
            Some("Run `rbb install …`, or start `rbb serve`, which installs itself on an empty database.")));
        return;
    }
    out.push(check(
        "Board",
        "Installation",
        Status::Ok,
        format!("{installed} user groups"),
        None,
    ));
    out.push(
        match sqlx::query_scalar::<_, String>("SELECT username FROM users WHERE is_system")
            .fetch_optional(db)
            .await
        {
            Ok(Some(name)) => check(
                "Board",
                "System account",
                Status::Ok,
                format!("present (“{name}”)"),
                None,
            ),
            _ => check(
                "Board",
                "System account",
                Status::Warn,
                "missing",
                Some("`rbb serve` creates it on start-up."),
            ),
        },
    );
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, value FROM settings")
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let s = crate::settings::Settings::from_rows(rows);
    let bburl = s.get("bburl").trim();
    if bburl.is_empty() {
        out.push(check(
            "Board",
            "Board URL",
            Status::Warn,
            "not set",
            Some("Admin CP → Settings → General → Board URL (used in emails and feeds)."),
        ));
    } else {
        out.push(check("Board", "Board URL", Status::Ok, bburl, None));
    }
    out.extend(check_proxy_cookies(
        bburl,
        env("RBB_SECURE_COOKIES").as_deref() == Some("true"),
        env("RBB_TRUST_PROXY").as_deref() == Some("true"),
    ));
    match s.get("mail_handler") {
        "smtp" if s.get("smtp_host").trim().is_empty() => out.push(check("Board", "Mail", Status::Fail, "SMTP is selected but no host is set",
            Some("Admin CP → Settings → Mail: set the SMTP host."))),
        "smtp" => out.push(check("Board", "Mail", Status::Ok, format!("SMTP via {}:{}", s.get("smtp_host"), s.get("smtp_port")), None)),
        _ => out.push(check("Board", "Mail", Status::Warn, "email is only written to the log: members won't receive activation or password-reset emails",
            Some("Admin CP → Settings → Mail: choose SMTP. Fine for development."))),
    }
}

/// Run every check, print the report, and return the process exit code.
pub async fn run(strict: bool) -> i32 {
    let mut checks = vec![];
    config_checks(&mut checks);
    database_checks(&mut checks).await;

    println!("rbb doctor ({})\n", env!("CARGO_PKG_VERSION"));
    let mut group = "";
    for c in &checks {
        if c.group != group {
            if !group.is_empty() {
                println!();
            }
            group = c.group;
            println!("{group}");
        }
        let mark = match c.status {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        };
        println!("  {mark} {}: {}", c.name, c.detail);
        if let (Some(fix), true) = (&c.fix, c.status != Status::Ok) {
            println!("      fix: {fix}");
        }
    }
    let count = |s: Status| checks.iter().filter(|c| c.status == s).count();
    let (warns, fails) = (count(Status::Warn), count(Status::Fail));
    println!(
        "\n{} ok, {warns} warning{}, {fails} failure{}",
        count(Status::Ok),
        if warns == 1 { "" } else { "s" },
        if fails == 1 { "" } else { "s" }
    );
    if fails > 0 || (strict && warns > 0) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_for_known_database_errors() {
        let ident = db_error_hint(
            "error returned from database: Ident authentication failed for user \"rbb\"",
        )
        .unwrap();
        assert!(
            ident.contains("pg_hba.conf") && ident.contains("scram-sha-256"),
            "{ident}"
        );
        assert!(
            db_error_hint("Peer authentication failed for user \"rbb\"")
                .unwrap()
                .contains("pg_hba.conf")
        );
        assert!(
            db_error_hint("password authentication failed for user \"rbb\"")
                .unwrap()
                .contains("password")
        );
        assert!(
            db_error_hint("error communicating with database: Connection refused (os error 61)")
                .unwrap()
                .contains("running")
        );
        assert!(
            db_error_hint("database \"rbbx\" does not exist")
                .unwrap()
                .contains("createdb")
        );
        let trgm =
            db_error_hint("while executing migration 1: extension \"pg_trgm\" is not available")
                .unwrap();
        assert!(trgm.contains("contrib"), "{trgm}");
        assert!(
            db_error_hint("permission denied for schema public")
                .unwrap()
                .contains("CREATE")
        );
        assert_eq!(db_error_hint("something nobody has seen before"), None);
    }

    #[test]
    fn known_database_errors_get_a_fix_attached() {
        let e = format!(
            "{:#}",
            hinted(anyhow::anyhow!(
                "Ident authentication failed for user \"rbb\""
            ))
        );
        assert!(
            e.contains("Ident authentication failed") && e.contains("pg_hba.conf"),
            "{e}"
        );
        assert_eq!(
            format!("{:#}", hinted(anyhow::anyhow!("disk full"))),
            "disk full"
        );
        let wrapped = anyhow::anyhow!("database \"x\" does not exist").context("connecting");
        let shown = format!("{:#}", hinted(wrapped));
        assert!(
            shown.starts_with("connecting: database \"x\" does not exist"),
            "keeps the cause: {shown}"
        );
        assert_eq!(shown.matches("does not exist").count(), 1, "{shown}");
    }

    #[test]
    fn secret_rules() {
        assert_eq!(check_secret(None).status, Status::Fail);
        assert_eq!(check_secret(Some("")).status, Status::Fail);
        assert_eq!(check_secret(Some("short")).status, Status::Fail);
        assert_eq!(
            check_secret(Some("change-me-to-a-long-random-string")).status,
            Status::Fail
        );
        assert_eq!(
            check_secret(Some(&"a".repeat(40))).status,
            Status::Warn,
            "long but trivially guessable"
        );
        let ok = check_secret(Some(
            "0cd49e2f52c5f60e0d1b60da6d4cbd5f61675a1b7343576d08861fd4f60fb753",
        ));
        assert_eq!(ok.status, Status::Ok);
        assert!(
            !ok.detail.contains("0cd49e2f"),
            "the secret itself is never printed"
        );
        assert!(
            check_secret(None)
                .fix
                .unwrap()
                .contains("openssl rand -hex 32")
        );
    }

    #[test]
    fn https_board_needs_secure_cookies() {
        let w = check_proxy_cookies("https://forum.example.com", false, true);
        assert!(
            w.iter()
                .any(|c| c.name.contains("RBB_SECURE_COOKIES") && c.status == Status::Warn)
        );
    }

    #[test]
    fn https_board_usually_sits_behind_a_proxy() {
        let w = check_proxy_cookies("https://forum.example.com", true, false);
        assert!(
            w.iter()
                .any(|c| c.name.contains("RBB_TRUST_PROXY") && c.status == Status::Warn)
        );
    }

    #[test]
    fn consistent_settings_have_no_warnings() {
        assert!(check_proxy_cookies("https://forum.example.com", true, true).is_empty());
        assert!(check_proxy_cookies("http://127.0.0.1:8080", false, false).is_empty());
    }

    #[test]
    fn server_versions() {
        assert_eq!(check_server_version(110022).status, Status::Fail);
        assert_eq!(check_server_version(140010).status, Status::Warn);
        assert_eq!(check_server_version(170004).status, Status::Ok);
        assert!(check_server_version(170004).detail.contains("17.4"));
        assert_eq!(check_server_version(180000).status, Status::Ok);
    }

    fn applied(version: i64, checksum: &[u8], success: bool) -> AppliedMigration {
        AppliedMigration {
            version,
            checksum: checksum.to_vec(),
            success,
        }
    }

    #[test]
    fn migrations_up_to_date() {
        let emb = vec![(1, vec![1]), (2, vec![2])];
        let c = compare_migrations(&emb, &[applied(1, &[1], true), applied(2, &[2], true)]);
        assert!(c.iter().all(|c| c.status == Status::Ok) && !c.is_empty());
    }

    #[test]
    fn pending_migrations_warn() {
        let emb = vec![(1, vec![1]), (2, vec![2]), (3, vec![3])];
        let c = compare_migrations(&emb, &[applied(1, &[1], true)]);
        let p = c
            .iter()
            .find(|c| c.status == Status::Warn)
            .expect("pending warning");
        assert!(p.detail.contains("2 pending"), "{}", p.detail);
    }

    #[test]
    fn migrations_from_a_newer_rbb_fail() {
        let c = compare_migrations(
            &[(1, vec![1])],
            &[applied(1, &[1], true), applied(9, &[9], true)],
        );
        assert!(
            c.iter()
                .any(|c| c.status == Status::Fail && c.detail.contains("9"))
        );
    }

    #[test]
    fn changed_migration_fails() {
        let c = compare_migrations(&[(1, vec![1])], &[applied(1, &[7], true)]);
        assert!(
            c.iter()
                .any(|c| c.status == Status::Fail && c.detail.contains("changed"))
        );
    }

    #[test]
    fn failed_migration_fails() {
        let c = compare_migrations(&[(1, vec![1])], &[applied(1, &[1], false)]);
        assert!(
            c.iter()
                .any(|c| c.status == Status::Fail && c.detail.contains("failed"))
        );
    }

    #[test]
    fn pool_against_server_limit() {
        assert_eq!(check_pool(32, 100, 3).status, Status::Ok);
        assert_eq!(
            check_pool(80, 100, 3).status,
            Status::Warn,
            "one node uses most of the server's connections"
        );
        assert_eq!(check_pool(120, 100, 3).status, Status::Fail);
        assert!(
            check_pool(32, 100, 3).detail.contains("3 nodes"),
            "says how many nodes fit"
        );
    }
}
