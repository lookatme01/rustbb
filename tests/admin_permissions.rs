//! Admin Permissions: a module-restricted administrator must not be able to mint new administrators.

mod common;

use common::TestApp;
use rbb::util::now;

async fn restricted_admin(t: &TestApp) -> common::Client {
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO adminoptions (uid, permissions) VALUES (1, '{\"users\": false, \"groups\": true}')")
        .execute(&t.db.pool)
        .await
        .unwrap();
    c
}

async fn cancp(t: &TestApp, gid: i32) -> bool {
    sqlx::query_scalar("SELECT COALESCE((perms->>'cancp')::bool, false) FROM usergroups WHERE gid = $1")
        .bind(gid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_restricted_admin_cannot_grant_admin_cp_access_to_a_group() {
    let t = test_app!();
    let c = restricted_admin(&t).await;
    let r = c
        .post_form(
            "/admin/groups/edit",
            &[("gid", "2"), ("title", "Registered"), ("cancp", "1")],
        )
        .await;
    assert!(r.body.contains("without Admin Permissions restrictions"), "{}", r.status);
    assert!(!cancp(&t, 2).await);
    // A brand-new group is refused the same way.
    c.post_form(
        "/admin/groups/edit",
        &[("gid", "0"), ("title", "Sneaky"), ("cancp", "1")],
    )
    .await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usergroups WHERE title = 'Sneaky'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn an_unrestricted_admin_can_grant_admin_cp_access_to_a_group() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    c.post_form(
        "/admin/groups/edit",
        &[("gid", "6"), ("title", "Moderators"), ("cancp", "1")],
    )
    .await;
    assert!(cancp(&t, 6).await);
}
