//! Guest page cache: a page rendered with permissions that were revoked while it was in flight
//! must never be stored for later guests.

mod common;

use common::TestApp;

#[tokio::test]
async fn a_guest_page_rendered_before_access_was_revoked_is_not_cached() {
    let Some(t) = TestApp::with_config(|c| c.page_cache_mb = 8).await else {
        return;
    };
    // The page is cacheable for guests to begin with.
    let warm = t.client().get("/forum/2").await;
    assert_eq!(warm.status, 200);
    assert_eq!(
        warm.headers
            .get("x-rbb-cache")
            .and_then(|v| v.to_str().ok()),
        Some("miss")
    );
    t.app.page_cache.clear();

    // Pause a guest request right after it has taken its permission snapshot: an unknown auth
    // cookie makes it look the login up, and that lookup waits on a lock on `logins`.
    let mut lock = t.db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE logins IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let guest = t.client();
    guest.set_cookie(rbb::ctx::AUTH_COOKIE, "not-a-valid-login-token");
    let paused = tokio::spawn(async move { guest.get("/forum/2").await });
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation
             WHERE c.relname = 'logins' AND NOT l.granted",
        )
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }

    // Revoke guest access to the forum while that request is paused, then let it finish.
    sqlx::query(
        "INSERT INTO forumpermissions (fid, gid, perms) VALUES (2, 1, '{\"canview\":false}')
         ON CONFLICT (fid, gid) DO UPDATE SET perms = forumpermissions.perms || EXCLUDED.perms",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["forumperms"]).await.unwrap();
    lock.commit().await.unwrap();
    let stale = paused.await.unwrap();
    assert_eq!(
        stale.status, 200,
        "the paused request still used its earlier snapshot"
    );

    assert!(
        t.app.page_cache.is_empty(),
        "a page rendered before the revocation was cached"
    );
    let after = t.client().get("/forum/2").await;
    assert_ne!(
        after
            .headers
            .get("x-rbb-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit")
    );
    assert_ne!(after.status, 200, "guests can no longer see the forum");
}
