//! Forum access and staff capabilities, end to end: what is hidden stays hidden everywhere.

mod common;

use common::TestApp;

#[tokio::test]
async fn saved_searches_and_subscriptions_recheck_thread_visibility() {
    let t = test_app!();
    let (tid, _) = hidden_subforum(&t).await;
    let viewer = t.create_user("searcher", "Passw0rd-searcher").await;
    let c = t.login_as(viewer).await;
    let pid: i32 = sqlx::query_scalar("SELECT firstpost FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    for (sid, kind, id) in [
        ("savedthreads", "threads", tid),
        ("savedposts", "posts", pid),
    ] {
        sqlx::query("INSERT INTO searchlog (sid, uid, resulttype, ids, keywords, dateline) VALUES ($1, $2, $3, $4, '', 1)")
            .bind(sid).bind(viewer).bind(kind).bind(vec![id]).execute(&t.db.pool).await.unwrap();
    }
    sqlx::query("INSERT INTO threadsubscriptions (uid, tid, dateline) VALUES ($1, $2, 1)")
        .bind(viewer)
        .bind(tid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let paths = [
        "/search/results/savedthreads",
        "/search/results/savedposts",
        "/usercp/subscriptions",
    ];
    for path in paths {
        let r = c.get(path).await;
        assert_eq!(r.status, 200, "{path}");
        assert!(r.body.contains("Zebracorn sightings"), "{path}");
    }
    // A thread can be unapproved while its individual posts remain approved.
    for state in [0i16, -1] {
        sqlx::query("UPDATE threads SET visible = $2 WHERE tid = $1")
            .bind(tid)
            .bind(state)
            .execute(&t.db.pool)
            .await
            .unwrap();
        for path in paths {
            let r = c.get(path).await;
            assert_eq!(r.status, 200, "{path}");
            assert!(
                !r.body.contains("Zebracorn sightings"),
                "hidden thread leaked at {path}"
            );
            assert!(
                !r.body.contains("A zebracorn was seen today"),
                "hidden post leaked at {path}"
            );
        }
        let admin = t.login_as(1).await;
        // Search ownership is separate from visibility; use an admin-owned stored search.
        sqlx::query("UPDATE searchlog SET uid = 1")
            .execute(&t.db.pool)
            .await
            .unwrap();
        assert!(
            admin
                .get(paths[0])
                .await
                .body
                .contains("Zebracorn sightings")
        );
        sqlx::query("UPDATE searchlog SET uid = $1")
            .bind(viewer)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn reactions_on_hidden_posts_are_not_public() {
    let t = test_app!();
    let (tid, uid) = hidden_subforum(&t).await;
    let pid: i32 = sqlx::query_scalar("SELECT firstpost FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE posts SET visible = -1 WHERE pid = $1")
        .bind(pid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    assert_eq!(c.get(&format!("/post/{pid}/reactions")).await.status, 404);
    assert_eq!(
        t.login_as(1)
            .await
            .get(&format!("/post/{pid}/reactions"))
            .await
            .status,
        200
    );
}

/// A subforum (fid 8) under General Discussion (3) with one thread containing "zebracorn".
async fn hidden_subforum(t: &TestApp) -> (i32, i32) {
    sqlx::query(
        "INSERT INTO forums (fid, name, description, type, pid, parentlist, disporder) VALUES (8, 'Inner Sanctum', '', 'f', 3, '{1,3,8}', 9)",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    let uid = t.create_user("insider", "Passw0rd-insider").await;
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES (8, 'Zebracorn sightings', $1, 'insider', 100, 100, 1) RETURNING tid",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible) VALUES ($1, 8, 'Zebracorn sightings', $2, 'insider', 100, 'A zebracorn was seen today', 1) RETURNING pid",
    )
    .bind(tid)
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE threads SET firstpost = $2 WHERE tid = $1")
        .bind(tid)
        .bind(pid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    rbb::ops::rebuild_all_counters(&t.app).await.unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    (tid, uid)
}

async fn assert_hidden_everywhere(t: &TestApp, c: &common::Client, tid: i32) {
    let r = c.get("/forum/8").await;
    assert!(r.status.is_client_error(), "forum page: {}", r.status);
    let r = c.get(&format!("/thread/{tid}")).await;
    assert!(r.status.is_client_error(), "thread page: {}", r.status);
    let r = c.get("/api/v1/search?q=zebracorn").await;
    assert!(!r.body.contains("Zebracorn"), "API search: {}", r.body);
    let r = c.get("/api/v1/forums").await;
    assert!(
        !r.body.contains("Inner Sanctum"),
        "API forum list: {}",
        r.body
    );
    let r = c.get("/syndication").await;
    assert!(!r.body.contains("Zebracorn"), "feed");
    let r = c.get("/sitemap.xml").await;
    assert!(!r.body.contains(&format!("/thread/{tid}")), "sitemap");
    let r = c.get("/portal").await;
    assert!(!r.body.contains("Zebracorn"), "portal");
    let r = c.get("/archive").await;
    assert!(!r.body.contains("Inner Sanctum"), "archive");
    let _ = t;
}

#[tokio::test]
async fn password_on_an_ancestor_hides_descendants_until_unlocked() {
    let t = test_app!();
    let (tid, _) = hidden_subforum(&t).await;
    let hash = rbb::auth::hash_password("open sesame").await.unwrap();
    sqlx::query(
        "UPDATE forums SET password = $1, password_version = password_version + 1 WHERE fid = 3",
    )
    .bind(hash)
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    let c = t.client();
    assert_hidden_everywhere(&t, &c, tid).await;
    // A wrong password does nothing; the right one unlocks the forum and its subforums.
    let page = c.get("/forum/3").await;
    let key = common::form_key(&page.body);
    let r = c
        .post_form(
            "/forum/3/password",
            &[("my_post_key", &key), ("password", "wrong")],
        )
        .await;
    assert!(c.cookie("forumpass_3").is_none(), "{}", r.status);
    c.post_form(
        "/forum/3/password",
        &[("my_post_key", &key), ("password", "open sesame")],
    )
    .await;
    assert!(c.cookie("forumpass_3").is_some());
    let r = c.get(&format!("/thread/{tid}")).await;
    assert_eq!(r.status, 200);
    // Changing the password signs everyone out, whatever the new password is.
    let hash = rbb::auth::hash_password("open sesame").await.unwrap();
    sqlx::query(
        "UPDATE forums SET password = $1, password_version = password_version + 1 WHERE fid = 3",
    )
    .bind(hash)
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    let r = c.get(&format!("/thread/{tid}")).await;
    assert!(
        r.status.is_client_error(),
        "old unlock cookie still works: {}",
        r.status
    );
}

#[tokio::test]
async fn inactive_ancestor_hides_descendants() {
    let t = test_app!();
    let (tid, _) = hidden_subforum(&t).await;
    sqlx::query("UPDATE forums SET active = FALSE WHERE fid = 3")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    let c = t.client();
    assert_hidden_everywhere(&t, &c, tid).await;
}

#[tokio::test]
async fn plaintext_forum_passwords_are_hashed_on_upgrade() {
    let t = test_app!();
    sqlx::query("UPDATE forums SET password = 'legacy-plain' WHERE fid = 4")
        .execute(&t.db.pool)
        .await
        .unwrap();
    rbb::install::upgrade(&t.db.pool).await.unwrap();
    let (pw, version): (String, i32) =
        sqlx::query_as("SELECT password, password_version FROM forums WHERE fid = 4")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert!(pw.starts_with("$argon2id$"), "{pw}");
    assert!(version > 0);
    assert!(rbb::auth::verify_password("legacy-plain", &pw).await);
}

#[tokio::test]
async fn forum_moderators_are_limited_to_their_forums() {
    let t = test_app!();
    let mod_uid = t.create_user("forummod", "Passw0rd-forummod").await;
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES (4, $1, FALSE, '{}')")
        .bind(mod_uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let victim = t.create_user("member", "Passw0rd-member").await;
    sqlx::query("INSERT INTO moderator_notes (uid, author, note, created) VALUES ($1, 1, 'secret staff note', 1)")
        .bind(victim)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let pm_rid: i32 = sqlx::query_scalar(
        "INSERT INTO reportedcontent (id, id2, uid, type, reason, dateline, lastreport) VALUES (1, $1, $1, 'pm', 'private message text', 1, 1) RETURNING rid",
    )
    .bind(victim)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let c = t.login_as(mod_uid).await;
    // Mod CP access, but no board-wide staff features.
    assert_eq!(c.get("/modcp").await.status, 200);
    let h = c.get(&format!("/modcp/member/{victim}")).await;
    assert_eq!(h.status, 200);
    assert!(
        !h.body.contains("secret staff note"),
        "notes leaked to a forum moderator"
    );
    let r = c.get(&format!("/modcp/reports/{pm_rid}")).await;
    assert!(
        r.status.is_client_error(),
        "PM report visible: {}",
        r.status
    );
    let r = c.get("/modcp/reports").await;
    assert!(!r.body.contains("private message text"));
    // The administrator sees both.
    let admin = t.login_as(1).await;
    assert!(
        admin
            .get(&format!("/modcp/member/{victim}"))
            .await
            .body
            .contains("secret staff note")
    );
    assert_eq!(
        admin.get(&format!("/modcp/reports/{pm_rid}")).await.status,
        200
    );
}
