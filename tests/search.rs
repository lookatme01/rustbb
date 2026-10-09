mod common;

#[tokio::test]
async fn empty_search_cache_is_not_reused_after_unlocking_a_forum() {
    let t = test_app!();
    let uid = t.create_user("unlocksearch", "Passw0rd-search").await;
    let hash = rbb::auth::hash_password("open sesame").await.unwrap();
    sqlx::query(
        "UPDATE forums SET password = $1, password_version = password_version + 1 WHERE fid = 3",
    )
    .bind(hash)
    .execute(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('searchfloodtime', '0') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES (3, 'Zebracorn password search', $1, 100, 100)")
        .bind(uid).execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["forums", "settings"]).await.unwrap();
    let c = t.login_as(uid).await;
    let fields = [("keywords", "Zebracorn"), ("postthread", "2")];
    let r = c.post_form("/search", &fields).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("No results for"));
    let r = c
        .post_form("/forum/3/password", &[("password", "open sesame")])
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    assert!(c.cookie("forumpass_3").is_some());
    let r = c.post_form("/search", &fields).await;
    assert!(
        r.status.is_redirection(),
        "unlocked search reused empty results: {}",
        r.body
    );
    assert!(
        c.get(r.location())
            .await
            .body
            .contains("Zebracorn password search")
    );
}

#[tokio::test]
async fn full_text_thread_search_deduplicates_and_sorts_before_limiting() {
    let t = test_app!();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('searchhardlimit', '50') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let c = t.login_as(1).await;
    let mut tids = vec![];
    for (subject, lastpost, count) in [("Old match", 100, 1), ("Busy match", 200, 160)] {
        let tid: i32 = sqlx::query_scalar(
            "INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES (3, $1, 1, $2, $2) RETURNING tid",
        )
        .bind(subject)
        .bind(lastpost)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO posts (tid, fid, uid, message, dateline) SELECT $1, 3, 1, 'Zebracorn content', $2 FROM generate_series(1, $3::int)")
            .bind(tid).bind(lastpost).bind(count).execute(&t.db.pool).await.unwrap();
        tids.push(tid);
    }
    for (sortby, order, expected) in [
        ("lastpost", "asc", tids.clone()),
        ("", "asc", tids.clone()),
        ("lastpost", "desc", vec![tids[1], tids[0]]),
        ("subject", "asc", vec![tids[1], tids[0]]),
    ] {
        let r = c
            .post_form(
                "/search",
                &[
                    ("keywords", "Zebracorn"),
                    ("sortby", sortby),
                    ("sortordr", order),
                ],
            )
            .await;
        assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
        let sid = r.location().rsplit('/').next().unwrap();
        let ids: Vec<i32> = sqlx::query_scalar("SELECT ids FROM searchlog WHERE sid = $1")
            .bind(sid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(ids, expected, "{sortby} {order}");
    }
}

#[tokio::test]
async fn api_search_obeys_the_shared_search_concurrency_limit() {
    let t = test_app!();
    let permit = t.app.search_sem.acquire_many(8).await.unwrap();
    let c = t.client();
    let request = c.get("/api/v1/search?q=Zebracorn");
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut request)
            .await
            .is_err(),
        "API search ran while every search slot was occupied"
    );
    drop(permit);
    let r = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .unwrap();
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn api_search_times_out_and_recovers_when_posts_are_locked() {
    let t = test_app!();
    let mut lock = t.db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE posts IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let c = t.client();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        c.get("/api/v1/search?q=Zebracorn"),
    )
    .await;
    lock.rollback().await.unwrap();
    let r = response.expect("API search exceeded its statement timeout");
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(r.body.contains("search took too long"), "{}", r.body);
    let r = c.get("/api/v1/search?q=Zebracorn").await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn selected_forums_and_subforum_checkbox_apply_to_all_searchable_forums() {
    let t = test_app!();
    let uid = t.create_user("searcher", "Passw0rd-searcher").await;
    sqlx::query("INSERT INTO settings (name, value) VALUES ('searchfloodtime', '0') ON CONFLICT (name) DO UPDATE SET value = '0'")
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO forums (fid, name, type, pid, parentlist) VALUES (8, 'Child', 'f', 3, '{1,3,8}')")
        .execute(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES (4, 2, '{\"canonlyviewownthreads\":true}') ON CONFLICT (fid, gid) DO UPDATE SET perms = EXCLUDED.perms")
        .execute(&t.db.pool).await.unwrap();
    for fid in [3, 8, 4] {
        sqlx::query("INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES ($1, 'Zebracorn example', $2, 100, 100)")
            .bind(fid).bind(uid).execute(&t.db.pool).await.unwrap();
    }
    t.app
        .invalidate(&["forums", "forumperms", "settings"])
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    for (subforums, expected) in [("0", vec![3]), ("1", vec![3, 8])] {
        let r = c
            .post_form(
                "/search",
                &[
                    ("keywords", "Zebracorn"),
                    ("postthread", "2"),
                    ("forums", "3"),
                    ("subforums", subforums),
                ],
            )
            .await;
        assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
        let sid = r.location().rsplit('/').next().unwrap();
        let fids: Vec<i32> = sqlx::query_scalar("SELECT fid FROM threads WHERE tid = ANY((SELECT ids FROM searchlog WHERE sid = $1)::int[]) ORDER BY fid")
            .bind(sid).fetch_all(&t.db.pool).await.unwrap();
        assert_eq!(fids, expected);
    }
}

#[tokio::test]
async fn permission_dependent_feeds_cannot_be_cached_by_shared_proxies() {
    let t = test_app!();
    let c = t.login_as(1).await;
    // Exercise both the initial render and the server-side cache hit.
    for _ in 0..2 {
        let r = c.get("/syndication").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("cache-control").unwrap(), "private, no-store");
    }
}

#[tokio::test]
async fn failed_searches_come_back_to_a_filled_in_form() {
    let t = test_app!();
    let uid = t.create_user("finder", "Passw0rd-finder").await;
    let c = t.login_as(uid).await;
    // Nothing matches: the form comes back with the query and an empty state, not an error page.
    let r = c.get("/search/quick?q=Quuxbarnacle").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains(r#"value="Quuxbarnacle""#));
    assert!(r.body.contains("No results for “Quuxbarnacle”"));
    // Repeating it straight away is answered from the cache, not refused by the flood limit.
    let r = c.get("/search/quick?q=Quuxbarnacle").await;
    assert!(r.body.contains("No results for"), "{}", r.body);
    assert!(!r.body.contains("searching a little fast"));
    // A validation failure keeps the filters that were set.
    let r = c
        .post_form(
            "/search",
            &[("keywords", ""), ("author", ""), ("postthread", "2")],
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("You did not enter any search terms"));
    assert!(r.body.contains(r#"value="2" checked"#));
}

#[tokio::test]
async fn empty_thread_listings_render_a_friendly_page() {
    let t = test_app!();
    sqlx::query("UPDATE threads SET lastpost = 1, dateline = 1")
        .execute(&t.db.pool)
        .await
        .unwrap();
    let uid = t.create_user("caughtup", "Passw0rd-caughtup").await;
    let c = t.login_as(uid).await;
    for (path, text) in [
        ("/search/today", "Nobody has posted in the last 24 hours."),
        ("/search/unread", "all caught up."),
    ] {
        let r = c.get(path).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
        assert!(r.body.contains(text), "{path}");
        assert!(r.body.contains("Browse the forums"));
    }
}
