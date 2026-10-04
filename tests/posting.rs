//! Posting under concurrency: counters stay consistent, merges serialize, edits are atomic.

mod common;

use common::{Client, TestApp};

#[tokio::test]
async fn api_requires_an_available_prefix_when_the_forum_requires_one() {
    let t = test_app!();
    let fid = forum(&t).await;
    sqlx::query("UPDATE forums SET requireprefix = TRUE WHERE fid = $1")
        .bind(fid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let prefix: i32 = sqlx::query_scalar(
        "INSERT INTO threadprefixes (prefix, forums) VALUES ('Required', $1) RETURNING pid",
    )
    .bind(vec![fid])
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["forums", "prefixes"]).await.unwrap();
    let c = t.login_as(1).await;
    let path = format!("/api/v1/forums/{fid}/threads");
    for bad in [0, -1, i32::MAX] {
        let r = c.send_json("POST", &path, serde_json::json!({"subject":"Missing prefix", "message":"Opening post", "prefix":bad}), &[("x-csrf-token", &c.csrf())]).await;
        assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    }
    let r = c.send_json("POST", &path, serde_json::json!({"subject":"Valid prefix", "message":"Opening post", "prefix":prefix}), &[("x-csrf-token", &c.csrf())]).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn setting(t: &TestApp, k: &str, v: &str) {
    sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .bind(k)
        .bind(v)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
}

async fn forum(t: &TestApp) -> i32 {
    sqlx::query_scalar("SELECT fid FROM forums WHERE name = 'General Discussion'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

async fn new_thread(t: &TestApp, c: &Client, fid: i32) -> i32 {
    let r = c
        .post_form(
            &format!("/newthread/{fid}"),
            &[
                ("subject", "Concurrency"),
                ("message", "The opening post of this thread."),
            ],
        )
        .await;
    assert!(
        r.status.is_redirection(),
        "new thread: {} {}",
        r.status,
        r.body
            .split("<main")
            .nth(1)
            .unwrap_or("")
            .chars()
            .take(600)
            .collect::<String>()
    );
    sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

async fn assert_consistent(t: &TestApp) {
    let problems = rbb::ops::check_counters(&t.db.pool).await.unwrap();
    assert!(problems.is_empty(), "counter problems: {problems:?}");
}

#[tokio::test]
async fn concurrent_replies_keep_counters_consistent() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "postmergemins", "0").await;
    let fid = forum(&t).await;
    let author = t.create_user("starter", "Passw0rd-starter").await;
    let tid = new_thread(&t, &t.login_as(author).await, fid).await;
    let mut clients = vec![];
    for i in 0..12 {
        let uid = t
            .create_user(&format!("replier{i}"), "Passw0rd-replier")
            .await;
        clients.push(t.login_as(uid).await);
    }
    let path = format!("/newreply/{tid}");
    let futs = clients
        .iter()
        .map(|c| c.post_form(&path, &[("message", "A reply written at the same moment.")]));
    for r in futures::future::join_all(futs).await {
        assert!(r.status.is_redirection(), "reply: {}", r.status);
    }
    let replies: i32 = sqlx::query_scalar("SELECT replies FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(replies, 12);
    assert_consistent(&t).await;
    // Notifications were queued in the same transactions, one job per post.
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE kind = 'post_notifications'")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(jobs, 13);
}

#[tokio::test]
async fn simultaneous_double_posts_merge_into_one() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "postmergemins", "60").await;
    let fid = forum(&t).await;
    let starter = t.create_user("opener", "Passw0rd-opener").await;
    let tid = new_thread(&t, &t.login_as(starter).await, fid).await;
    let uid = t.create_user("doubler", "Passw0rd-doubler").await;
    let c = t.login_as(uid).await;
    let path = format!("/newreply/{tid}");
    // A first reply, then four more at once: all four must merge into it, none may become
    // a separate post or be lost.
    c.post_form(&path, &[("message", "first part")]).await;
    let parts = ["part a", "part b", "part c", "part d"];
    let forms: Vec<[(&str, &str); 1]> = parts.iter().map(|m| [("message", *m)]).collect();
    let futs = forms.iter().map(|f| c.post_form(&path, f));
    futures::future::join_all(futs).await;
    let posts: Vec<String> =
        sqlx::query_scalar("SELECT message FROM posts WHERE tid = $1 AND uid = $2")
            .bind(tid)
            .bind(uid)
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(posts.len(), 1, "{posts:?}");
    for p in parts {
        assert!(posts[0].contains(p), "{p} lost: {}", posts[0]);
    }
    assert_consistent(&t).await;
}

#[tokio::test]
async fn editing_keeps_history_and_subject_together() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "keepedithistory", "1").await;
    let fid = forum(&t).await;
    let uid = t.create_user("editor", "Passw0rd-editor").await;
    let c = t.login_as(uid).await;
    let tid = new_thread(&t, &c, fid).await;
    let pid: i32 = sqlx::query_scalar("SELECT firstpost FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let r = c
        .post_form(
            &format!("/editpost/{pid}"),
            &[
                ("subject", "Renamed"),
                ("message", "Edited text of the opening post."),
                ("editreason", "typo"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "edit: {}", r.status);
    let (tsub, fsub): (String, String) = sqlx::query_as(
        "SELECT t.subject, f.lastpostsubject FROM threads t JOIN forums f ON f.fid = t.fid WHERE t.tid = $1",
    )
    .bind(tid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!((tsub.as_str(), fsub.as_str()), ("Renamed", "Renamed"));
    let (hsub, hmsg, reason): (String, String, String) =
        sqlx::query_as("SELECT subject, message, reason FROM post_edits WHERE pid = $1")
            .bind(pid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(hsub, "Concurrency");
    assert_eq!(hmsg, "The opening post of this thread.");
    assert_eq!(reason, "typo");
}

#[tokio::test]
async fn moderator_merge_and_delete_of_the_same_posts_serialize() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "postmergemins", "0").await;
    let fid = forum(&t).await;
    let uid = t.create_user("poster", "Passw0rd-poster").await;
    let c = t.login_as(uid).await;
    let tid = new_thread(&t, &c, fid).await;
    for i in 0..4 {
        c.post_form(
            &format!("/newreply/{tid}"),
            &[("message", &format!("reply number {i}"))],
        )
        .await;
    }
    let pids: Vec<i32> =
        sqlx::query_scalar("SELECT pid FROM posts WHERE tid = $1 ORDER BY pid OFFSET 1")
            .bind(tid)
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    let (m, d) = tokio::join!(
        rbb::ops::merge_posts(&t.app, &pids, "\n"),
        rbb::ops::delete_posts(&t.app, &pids[2..])
    );
    // Either order is fine; what matters is that nothing is half-applied.
    let _ = (m, d);
    assert_consistent(&t).await;
}
