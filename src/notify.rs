//! Notifications: thread/forum subscriptions (email or PM), alerts for quotes, mentions,
//! replies, PMs and reputation, delivered live over SSE.

use crate::app::{App, LiveEvent};
use crate::util::now;

/// Create an alert for a user (respects the board toggle and ignore lists).
pub async fn alert(
    app: &App,
    uid: i32,
    from_uid: i32,
    kind: &str,
    object_id: i32,
    extra: serde_json::Value,
) {
    if uid == 0 || uid == from_uid || !app.cache().settings.bool("enablealerts") {
        return;
    }
    if from_uid > 0 {
        let ignored: Option<bool> =
            sqlx::query_scalar("SELECT $2 = ANY(ignorelist) FROM users WHERE uid = $1")
                .bind(uid)
                .bind(from_uid)
                .fetch_optional(&app.db)
                .await
                .ok()
                .flatten();
        if ignored == Some(true) {
            return;
        }
    }
    let r = sqlx::query(
        "INSERT INTO alerts (uid, from_uid, kind, object_id, extra, dateline) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(uid)
    .bind(from_uid)
    .bind(kind)
    .bind(object_id)
    .bind(&extra)
    .bind(now())
    .execute(&app.db)
    .await;
    if r.is_ok() {
        let _ = sqlx::query("UPDATE users SET unreadalerts = unreadalerts + 1 WHERE uid = $1")
            .bind(uid)
            .execute(&app.db)
            .await;
        app.publish_all(LiveEvent {
            kind: "alert",
            tid: 0,
            uid,
            data: serde_json::json!({"kind": kind}),
        })
        .await;
    }
}

pub async fn thread_subscribers(
    app: &App,
    tid: i32,
    pid: i32,
    poster_uid: i32,
    subject: &str,
    poster: &str,
    message: &str,
) {
    let cache = app.cache();
    let s = &cache.settings;
    // Only notify subscribers who have visited since the last notification (MyBB behaviour):
    // lastactive > previous post's dateline avoids flooding inactive users.
    let subs: Vec<(i32, i16, String, String, i64)> = match sqlx::query_as(
        "SELECT u.uid, ts.notification, u.email, u.username, u.lastactive FROM threadsubscriptions ts
         JOIN users u ON u.uid = ts.uid WHERE ts.tid = $1 AND ts.uid <> $2",
    )
    .bind(tid)
    .bind(poster_uid)
    .fetch_all(&app.db)
    .await
    {
        Ok(v) => v,
        Err(_) => return,
    };
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    let excerpt = crate::util::truncate_chars(&crate::parser::to_plaintext(message), 500);
    for (uid, notification, email, username, _lastactive) in subs {
        alert(
            app,
            uid,
            poster_uid,
            "subscribed_thread",
            pid,
            serde_json::json!({"tid": tid, "subject": subject, "poster": poster}),
        )
        .await;
        match notification {
            1 => {
                let body = format!(
                    "{username},\n\n{poster} has just replied to a thread you have subscribed to at {}. This thread is titled \"{subject}\".\n\nHere is an excerpt of the message:\n------------------------------------------\n{excerpt}\n------------------------------------------\n\nTo view the thread, go to:\n{bburl}/post/{pid}\n\nTo unsubscribe, visit your subscriptions in the User CP:\n{bburl}/usercp/subscriptions\n",
                    s.get("bbname")
                );
                crate::mail::queue(app, &email, &format!("New Reply to {subject}"), &body).await;
            }
            2 => {
                let msg = format!(
                    "{} has just replied to a thread you are subscribed to: [url={bburl}/post/{pid}]{}[/url]\n\n[quote]{}[/quote]",
                    crate::parser::literal(&poster),
                    crate::parser::literal(&subject),
                    crate::parser::literal(&excerpt),
                );
                let _ = crate::routes::private::send_system_pm(
                    app,
                    uid,
                    &format!("New Reply to {subject}"),
                    &msg,
                )
                .await;
            }
            _ => {}
        }
    }
}

pub async fn forum_subscribers(
    app: &App,
    fid: i32,
    tid: i32,
    poster_uid: i32,
    subject: &str,
    poster: &str,
) {
    let cache = app.cache();
    let s = &cache.settings;
    let forum_name = cache.forum(fid).map(|f| f.name.clone()).unwrap_or_default();
    let subs: Vec<(i32, String, String)> = match sqlx::query_as(
        "SELECT u.uid, u.email, u.username FROM forumsubscriptions fs JOIN users u ON u.uid = fs.uid WHERE fs.fid = $1 AND fs.uid <> $2",
    )
    .bind(fid)
    .bind(poster_uid)
    .fetch_all(&app.db)
    .await
    {
        Ok(v) => v,
        Err(_) => return,
    };
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    for (uid, email, username) in subs {
        // Respect current permissions of the subscriber.
        let groups: Option<(i32, Vec<i32>)> =
            sqlx::query_as("SELECT usergroup, additionalgroups FROM users WHERE uid = $1")
                .bind(uid)
                .fetch_optional(&app.db)
                .await
                .ok()
                .flatten();
        if let Some((g, extra)) = groups {
            let mut all = vec![g];
            all.extend(extra);
            let fp = cache.forum_perms(&all, fid);
            if !fp.canview || !fp.canviewthreads || fp.canonlyviewownthreads {
                continue;
            }
        }
        alert(app, uid, poster_uid, "subscribed_forum", tid, serde_json::json!({"tid": tid, "subject": subject, "poster": poster, "forum": forum_name})).await;
        let body = format!(
            "{username},\n\n{poster} has just started a new thread in \"{forum_name}\", a forum you are subscribed to at {}.\n\nThe thread is titled \"{subject}\":\n{bburl}/thread/{tid}\n",
            s.get("bbname")
        );
        crate::mail::queue(app, &email, &format!("New Thread in {forum_name}"), &body).await;
    }
}

pub async fn mentions_and_quotes(
    app: &App,
    poster_uid: i32,
    poster: &str,
    tid: i32,
    pid: i32,
    subject: &str,
    message: &str,
) {
    let cache = app.cache();
    let mut notified: Vec<i32> = vec![poster_uid];
    for qpid in crate::parser::extract_quoted_pids(message) {
        let quoted: Option<i32> = sqlx::query_scalar("SELECT uid FROM posts WHERE pid = $1")
            .bind(qpid)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
        if let Some(q) = quoted {
            if q > 0 && !notified.contains(&q) {
                notified.push(q);
                alert(
                    app,
                    q,
                    poster_uid,
                    "quoted",
                    pid,
                    serde_json::json!({"tid": tid, "subject": subject, "poster": poster}),
                )
                .await;
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
                    .await
                    .unwrap_or_default();
            for u in uids {
                if !notified.contains(&u) {
                    notified.push(u);
                    alert(
                        app,
                        u,
                        poster_uid,
                        "mention",
                        pid,
                        serde_json::json!({"tid": tid, "subject": subject, "poster": poster}),
                    )
                    .await;
                }
            }
        }
    }
}
