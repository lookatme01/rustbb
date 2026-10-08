//! Deterministic, local moderation. Queue consumption, content changes, counters and audit
//! commit together. No deletion, bans, external services or retroactive scans.
use crate::{
    app::App,
    error::{AppError, AppResult},
    ops,
    util::now,
};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub mode: String,
    pub max_links: usize,
    pub new_user_posts: i32,
    pub duplicate_limit: i64,
    pub phrases: Vec<String>,
}
impl Policy {
    pub fn validate(&self) -> AppResult<()> {
        if !matches!(self.mode.as_str(), "off" | "observe" | "quarantine")
            || self.max_links > 100
            || !(0..=1000).contains(&self.new_user_posts)
            || !(2..=20).contains(&self.duplicate_limit)
            || self.phrases.len() > 100
            || self
                .phrases
                .iter()
                .any(|s| s.trim().len() < 3 || s.len() > 200)
        {
            return Err(AppError::user(
                "Invalid policy: use off/observe/quarantine; links 0–100; new-user posts 0–1000; duplicates 2–20; at most 100 phrases of 3–200 bytes.",
            ));
        }
        Ok(())
    }
    pub fn reasons(
        &self,
        subject: &str,
        message: &str,
        postnum: i32,
        duplicates: i64,
    ) -> Vec<String> {
        let text = format!("{subject}\n{message}").to_lowercase();
        let mut reasons = vec![];
        for phrase in &self.phrases {
            if text.contains(&phrase.trim().to_lowercase()) {
                reasons.push(format!("Blocked phrase: {}", phrase.trim()));
            }
        }
        // Link bursts and repetition apply only to members below the configured trust threshold.
        if postnum < self.new_user_posts {
            let links = LINK_RE.find_iter(&text).count();
            if self.max_links > 0 && links >= self.max_links {
                reasons.push(format!(
                    "Link burst: {links} links (threshold {})",
                    self.max_links
                ));
            }
            if duplicates >= self.duplicate_limit {
                reasons.push(format!(
                    "Repeated message: {duplicates} posts in 10 minutes"
                ));
            }
        }
        reasons
    }
}
static LINK_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)(?:\[url(?:=[^\]]*)?\][^\[]*\[/url\]|https?://|www\.)").unwrap()
});

async fn log(
    c: &mut PgConnection,
    uid: i32,
    fid: i32,
    tid: i32,
    pid: i32,
    action: &str,
    data: serde_json::Value,
) -> AppResult<()> {
    sqlx::query("INSERT INTO moderatorlog (uid, dateline, fid, tid, pid, action, data) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(uid).bind(now()).bind(fid).bind(tid).bind(pid).bind(action).bind(data).execute(c).await?;
    Ok(())
}

pub async fn run(app: &App) -> anyhow::Result<crate::tasks::Ran> {
    let pids: Vec<i32> =
        sqlx::query_scalar("SELECT pid FROM automod_queue ORDER BY queued_at, pid LIMIT 100")
            .fetch_all(&app.db)
            .await?;
    let mut processed = 0;
    let mut failed = 0;
    for pid in pids {
        match process(app, pid).await {
            Ok(()) => processed += 1,
            Err(e) => {
                failed += 1;
                tracing::error!(pid, "automod failed; item retained for retry: {e}");
            }
        }
    }
    Ok(crate::tasks::Ran::new(
        processed + failed > 0,
        format!("System evaluated {processed} queued posts; {failed} retained for retry"),
    ))
}

/// Evaluate one queued post now (the task runs this for each; tests call it directly).
pub async fn process(app: &App, pid: i32) -> AppResult<()> {
    let mut tx = app.db.begin().await?;
    // Also shared by policy changes and undo: mode switches and bulk rollback are barriers.
    sqlx::query("SELECT pg_advisory_xact_lock(424244)")
        .execute(&mut *tx)
        .await?;
    let (revision, value): (i64, serde_json::Value) =
        sqlx::query_as("SELECT revision, policy FROM automod_config WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    let policy: Policy = serde_json::from_value(value).map_err(anyhow::Error::from)?;
    policy.validate()?;
    let tid: Option<i32> = sqlx::query_scalar("SELECT tid FROM posts WHERE pid = $1")
        .bind(pid)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(tid) = tid {
        let thread: Option<(i32, i32, i16, String)> = sqlx::query_as(
            "SELECT fid, firstpost, visible, closed FROM threads WHERE tid = $1 FOR UPDATE",
        )
        .bind(tid)
        .fetch_optional(&mut *tx)
        .await?;
        let row: Option<(i32, i32, i16, String, String)> = sqlx::query_as(
            "SELECT uid, tid, visible, subject, message FROM posts WHERE pid = $1 FOR UPDATE",
        )
        .bind(pid)
        .fetch_optional(&mut *tx)
        .await?;
        let queued: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM automod_queue WHERE pid = $1)")
                .bind(pid)
                .fetch_one(&mut *tx)
                .await?;
        if let (
            Some((fid, firstpost, tvis, closed)),
            Some((uid, current_tid, vis, subject, message)),
        ) = (thread, row)
        {
            // If a move raced the initial lookup, leave the item queued for a fresh attempt.
            if current_tid != tid {
                return Ok(());
            }
            let reviewed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM automod_actions WHERE pid = $1 AND status = 'undone')").bind(pid).fetch_one(&mut *tx).await?;
            if queued
                && !reviewed
                && policy.mode != "off"
                && vis == 1
                && tvis == 1
                && !closed.starts_with("moved|")
            {
                let user: Option<crate::models::User> = sqlx::query_as(&format!(
                    "SELECT {} FROM users WHERE uid = $1",
                    crate::models::USER_COLUMNS
                ))
                .bind(uid)
                .fetch_optional(&mut *tx)
                .await?;
                let cache = app.cache();
                let (protected, count) = if let Some(u) = user {
                    let mut groups = u.additionalgroups.clone();
                    groups.push(u.usergroup);
                    (
                        u.is_system
                            || cache.is_any_moderator(uid, &groups, &cache.group_perms(&groups)),
                        u.postnum,
                    )
                } else {
                    (false, 0)
                };
                if !protected {
                    let duplicates: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT pid FROM posts WHERE uid = $1 AND dateline >= $2 AND lower(btrim(message)) = lower(btrim($3)) LIMIT 20) recent")
                        .bind(uid).bind(now() - 600).bind(&message).fetch_one(&mut *tx).await?;
                    let reasons = policy.reasons(&subject, &message, count, duplicates);
                    if !reasons.is_empty() {
                        let first = firstpost == pid;
                        let status = if policy.mode == "quarantine" {
                            ops::automod_visibility(&mut tx, pid, tid, first, 0).await?;
                            "quarantined"
                        } else {
                            "observed"
                        };
                        let prev: i64 =
                            sqlx::query_scalar("SELECT automod_revision FROM posts WHERE pid = $1")
                                .bind(pid)
                                .fetch_one(&mut *tx)
                                .await?;
                        let trev: i64 = sqlx::query_scalar(
                            "SELECT automod_revision FROM threads WHERE tid = $1",
                        )
                        .bind(tid)
                        .fetch_one(&mut *tx)
                        .await?;
                        let system_uid: i32 =
                            sqlx::query_scalar("SELECT uid FROM users WHERE is_system")
                                .fetch_one(&mut *tx)
                                .await?;
                        let id: i64 = sqlx::query_scalar("INSERT INTO automod_actions (pid,tid,fid,uid,dateline,policy_revision,reasons,status,post_revision,thread_revision,is_first) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
                            .bind(pid).bind(tid).bind(fid).bind(system_uid).bind(now()).bind(revision).bind(serde_json::json!(reasons)).bind(status).bind(prev).bind(trev).bind(first).fetch_one(&mut *tx).await?;
                        log(&mut tx, system_uid, fid, tid, pid, &format!("System automod: {status}"), serde_json::json!({"action_id":id,"reasons":reasons,"policy_revision":revision})).await?;
                    }
                }
            }
        }
    }
    sqlx::query("DELETE FROM automod_queue WHERE pid = $1")
        .bind(pid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.mod_counts.invalidate_all();
    app.content_changed();
    Ok(())
}

/// Return false on conflict; never overwrite later human decisions or missing content.
pub async fn undo(app: &App, id: i64, actor: i32) -> AppResult<bool> {
    let mut tx = app.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(424244)")
        .execute(&mut *tx)
        .await?;
    let action: Option<(i32,i32,i32,i64,i64,bool)> = sqlx::query_as("SELECT pid,tid,fid,post_revision,thread_revision,is_first FROM automod_actions WHERE id = $1 AND status = 'quarantined' FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?;
    let Some((pid, tid, fid, prev, trev, first)) = action else {
        return Ok(false);
    };
    let thread: Option<(i16, i64, i32, i32)> = sqlx::query_as(
        "SELECT visible, automod_revision, firstpost, fid FROM threads WHERE tid = $1 FOR UPDATE",
    )
    .bind(tid)
    .fetch_optional(&mut *tx)
    .await?;
    let post: Option<(i16, i64, i32, i32)> = sqlx::query_as(
        "SELECT visible, automod_revision, tid, fid FROM posts WHERE pid = $1 FOR UPDATE",
    )
    .bind(pid)
    .fetch_optional(&mut *tx)
    .await?;
    let valid = post == Some((0, prev, tid, fid))
        && thread.is_some_and(|(v, r, f, forum)| {
            forum == fid
                && if first {
                    r == trev && v == 0 && f == pid
                } else {
                    v == 1 && f != pid
                }
        });
    if !valid {
        return Ok(false);
    }
    ops::automod_visibility(&mut tx, pid, tid, first, 1).await?;
    sqlx::query("UPDATE automod_actions SET status = 'undone', undone_by = $2, undone_at = $3 WHERE id = $1").bind(id).bind(actor).bind(now()).execute(&mut *tx).await?;
    log(
        &mut tx,
        actor,
        fid,
        tid,
        pid,
        "System automod: undone",
        serde_json::json!({"action_id":id}),
    )
    .await?;
    tx.commit().await?;
    app.mod_counts.invalidate_all();
    app.content_changed();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy {
            mode: "observe".into(),
            max_links: 3,
            new_user_posts: 5,
            duplicate_limit: 3,
            phrases: vec!["buy spam".into()],
        }
    }
    #[test]
    fn rules_and_trust() {
        let p = policy();
        assert_eq!(
            p.reasons("BUY SPAM", "https://a https://b https://c", 1, 3)
                .len(),
            3
        );
        assert_eq!(
            p.reasons("", "https://a https://b https://c", 5, 3).len(),
            0
        );
        assert_eq!(p.reasons("", "BUY spam", 100, 1).len(), 1);
        assert!(p.reasons("", "Normal discussion", 0, 1).is_empty());
    }
    #[test]
    fn validates_bounds() {
        let mut p = policy();
        assert!(p.validate().is_ok());
        p.mode = "delete".into();
        assert!(p.validate().is_err());
        p = policy();
        p.phrases.push(" ".into());
        assert!(p.validate().is_err());
        p = policy();
        p.duplicate_limit = 1;
        assert!(p.validate().is_err());
    }
    #[test]
    fn link_formats_and_disabled_rule() {
        let mut p = policy();
        assert_eq!(
            p.reasons("", "www.a [url]a[/url] [url=a]b[/url]", 0, 1)
                .len(),
            1
        );
        p.max_links = 0;
        assert!(
            p.reasons("", "https://a https://b https://c", 0, 1)
                .is_empty()
        );
    }
}
