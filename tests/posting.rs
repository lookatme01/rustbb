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
async fn copying_a_thread_updates_its_authors_thread_count() {
    let t = test_app!();
    let fid = forum(&t).await;
    let uid = t.create_user("copiedauthor", "Passw0rd-author").await;
    let tid = new_thread(&t, &t.login_as(uid).await, fid).await;
    rbb::ops::copy_thread(&t.app, tid, fid).await.unwrap();
    let counts: (i32, i32) = sqlx::query_as("SELECT threadnum, postnum FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(counts, (2, 2));
    assert_consistent(&t).await;
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
async fn parallel_posts_cannot_dodge_flood_control() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "1").await;
    setting(&t, "postfloodsecs", "600").await;
    setting(&t, "postmergemins", "0").await;
    let fid = forum(&t).await;
    let starter = t.create_user("floodopener", "Passw0rd-opener").await;
    let tid = new_thread(&t, &t.login_as(starter).await, fid).await;
    // Everyone is past the flood window when the requests start, so each passes the
    // snapshot check; only the locked re-check can stop the extra ones.
    sqlx::query("UPDATE users SET lastpost = 0")
        .execute(&t.db.pool)
        .await
        .unwrap();
    let uid = t.create_user("flooder", "Passw0rd-flooder").await;
    let c = t.login_as(uid).await;
    let path = format!("/newreply/{tid}");
    let futs = (0..5).map(|_| c.post_form(&path, &[("message", "A reply sent in a burst.")]));
    let ok = futures::future::join_all(futs)
        .await
        .iter()
        .filter(|r| r.status.is_redirection())
        .count();
    assert_eq!(ok, 1, "only one reply of the burst may get through");
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE tid = $1 AND uid = $2")
        .bind(tid)
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(posts, 1);
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

#[tokio::test]
async fn the_api_honours_reply_own_threads_only_and_deleted_threads() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    let fid = forum(&t).await;
    let admin = t.login_as(1).await;
    let tid = new_thread(&t, &admin, fid).await;
    let member = t.create_user("replier", "Passw0rd-replier").await;
    let c = t.login_as(member).await;
    let path = format!("/api/v1/threads/{tid}/posts");
    let r = api_reply(&c, &path).await;
    assert_eq!(r.status, 200, "{}", r.body);

    sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES ($1, 2, '{\"canonlyreplyownthreads\":true}') ON CONFLICT (fid, gid) DO UPDATE SET perms = EXCLUDED.perms")
        .bind(fid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums", "forumperms"]).await.unwrap();
    let r = api_reply(&c, &path).await;
    assert_eq!(r.status, 403, "{}", r.body);

    sqlx::query("DELETE FROM forumpermissions WHERE fid = $1 AND gid = 2")
        .bind(fid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE threads SET visible = -1 WHERE tid = $1")
        .bind(tid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums", "forumperms"]).await.unwrap();
    let r = admin
        .send_json(
            "POST",
            &path,
            serde_json::json!({"message": "Into a deleted thread."}),
            &[("x-csrf-token", &admin.csrf())],
        )
        .await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
}

#[tokio::test]
async fn the_moderation_queue_only_acts_on_items_awaiting_approval() {
    let t = test_app!();
    let fid = forum(&t).await;
    let admin = t.login_as(1).await;
    let tid = new_thread(&t, &admin, fid).await;
    let moderator = t.create_user("queuemod", "Passw0rd-queuemod").await;
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, FALSE, '{}')")
        .bind(fid)
        .bind(moderator)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let c = t.login_as(moderator).await;
    let id = tid.to_string();
    // A live thread is not in the queue, so the queue cannot delete it.
    c.post_form("/modcp/modqueue", &[("kind", "threads"), ("delete", &id)])
        .await;
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(live, 1);
    // Nor can it restore a thread a moderator soft-deleted.
    sqlx::query("UPDATE threads SET visible = -1 WHERE tid = $1")
        .bind(tid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    c.post_form("/modcp/modqueue", &[("kind", "threads"), ("approve", &id)])
        .await;
    let v: i16 = sqlx::query_scalar("SELECT visible FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(v, -1);
    // A genuinely pending thread is approved as before.
    sqlx::query("UPDATE threads SET visible = 0 WHERE tid = $1")
        .bind(tid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    c.post_form("/modcp/modqueue", &[("kind", "threads"), ("approve", &id)])
        .await;
    let v: i16 = sqlx::query_scalar("SELECT visible FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(v, 1);
}

async fn api_reply(c: &Client, path: &str) -> common::Response {
    let csrf = c.csrf();
    c.send_json(
        "POST",
        path,
        serde_json::json!({"message": "A reply from the API."}),
        &[("x-csrf-token", &csrf)],
    )
    .await
}

#[tokio::test]
async fn drafts_cannot_point_at_threads_the_member_cannot_see() {
    let t = test_app!();
    let fid = forum(&t).await;
    let admin = t.login_as(1).await;
    let tid = new_thread(&t, &admin, fid).await;
    let member = t.create_user("drafter", "Passw0rd-drafter").await;
    let c = t.login_as(member).await;
    let tid_s = tid.to_string();
    let fid_s = fid.to_string();
    let save = |tid: &str, fid: &str| {
        let fields = [
            ("tid", tid.to_string()),
            ("fid", fid.to_string()),
            ("message", "draft".to_string()),
        ];
        let c = &c;
        async move {
            let f: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
            c.post_form("/drafts/save", &f).await
        }
    };
    assert_eq!(save(&tid_s, &fid_s).await.status, 200);
    sqlx::query("UPDATE threads SET visible = -1 WHERE tid = $1")
        .bind(tid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let r = save(&tid_s, &fid_s).await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    let r = save("999999", "0").await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM drafts WHERE uid = $1")
        .bind(member)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_suspended_member_cannot_edit_posts_or_add_polls() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    let fid = forum(&t).await;
    let uid = t.create_user("suspended", "Passw0rd-suspended").await;
    let c = t.login_as(uid).await;
    let tid = new_thread(&t, &c, fid).await;
    let pid: i32 = sqlx::query_scalar("SELECT firstpost FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let r = edit_post(&c, pid, "An edit before the suspension.").await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    sqlx::query("UPDATE users SET suspendposting = TRUE, suspensiontime = 0 WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let r = edit_post(&c, pid, "An edit during the suspension.").await;
    assert!(r.body.contains("suspended"), "{} {}", r.status, r.body);
    let message: String = sqlx::query_scalar("SELECT message FROM posts WHERE pid = $1")
        .bind(pid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(message, "An edit before the suspension.");
    // A direct POST to start a poll is refused too, not just the form.
    let r = c
        .post_form(
            &format!("/thread/{tid}/poll/new"),
            &[("question", "Is this allowed?"), ("options", "Yes\nNo")],
        )
        .await;
    assert!(r.body.contains("suspended"), "{} {}", r.status, r.body);
    let polls: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM polls")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(polls, 0);
}

async fn edit_post(c: &Client, pid: i32, message: &str) -> common::Response {
    c.post_form(
        &format!("/editpost/{pid}"),
        &[("subject", "Concurrency"), ("message", message)],
    )
    .await
}

#[tokio::test]
async fn moving_a_thread_re_renders_posts_with_the_new_forums_settings() {
    let t = test_app!();
    let fids: Vec<i32> =
        sqlx::query_scalar("SELECT fid FROM forums WHERE type = 'f' ORDER BY fid LIMIT 2")
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    let (from, to) = (fids[0], fids[1]);
    sqlx::query("UPDATE forums SET allowimgcode = FALSE WHERE fid = $1")
        .bind(to)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    let c = t.login_as(1).await;
    let r = c
        .post_form(
            &format!("/newthread/{from}"),
            &[
                ("subject", "Picture thread"),
                (
                    "message",
                    "Look: [img]https://img.example.test/pic.png[/img]",
                ),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let tid: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    // Viewing renders and caches the post with images allowed.
    let page = c.get(&format!("/thread/{tid}")).await;
    assert!(
        page.body.contains("img.example.test/pic.png"),
        "{}",
        page.status
    );
    rbb::ops::move_threads(&t.app, &[tid], to, None)
        .await
        .unwrap();
    let page = c.get(&format!("/thread/{tid}")).await;
    assert!(
        !page
            .body
            .contains("<img src=\"https://img.example.test/pic.png"),
        "the moved post was served from HTML rendered for the old forum"
    );
}

#[tokio::test]
async fn the_recycle_bin_purge_only_removes_threads_that_are_still_deleted_and_old() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    let fid = forum(&t).await;
    let c = t.login_as(1).await;
    let old = new_thread(&t, &c, fid).await;
    let recent = new_thread(&t, &c, fid).await;
    let restored = new_thread(&t, &c, fid).await;
    rbb::ops::set_threads_visibility(&t.app, &[old, recent, restored], -1)
        .await
        .unwrap();
    let cutoff = rbb::util::now() - 90 * 86400;
    for tid in [old, restored] {
        sqlx::query("UPDATE threads SET deletetime = $2 WHERE tid = $1")
            .bind(tid)
            .bind(cutoff - 10)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    // A moderator restores one of the old threads before the purge gets to it.
    rbb::ops::set_threads_visibility(&t.app, &[restored], 1)
        .await
        .unwrap();
    let n = rbb::ops::purge_deleted_threads(&t.app, cutoff, 500)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let left: Vec<i32> =
        sqlx::query_scalar("SELECT tid FROM threads WHERE tid = ANY($1) ORDER BY tid")
            .bind(vec![old, recent, restored])
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(left, vec![recent, restored]);
    assert_consistent(&t).await;
}

#[tokio::test]
async fn reports_follow_their_post_when_a_thread_moves_to_another_forum() {
    let t = test_app!();
    let fids: Vec<i32> =
        sqlx::query_scalar("SELECT fid FROM forums WHERE type = 'f' ORDER BY fid LIMIT 2")
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    let (from, to) = (fids[0], fids[1]);
    let c = t.login_as(1).await;
    let tid = new_thread(&t, &c, from).await;
    let pid: i32 = sqlx::query_scalar("SELECT firstpost FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let rid: i32 = sqlx::query_scalar(
        "INSERT INTO reportedcontent (id, id2, id3, uid, type, reason, dateline, lastreport)
         VALUES ($1, 1, $2, 1, 'post', 'spam', 1, 1) RETURNING rid",
    )
    .bind(pid)
    .bind(from)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    rbb::ops::move_threads(&t.app, &[tid], to, None)
        .await
        .unwrap();
    let id3: i32 = sqlx::query_scalar("SELECT id3 FROM reportedcontent WHERE rid = $1")
        .bind(rid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(
        id3, to,
        "moderators of the new forum must be able to see the report"
    );
}

#[tokio::test]
async fn splitting_never_publishes_a_hidden_post() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "postmergemins", "0").await;
    let fid = forum(&t).await;
    let c = t.login_as(1).await;
    let tid = new_thread(&t, &c, fid).await;
    for i in 0..2 {
        let r = c
            .post_form(
                &format!("/newreply/{tid}"),
                &[("message", &format!("Reply number {i} in the thread."))],
            )
            .await;
        assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    }
    let pids: Vec<i32> = sqlx::query_scalar("SELECT pid FROM posts WHERE tid = $1 ORDER BY pid")
        .bind(tid)
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    // The first reply is soft-deleted; the second stays visible.
    rbb::ops::set_posts_visibility(&t.app, &[pids[1]], -1)
        .await
        .unwrap();
    let (hidden, shown) = (pids[1].to_string(), pids[2].to_string());
    let tid_s = tid.to_string();
    c.post_form(
        "/moderation/split",
        &[
            ("tid", &tid_s),
            ("pids", &hidden),
            ("pids", &shown),
            ("subject", "Split off"),
        ],
    )
    .await;
    let vis: i16 = sqlx::query_scalar("SELECT visible FROM posts WHERE pid = $1")
        .bind(pids[1])
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(vis, -1, "the deleted post must stay deleted");
    let stayed: i32 = sqlx::query_scalar("SELECT tid FROM posts WHERE pid = $1")
        .bind(pids[1])
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(stayed, tid);
    assert_consistent(&t).await;
}

#[tokio::test]
async fn subscribers_hear_nothing_about_a_reply_hidden_before_the_job_ran() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    let fid = forum(&t).await;
    let admin = t.login_as(1).await;
    let tid = new_thread(&t, &admin, fid).await;
    let watcher = t.create_user("watcher", "Passw0rd-watcher").await;
    sqlx::query(
        "INSERT INTO threadsubscriptions (uid, tid, notification, dateline) VALUES ($1, $2, 0, 1)",
    )
    .bind(watcher)
    .bind(tid)
    .execute(&t.db.pool)
    .await
    .unwrap();
    // Keep queued jobs from earlier requests out of the picture.
    rbb::infra::outbox::run_batch(&t.app).await.unwrap();
    let alerts = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM alerts WHERE uid = $1")
            .bind(watcher)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    };
    let before = alerts().await;
    let r = admin
        .post_form(
            &format!("/newreply/{tid}"),
            &[("message", "A reply that gets pulled.")],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let pid: i32 = sqlx::query_scalar("SELECT MAX(pid) FROM posts")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    // A moderator removes it before the notification job gets to run.
    rbb::ops::set_posts_visibility(&t.app, &[pid], -1)
        .await
        .unwrap();
    rbb::infra::outbox::run_batch(&t.app).await.unwrap();
    assert_eq!(alerts().await, before, "no alert for a post nobody can see");
}

#[tokio::test]
async fn twitch_embeds_name_the_boards_own_host_as_their_parent() {
    let t = test_app!();
    sqlx::query("UPDATE settings SET value = 'https://forum.example.test' WHERE name = 'bburl'")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let fid = forum(&t).await;
    let c = t.login_as(1).await;
    let r = c
        .post_form(
            &format!("/newthread/{fid}"),
            &[
                ("subject", "Stream"),
                (
                    "message",
                    "[video=twitch]https://www.twitch.tv/videos/123456789[/video]",
                ),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let tid: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let page = c.get(&format!("/thread/{tid}")).await;
    assert!(
        page.body.contains("parent=forum.example.test"),
        "{}",
        page.status
    );
    assert!(!page.body.contains("parent=localhost"));
}

#[tokio::test]
async fn the_moderation_queue_honours_the_moderators_approve_and_delete_rights() {
    let t = test_app!();
    let fid = forum(&t).await;
    let admin = t.login_as(1).await;
    let author = t.create_user("queueauthor", "Passw0rd-queueauthor").await;
    let moderator = t.create_user("limitedmod", "Passw0rd-limitedmod").await;
    // May approve threads, may not delete them.
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, FALSE, '{\"canapproveunapprovethreads\":true,\"candeletethreads\":false}')")
        .bind(fid)
        .bind(moderator)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let _ = admin;
    let mut pending = vec![];
    for i in 0..2 {
        let tid: i32 = sqlx::query_scalar(
            "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, $2, $3, 'queueauthor', 100, 100, 0) RETURNING tid",
        )
        .bind(fid)
        .bind(format!("Pending {i}"))
        .bind(author)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        pending.push(tid);
    }
    let c = t.login_as(moderator).await;
    let (keep, approve) = (pending[0].to_string(), pending[1].to_string());
    c.post_form("/modcp/modqueue", &[("kind", "threads"), ("delete", &keep)])
        .await;
    c.post_form(
        "/modcp/modqueue",
        &[("kind", "threads"), ("approve", &approve)],
    )
    .await;
    let state = |tid: i32| {
        let pool = t.db.pool.clone();
        async move {
            sqlx::query_scalar::<_, i16>("SELECT visible FROM threads WHERE tid = $1")
                .bind(tid)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        state(pending[0]).await,
        Some(0),
        "deleting needs candeletethreads"
    );
    assert_eq!(state(pending[1]).await, Some(1), "approving was allowed");
}

#[tokio::test]
async fn posting_while_a_moderator_toggles_the_authors_thread_does_not_deadlock() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "postmergemins", "0").await;
    let fid = forum(&t).await;
    let author = t.create_user("busyposter", "Passw0rd-busyposter").await;
    let c = t.login_as(author).await;
    let tid = new_thread(&t, &c, fid).await;
    let other = new_thread(&t, &c, fid).await;
    // Replies by the member (forum counters, then their post count) against a moderator hiding
    // and restoring the member's other thread (their post count, then the forum counters).
    let replies = async {
        let mut failures = vec![];
        for i in 0..120 {
            let r = c
                .post_form(
                    &format!("/newreply/{tid}"),
                    &[("message", &format!("concurrent reply number {i} here"))],
                )
                .await;
            if !r.status.is_redirection() {
                failures.push((i, r.status.as_u16()));
            }
        }
        failures
    };
    let toggling = async {
        for _ in 0..120 {
            rbb::ops::set_threads_visibility(&t.app, &[other], -1)
                .await
                .unwrap();
            rbb::ops::set_threads_visibility(&t.app, &[other], 1)
                .await
                .unwrap();
        }
    };
    let (failures, ()) = tokio::join!(replies, toggling);
    assert!(
        failures.is_empty(),
        "replies failed under moderation: {failures:?}"
    );
    assert_consistent(&t).await;
}

#[tokio::test]
async fn splitting_posts_that_have_since_gone_leaves_no_empty_thread() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    setting(&t, "postmergemins", "0").await;
    let fid = forum(&t).await;
    let c = t.login_as(1).await;
    let tid = new_thread(&t, &c, fid).await;
    for i in 0..3 {
        let r = c
            .post_form(
                &format!("/newreply/{tid}"),
                &[("message", &format!("reply {i} in the thread to split"))],
            )
            .await;
        assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    }
    let pids: Vec<i32> = sqlx::query_scalar("SELECT pid FROM posts WHERE tid = $1 ORDER BY pid")
        .bind(tid)
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    let threads_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    // The caller picked these replies, then someone else deleted both.
    sqlx::query("DELETE FROM posts WHERE pid = ANY($1)")
        .bind(vec![pids[1], pids[2]])
        .execute(&t.db.pool)
        .await
        .unwrap();
    // Raw SQL left the counters behind; settle them so the check at the end measures the splits.
    rbb::ops::rebuild_all_counters(&t.app).await.unwrap();
    let err = rbb::ops::split_posts(&t.app, &[pids[1], pids[2]], "stale split", fid).await;
    assert!(
        err.is_err(),
        "a split of posts that are gone must be refused"
    );
    let threads_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(threads_after, threads_before, "no empty thread was created");
    // The first post cannot be split off, even if it is in the list.
    let err = rbb::ops::split_posts(&t.app, &[pids[0]], "first post", fid).await;
    assert!(err.is_err(), "the opening post stays with its thread");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM threads")
            .fetch_one(&t.db.pool)
            .await
            .unwrap(),
        threads_before
    );
    // A list with a post that is still there splits just that one.
    let new_tid = rbb::ops::split_posts(&t.app, &[pids[3], pids[0]], "partial split", fid)
        .await
        .unwrap();
    let moved: Vec<i32> = sqlx::query_scalar("SELECT pid FROM posts WHERE tid = $1")
        .bind(new_tid)
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(moved, vec![pids[3]]);
    assert_consistent(&t).await;
}

#[tokio::test]
async fn merging_with_a_thread_that_has_since_gone_is_refused_not_an_error() {
    let t = test_app!();
    setting(&t, "postfloodcheck", "0").await;
    let fid = forum(&t).await;
    let c = t.login_as(1).await;
    let keep = new_thread(&t, &c, fid).await;
    let gone = new_thread(&t, &c, fid).await;
    rbb::ops::delete_threads(&t.app, &[gone]).await.unwrap();
    let err = rbb::ops::merge_threads(&t.app, keep, gone, None).await;
    let msg = format!("{:?}", err.as_ref().err());
    assert!(err.is_err() && msg.contains("merged or deleted"), "{msg}");
    assert_consistent(&t).await;
}

#[tokio::test]
async fn new_thread_in_a_category_lands_on_the_category() {
    let t = test_app!();
    let cat: i32 =
        sqlx::query_scalar("SELECT fid FROM forums WHERE type = 'c' ORDER BY fid LIMIT 1")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    let c = t.login_as(1).await;
    let r = c.get(&format!("/newthread/{cat}")).await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    assert!(
        r.location().starts_with(&format!("/forum/{cat}")),
        "{}",
        r.location()
    );
}
