//! Notifications: thread/forum subscriptions (email or PM), alerts for quotes, mentions,
//! replies, PMs and reputation, delivered live over SSE.

use crate::app::{App, LiveEvent};
use crate::infra::outbox::{Delivery, first_delivery};
use crate::util::now;

/// Create an alert for a user (respects the board toggle and ignore lists). A failure is logged:
/// for alerts that must not be lost, enqueue a [`crate::infra::outbox::Job::Alert`] instead.
pub async fn alert(
    app: &App,
    uid: i32,
    from_uid: i32,
    kind: &str,
    object_id: i32,
    extra: serde_json::Value,
) {
    if let Err(e) = deliver_alert(app, None, uid, from_uid, kind, object_id, extra).await {
        tracing::warn!(uid, kind, error = %format!("{e:#}"), "could not create alert");
    }
}

/// Create an alert for a user (respects the board toggle and ignore lists). With a delivery
/// key, an alert already created under that key is not created again.
pub async fn deliver_alert(
    app: &App,
    key: Option<&str>,
    uid: i32,
    from_uid: i32,
    kind: &str,
    object_id: i32,
    extra: serde_json::Value,
) -> anyhow::Result<()> {
    if uid == 0 || uid == from_uid || !app.cache().settings.bool("enablealerts") {
        return Ok(());
    }
    if from_uid > 0 {
        let ignored: Option<bool> =
            sqlx::query_scalar("SELECT $2 = ANY(ignorelist) FROM users WHERE uid = $1")
                .bind(uid)
                .bind(from_uid)
                .fetch_optional(&app.db)
                .await?;
        if ignored == Some(true) {
            return Ok(());
        }
    }
    let mut tx = app.db.begin().await?;
    if let Some(key) = key
        && !first_delivery(&mut tx, key).await?
    {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO alerts (uid, from_uid, kind, object_id, extra, dateline) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(uid)
    .bind(from_uid)
    .bind(kind)
    .bind(object_id)
    .bind(&extra)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE users SET unreadalerts = unreadalerts + 1 WHERE uid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.publish_all(LiveEvent {
        kind: "alert",
        tid: 0,
        uid,
        data: serde_json::json!({"kind": kind}),
    })
    .await;
    Ok(())
}

/// Of `uids`, those who may currently read the thread `tid` (its forum, ancestors, passwords,
/// "own threads only"…). Notifications carry subjects and excerpts, so they follow the same rules
/// as viewing.
async fn readers(
    app: &App,
    tid: i32,
    uids: &[i32],
) -> sqlx::Result<std::collections::HashSet<i32>> {
    let mut ok = std::collections::HashSet::new();
    if uids.is_empty() {
        return Ok(ok);
    }
    let Some((fid, author, visible)) = sqlx::query_as::<_, (i32, i32, i16)>(
        "SELECT fid, uid, visible FROM threads WHERE tid = $1",
    )
    .bind(tid)
    .fetch_optional(&app.db)
    .await?
    else {
        // The thread is gone: nobody hears about it.
        return Ok(ok);
    };
    let rows: Vec<(i32, i32, Vec<i32>)> =
        sqlx::query_as("SELECT uid, usergroup, additionalgroups FROM users WHERE uid = ANY($1)")
            .bind(uids)
            .fetch_all(&app.db)
            .await?;
    let cache = app.cache();
    for (uid, g, extra) in rows {
        let mut groups = vec![g];
        groups.extend(extra);
        let access = crate::domain::access::member(&cache, uid, &groups);
        let state_ok = visible == 1
            || access
                .forum(fid)
                .is_ok_and(|a| a.visible_states().contains(&visible));
        if state_ok && access.can_read_thread(fid, author, uid) {
            ok.insert(uid);
        }
    }
    Ok(ok)
}

#[allow(clippy::too_many_arguments)]
pub async fn thread_subscribers(
    app: &App,
    d: Delivery<'_>,
    tid: i32,
    pid: i32,
    poster_uid: i32,
    subject: &str,
    poster: &str,
    message: &str,
) -> anyhow::Result<()> {
    let cache = app.cache();
    let s = &cache.settings;
    // Only notify subscribers who have visited since the last notification (MyBB behaviour):
    // lastactive > previous post's dateline avoids flooding inactive users.
    let subs: Vec<(i32, i16, String, String, i64)> = sqlx::query_as(
        "SELECT u.uid, ts.notification, u.email, u.username, u.lastactive FROM threadsubscriptions ts
         JOIN users u ON u.uid = ts.uid WHERE ts.tid = $1 AND ts.uid <> $2",
    )
    .bind(tid)
    .bind(poster_uid)
    .fetch_all(&app.db)
    .await?;
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    let excerpt = crate::util::truncate_chars(&crate::parser::to_plaintext(message), 500);
    let allowed = readers(app, tid, &subs.iter().map(|s| s.0).collect::<Vec<_>>()).await?;
    for (uid, notification, email, username, _lastactive) in subs {
        if !allowed.contains(&uid) {
            continue;
        }
        deliver_alert(
            app,
            Some(&d.key("alert:subscribed_thread", uid)),
            uid,
            poster_uid,
            "subscribed_thread",
            pid,
            serde_json::json!({"tid": tid, "subject": subject, "poster": poster}),
        )
        .await?;
        match notification {
            1 => {
                let body = format!(
                    "{username},\n\n{poster} has just replied to a thread you have subscribed to at {}. This thread is titled \"{subject}\".\n\nHere is an excerpt of the message:\n------------------------------------------\n{excerpt}\n------------------------------------------\n\nTo view the thread, go to:\n{bburl}/post/{pid}\n\nTo unsubscribe, visit your subscriptions in the User CP:\n{bburl}/usercp/subscriptions\n",
                    s.get("bbname")
                );
                crate::mail::deliver(
                    app,
                    Some(&d.key("mail:subscribed_thread", uid)),
                    &email,
                    &format!("New Reply to {subject}"),
                    &body,
                )
                .await?;
            }
            2 => {
                let msg = format!(
                    "{} has just replied to a thread you are subscribed to: [url={bburl}/post/{pid}]{}[/url]\n\n[quote]{}[/quote]",
                    crate::parser::literal(poster),
                    crate::parser::literal(subject),
                    crate::parser::literal(&excerpt),
                );
                crate::routes::private::deliver_system_pm(
                    app,
                    Some(&d.key("pm:subscribed_thread", uid)),
                    uid,
                    &format!("New Reply to {subject}"),
                    &msg,
                )
                .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub async fn forum_subscribers(
    app: &App,
    d: Delivery<'_>,
    fid: i32,
    tid: i32,
    poster_uid: i32,
    subject: &str,
    poster: &str,
) -> anyhow::Result<()> {
    let cache = app.cache();
    let s = &cache.settings;
    let forum_name = cache.forum(fid).map(|f| f.name.clone()).unwrap_or_default();
    let subs: Vec<(i32, String, String, i32, Vec<i32>)> = sqlx::query_as(
        "SELECT u.uid, u.email, u.username, u.usergroup, u.additionalgroups
         FROM forumsubscriptions fs JOIN users u ON u.uid = fs.uid WHERE fs.fid = $1 AND fs.uid <> $2",
    )
    .bind(fid)
    .bind(poster_uid)
    .fetch_all(&app.db)
    .await?;
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    for (uid, email, username, g, extra) in subs {
        // Load current permissions with the subscription rows, avoiding a query per member.
        let mut all = vec![g];
        all.extend(extra);
        let access = crate::domain::access::member(&cache, uid, &all);
        if !access
            .forum(fid)
            .is_ok_and(|a| a.threads == crate::domain::access::Threads::All)
        {
            continue;
        }
        deliver_alert(app, Some(&d.key("alert:subscribed_forum", uid)), uid, poster_uid, "subscribed_forum", tid, serde_json::json!({"tid": tid, "subject": subject, "poster": poster, "forum": forum_name})).await?;
        let body = format!(
            "{username},\n\n{poster} has just started a new thread in \"{forum_name}\", a forum you are subscribed to at {}.\n\nThe thread is titled \"{subject}\":\n{bburl}/thread/{tid}\n",
            s.get("bbname")
        );
        crate::mail::deliver(
            app,
            Some(&d.key("mail:subscribed_forum", uid)),
            &email,
            &format!("New Thread in {forum_name}"),
            &body,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn mentions_and_quotes(
    app: &App,
    d: Delivery<'_>,
    poster_uid: i32,
    poster: &str,
    tid: i32,
    pid: i32,
    subject: &str,
    message: &str,
) -> anyhow::Result<()> {
    let cache = app.cache();
    let mut notified: Vec<i32> = vec![poster_uid];
    let mut candidates: Vec<(i32, &str)> = vec![];
    let quoted_pids = crate::parser::extract_quoted_pids(message);
    if !quoted_pids.is_empty() {
        let quoted: Vec<i32> =
            sqlx::query_scalar("SELECT uid FROM posts WHERE pid = ANY($1) ORDER BY pid")
                .bind(&quoted_pids)
                .fetch_all(&app.db)
                .await?;
        for q in quoted {
            if q > 0 && !notified.contains(&q) {
                notified.push(q);
                candidates.push((q, "quoted"));
            }
        }
    }
    if cache.settings.bool("enablementions") {
        let names = crate::parser::extract_mentions(message);
        if !names.is_empty() {
            let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
            let uids: Vec<i32> =
                sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = ANY($1)")
                    .bind(&lower)
                    .fetch_all(&app.db)
                    .await?;
            for u in uids {
                if !notified.contains(&u) {
                    notified.push(u);
                    candidates.push((u, "mention"));
                }
            }
        }
    }
    // Only members who can read the thread hear about it.
    let allowed = readers(
        app,
        tid,
        &candidates.iter().map(|c| c.0).collect::<Vec<_>>(),
    )
    .await?;
    for (u, kind) in candidates {
        if allowed.contains(&u) {
            deliver_alert(
                app,
                Some(&d.key(&format!("alert:{kind}"), u)),
                u,
                poster_uid,
                kind,
                pid,
                serde_json::json!({"tid": tid, "subject": subject, "poster": poster}),
            )
            .await?;
        }
    }
    Ok(())
}
