//! Privacy controls: retention limits, IP address shortening, anonymizing what deleted members
//! leave behind, and the plain-language retention summary used by the privacy page.

/// The retention settings that matter for the summary (0 days = kept indefinitely).
#[derive(Debug, Clone)]
pub struct Retention {
    pub ip_days: i64,
    pub audit_days: i64,
    pub maillog_days: i64,
    pub anonymize_deleted: bool,
    pub deleted_name: String,
}

impl Retention {
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        Retention {
            ip_days: s.int("privacy_ip_days").max(0),
            audit_days: s.int("privacy_audit_days").max(0),
            maillog_days: s.int("privacy_maillog_days").max(0),
            anonymize_deleted: s.bool("privacy_anonymize_deleted"),
            deleted_name: s.get("privacy_deleted_name").trim().to_string(),
        }
    }
}

/// Where IP addresses are stored, with the time column that dates each row.
const IP_COLUMNS: &[(&str, &str, &str)] = &[
    ("posts", "dateline", "ipaddress"),
    ("privatemessages", "dateline", "ipaddress"),
    ("user_audit", "dateline", "ipaddress"),
    ("moderatorlog", "dateline", "ipaddress"),
    ("adminlog", "dateline", "ipaddress"),
    ("pollvotes", "dateline", "ipaddress"),
    ("maillogs", "dateline", "ipaddress"),
    ("spamlog", "dateline", "ipaddress"),
    ("searchlog", "dateline", "ipaddress"),
    ("system_authorship", "dateline", "ipaddress"),
    ("users", "regdate", "regip"),
    ("users", "lastactive", "lastip"),
];
const BATCH: i64 = 5000;

/// Shorten IP addresses on rows dated before `cut`, in batches so no table is locked for long.
/// Every old row is checked each run (rows can arrive with old dates, e.g. from an import or a
/// restore). An IPv4 address ending in ".0" is already its own /24, so a cheap text test skips it
/// before the shortening function runs.
async fn shorten_ips(db: &sqlx::PgPool, cut: i64) -> anyhow::Result<u64> {
    let mut total = 0;
    for (table, time, col) in IP_COLUMNS {
        let sql = format!(
            "UPDATE {table} SET {col} = rbb_anon_ip({col}) WHERE ctid IN (
                SELECT ctid FROM {table} WHERE {time} < $1 AND {col} <> '' AND {col} NOT LIKE '%.0' AND {col} <> rbb_anon_ip({col}) LIMIT $2)"
        );
        loop {
            let n = sqlx::query(&sql).bind(cut).bind(BATCH).execute(db).await?.rows_affected();
            total += n;
            if n < BATCH as u64 {
                break;
            }
        }
    }
    Ok(total)
}

/// Scheduled task: shorten IP addresses past the limit and prune logs past their retention.
pub async fn run(app: &crate::app::App) -> anyhow::Result<String> {
    let s = app.cache().settings.clone();
    let db = &app.db;
    let t = crate::util::now();
    let day = 86_400;
    let mut done = vec![];
    let ip_days = s.int("privacy_ip_days");
    if ip_days > 0 {
        let n = shorten_ips(db, t - ip_days * day).await?;
        done.push(format!("shortened {n} IP addresses"));
    }
    for (setting, table) in [("privacy_audit_days", "user_audit"), ("privacy_spamlog_days", "spamlog"), ("privacy_maillog_days", "maillogs")] {
        let days = s.int(setting);
        if days > 0 {
            let n = sqlx::query(&format!("DELETE FROM {table} WHERE dateline < $1")).bind(t - days * day).execute(db).await?.rows_affected();
            done.push(format!("removed {n} {table} rows"));
        }
    }
    Ok(if done.is_empty() { "nothing to do".into() } else { done.join(", ") })
}

/// Remove a member's name and IP addresses from what they leave behind. Runs inside the account
/// deletion transaction, before the users row goes.
pub async fn anonymize_member(tx: &mut sqlx::PgConnection, uid: i32, name: &str) -> crate::error::AppResult<()> {
    for sql in [
        "UPDATE posts SET username = $2, ipaddress = '' WHERE uid = $1",
        "UPDATE threads SET username = $2 WHERE uid = $1",
        "UPDATE threads SET lastposter = $2 WHERE lastposteruid = $1",
        "UPDATE forums SET lastposter = $2 WHERE lastposteruid = $1",
    ] {
        sqlx::query(sql).bind(uid).bind(name).execute(&mut *tx).await?;
    }
    for sql in [
        "UPDATE pollvotes SET ipaddress = '' WHERE uid = $1",
        "UPDATE threadratings SET ipaddress = '' WHERE uid = $1",
        "UPDATE privatemessages SET ipaddress = '' WHERE fromid = $1",
        "UPDATE moderatorlog SET ipaddress = '' WHERE uid = $1",
        "UPDATE adminlog SET ipaddress = '' WHERE uid = $1",
        "UPDATE system_authorship SET ipaddress = '' WHERE actor = $1",
        "DELETE FROM maillogs WHERE fromuid = $1 OR touid = $1",
        "DELETE FROM searchlog WHERE uid = $1",
    ] {
        sqlx::query(sql).bind(uid).execute(&mut *tx).await?;
    }
    Ok(())
}

/// Plain sentences describing the current retention settings, for `{retention}` in the privacy
/// policy.
pub fn retention_summary(r: &Retention) -> String {
    let days = |n: i64| format!("{n} day{}", if n == 1 { "" } else { "s" });
    let mut out = vec![];
    out.push(if r.ip_days > 0 {
        format!("IP addresses are kept for {}, for moderation and abuse prevention, then shortened to their network so they no longer identify you.", days(r.ip_days))
    } else {
        "IP addresses are kept with posts and account activity for moderation and abuse prevention.".to_string()
    });
    out.push(if r.audit_days > 0 {
        format!("Your account activity log (sign-ins and security changes) is kept for {}.", days(r.audit_days))
    } else {
        "Your account activity log (sign-ins and security changes) is kept until you delete your account.".to_string()
    });
    if r.maillog_days > 0 {
        out.push(format!("Copies of emails sent through the board are deleted after {}.", days(r.maillog_days)));
    }
    out.push(if r.anonymize_deleted {
        format!("If you delete your account and keep your posts, they are shown as “{}” and their IP addresses are removed.", r.deleted_name)
    } else {
        "If you delete your account, you can choose whether your posts are deleted too.".to_string()
    });
    out.push("The board uses cookies only to keep you signed in, remember your preferences and protect forms.".to_string());
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Retention {
        Retention { ip_days: 0, audit_days: 365, maillog_days: 0, anonymize_deleted: false, deleted_name: "Former member".into() }
    }

    #[test]
    fn describes_ip_shortening() {
        let s = retention_summary(&Retention { ip_days: 30, ..defaults() });
        assert!(s.contains("IP addresses") && s.contains("30 days") && s.contains("shortened"), "{s}");
        let kept = retention_summary(&defaults());
        assert!(kept.contains("IP addresses") && !kept.contains("shortened"), "{kept}");
    }

    #[test]
    fn describes_activity_log_retention() {
        assert!(retention_summary(&defaults()).contains("365 days"));
        assert!(retention_summary(&Retention { audit_days: 0, ..defaults() }).contains("account activity log"));
    }

    #[test]
    fn describes_mail_log_only_when_limited() {
        assert!(retention_summary(&Retention { maillog_days: 60, ..defaults() }).contains("60 days"));
    }

    #[test]
    fn describes_account_deletion() {
        let on = retention_summary(&Retention { anonymize_deleted: true, ..defaults() });
        assert!(on.contains("“Former member”"), "{on}");
        let off = retention_summary(&defaults());
        assert!(off.contains("delete your account") && !off.contains("Former member"), "{off}");
    }

    #[test]
    fn singular_day() {
        assert!(retention_summary(&Retention { ip_days: 1, ..defaults() }).contains("1 day,") || retention_summary(&Retention { ip_days: 1, ..defaults() }).contains("1 day "));
    }

    #[test]
    fn mentions_cookies() {
        assert!(retention_summary(&defaults()).contains("cookies"));
    }
}
