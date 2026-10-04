mod common;

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
