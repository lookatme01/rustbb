//! Regression checks for batched recounts and indexable title-search predicates.

mod common;

#[tokio::test]
async fn batched_rebuild_preserves_first_last_visibility_and_zero_counts() {
    let t = test_app!();
    // More than one rebuild batch; timestamp ties exercise the pid tie-breaker.
    let tids: Vec<i32> = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, dateline, lastpost, visible, replies, unapprovedposts, deletedposts)
         SELECT 3, 'Rebuild ' || g, 1, 900, 900, (g % 3 - 1)::smallint, 99, 99, 99
         FROM generate_series(1, 505) g RETURNING tid",
    )
    .fetch_all(&t.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO posts (tid, fid, uid, username, dateline, visible, message)
         SELECT t.tid, 3, 1, CASE WHEN g = 1 THEN 'first' ELSE 'last' END,
                CASE WHEN g <= 2 THEN 100 ELSE g * 100 END,
                CASE WHEN g = 1 THEN t.visible WHEN g = 2 THEN 1 WHEN g = 3 THEN 0 ELSE -1 END, 'message'
         FROM threads t CROSS JOIN generate_series(1, 4) g WHERE t.tid = ANY($1)
         ORDER BY t.tid, g",
    )
    .bind(&tids)
    .execute(&t.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO attachments (pid, uid, filename, filetype, filesize, attachname, dateuploaded, visible)
         SELECT p.pid, 1, 'file', 'text/plain', 1, '', 100, v
         FROM posts p CROSS JOIN unnest(ARRAY[true, false]) v
         WHERE p.tid = ANY($1) AND p.username = 'last'",
    )
    .bind(&tids)
    .execute(&t.db.pool)
    .await
    .unwrap();
    let empty_uid: i32 = sqlx::query_scalar(
        "INSERT INTO users (username, password, email, usergroup, postnum, threadnum, regdate)
         VALUES ('no-content', '', 'empty@example.test', 2, 99, 99, 1) RETURNING uid",
    )
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE forums SET lastpost = 999, lastposter = 'stale', lastposteruid = 1, lastposttid = 1, lastpostsubject = 'stale' WHERE fid = 4")
        .execute(&t.db.pool).await.unwrap();
    rbb::ops::rebuild_all_counters(&t.app).await.unwrap();
    let rows: Vec<(i32, i32, i32, String, i64, i64, String, i32)> = sqlx::query_as(
        "SELECT replies, unapprovedposts, deletedposts, username, dateline, lastpost, lastposter, attachmentcount
         FROM threads WHERE tid = ANY($1)",
    )
    .bind(&tids)
    .fetch_all(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 505);
    for row in rows {
        assert_eq!(row, (1, 1, 1, "first".into(), 100, 100, "last".into(), 1));
    }
    let empty: (i32, i32) = sqlx::query_as("SELECT postnum, threadnum FROM users WHERE uid = $1")
        .bind(empty_uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(empty, (0, 0));
    let forum: (i64, String, i32, i32, String) = sqlx::query_as(
        "SELECT lastpost, lastposter, lastposteruid, lastposttid, lastpostsubject FROM forums WHERE fid = 4",
    ).fetch_one(&t.db.pool).await.unwrap();
    assert_eq!(forum, (0, "".into(), 0, 0, "".into()));
    assert!(
        rbb::ops::check_counters(&t.db.pool)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn recount_without_public_posts_uses_the_first_post_and_leaves_empty_threads_alone() {
    let t = test_app!();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, dateline, visible) VALUES (3, 'Pending', 900, 0) RETURNING tid",
    ).fetch_one(&t.db.pool).await.unwrap();
    let first: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, fid, uid, username, dateline, visible, message)
         VALUES ($1, 3, 1, 'pending author', 100, 0, 'first') RETURNING pid",
    )
    .bind(tid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO posts (tid, fid, dateline, visible, message) VALUES ($1, 3, 200, -1, 'deleted')")
        .bind(tid).execute(&t.db.pool).await.unwrap();
    let empty: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, dateline, lastpost, replies) VALUES (3, 'Empty', 900, 900, 7) RETURNING tid",
    ).fetch_one(&t.db.pool).await.unwrap();
    let mut conn = t.db.pool.acquire().await.unwrap();
    rbb::ops::recount_thread(&mut conn, tid).await.unwrap();
    rbb::ops::recount_thread(&mut conn, empty).await.unwrap();
    let row: (i32, i32, i32, i32, i64, String, i32) = sqlx::query_as(
        "SELECT firstpost, replies, unapprovedposts, deletedposts, lastpost, lastposter, lastposteruid FROM threads WHERE tid = $1",
    ).bind(tid).fetch_one(&t.db.pool).await.unwrap();
    assert_eq!(row, (first, 0, 0, 1, 100, "pending author".into(), 1));
    let replies: i32 = sqlx::query_scalar("SELECT replies FROM threads WHERE tid = $1")
        .bind(empty)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(replies, 7);
}

#[tokio::test]
async fn title_search_keeps_all_words_literal_wildcards_and_author_only_search() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query(
        "INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES
        (3, 'Zebracorn 100% under_score', 1, 100, 100),
        (3, 'Zebracorn 1000 underXscore', 1, 200, 200),
        (3, 'Zebracorn incomplete', 1, 300, 300)",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    let r = c
        .post_form(
            "/search",
            &[
                ("keywords", "Zebracorn 100% under_score"),
                ("postthread", "2"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{}", r.body);
    let ids: Vec<i32> = sqlx::query_scalar("SELECT ids FROM searchlog WHERE sid = $1")
        .bind(r.location().rsplit('/').next().unwrap())
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let subject: String = sqlx::query_scalar("SELECT subject FROM threads WHERE tid = $1")
        .bind(ids[0])
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(subject, "Zebracorn 100% under_score");
    let r = c
        .post_form(
            "/search",
            &[
                ("author", "admin"),
                ("matchusername", "1"),
                ("postthread", "2"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{}", r.body);
}

#[tokio::test]
async fn portal_merges_top_announcements_across_forums_before_limiting() {
    let t = test_app!();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('portal', '1'), ('portal_announcementsfid', '3,4'), ('portal_numannouncements', '3'), ('portal_showdiscussions', '0') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    for (fid, stamp, name) in [
        (3, 100, "Oldest"),
        (4, 200, "Second"),
        (3, 300, "Third"),
        (4, 400, "Newest"),
    ] {
        let tid: i32 = sqlx::query_scalar("INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES ($1, $2, 1, $3, $3) RETURNING tid")
            .bind(fid).bind(name).bind(stamp as i64).fetch_one(&t.db.pool).await.unwrap();
        let pid: i32 = sqlx::query_scalar("INSERT INTO posts (tid, fid, uid, dateline, message) VALUES ($1, $2, 1, $3, $4) RETURNING pid")
            .bind(tid).bind(fid).bind(stamp as i64).bind(format!("Announcement body {name}")).fetch_one(&t.db.pool).await.unwrap();
        sqlx::query("UPDATE threads SET firstpost = $2 WHERE tid = $1")
            .bind(tid)
            .bind(pid)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    let r = t.client().get("/portal").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(!r.body.contains("Announcement body Oldest"));
    let newest = r.body.find("Announcement body Newest").unwrap();
    let third = r.body.find("Announcement body Third").unwrap();
    let second = r.body.find("Announcement body Second").unwrap();
    assert!(newest < third && third < second);
}

#[tokio::test]
async fn batched_quotes_still_deduplicate_recipients_respect_ignores_and_retry_safely() {
    let t = test_app!();
    let quoted = t.create_user("quoted-reader", "Passw0rd-reader").await;
    let ignored = t.create_user("ignoring-reader", "Passw0rd-reader").await;
    sqlx::query("UPDATE users SET ignorelist = ARRAY[1] WHERE uid = $1")
        .bind(ignored)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let mut pids = vec![];
    for uid in [quoted, quoted, ignored] {
        let pid: i32 = sqlx::query_scalar("INSERT INTO posts (tid, fid, uid, dateline, message) VALUES (1, 2, $1, 100, 'quoted post') RETURNING pid")
            .bind(uid).fetch_one(&t.db.pool).await.unwrap();
        pids.push(pid);
    }
    let message = format!(
        "[quote=reader pid={}]a[/quote][quote=reader pid={}]b[/quote][quote=reader pid={}]c[/quote]",
        pids[0], pids[1], pids[2]
    );
    for _ in 0..2 {
        rbb::notify::mentions_and_quotes(
            &t.app,
            rbb::infra::outbox::Delivery::new("batch-quote-test"),
            1,
            "admin",
            1,
            pids[0],
            "Thread",
            &message,
        )
        .await
        .unwrap();
    }
    let alerts: Vec<(i32, String)> =
        sqlx::query_as("SELECT uid, kind FROM alerts WHERE kind = 'quoted'")
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(alerts, vec![(quoted, "quoted".into())]);
}

#[tokio::test]
async fn admin_ip_tab_keeps_the_matches_for_each_address() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(rbb::util::now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let a = t.create_user("shared-address-one", "Passw0rd-reader").await;
    let b = t.create_user("shared-address-two", "Passw0rd-reader").await;
    sqlx::query("UPDATE users SET lastip = '203.0.113.1' WHERE uid = $1")
        .bind(a)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET regip = '203.0.113.2' WHERE uid = $1")
        .bind(b)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO posts (tid, fid, uid, dateline, message, ipaddress) VALUES (1, 2, 1, 100, 'one', '203.0.113.1'), (1, 2, 1, 200, 'two', '203.0.113.2')")
        .execute(&t.db.pool).await.unwrap();
    let r = c.get("/admin/users/1?tab=ips").await;
    assert_eq!(r.status, 200, "{}", r.body);
    for value in [
        "203.0.113.1",
        "203.0.113.2",
        "shared-address-one",
        "shared-address-two",
    ] {
        assert!(r.body.contains(value), "Missing {value}");
    }
}
