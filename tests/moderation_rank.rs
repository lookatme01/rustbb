//! Staff rank: a moderator cannot use warnings or bans to act on, or undo what happened to,
//! someone they could not act on directly.

mod common;

use common::TestApp;

async fn setup(t: &TestApp) -> (i32, i32, i32) {
    for (k, v) in [
        ("enablewarningsystem", "1"),
        ("allowcustomwarnings", "1"),
        ("allowwarningsnopost", "1"),
        ("maxwarningpoints", "10"),
    ] {
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
            .bind(k)
            .bind(v)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO usergroups (gid, title, perms) VALUES (30, 'Warners', '{\"canwarnusers\":true,\"canmodcp\":true,\"canbanusers\":true,\"canreceivewarnings\":true}'), (31, 'Supers', '{\"issupermod\":true,\"canmodcp\":true,\"canreceivewarnings\":true}')")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings", "groups"]).await.unwrap();
    let warner = t.create_user("warner", "Passw0rd-warner").await;
    let sup = t.create_user("supermod", "Passw0rd-supermod").await;
    let plain = t.create_user("plainuser", "Passw0rd-plain").await;
    for (uid, gid) in [(warner, 30), (sup, 31)] {
        sqlx::query("UPDATE users SET usergroup = $2 WHERE uid = $1")
            .bind(uid)
            .bind(gid)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    (warner, sup, plain)
}

async fn points(t: &TestApp, uid: i32) -> i32 {
    sqlx::query_scalar("SELECT warningpoints FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn warnings_respect_staff_rank_and_cannot_be_revoked_by_their_recipient() {
    let t = test_app!();
    let (warner, sup, plain) = setup(&t).await;
    let c = t.login_as(warner).await;
    let r = warn_user(&c, sup).await;
    assert!(r.body.contains("cannot warn"), "{} {}", r.status, r.body);
    assert_eq!(points(&t, sup).await, 0);
    assert!(warn_user(&c, plain).await.status.is_redirection());
    assert_eq!(points(&t, plain).await, 2);
    // A warning the moderator received from an admin: they cannot revoke it themselves.
    sqlx::query("UPDATE users SET warningpoints = 3 WHERE uid = $1")
        .bind(warner)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let wid: i32 = sqlx::query_scalar(
        "INSERT INTO warnings (uid, title, points, dateline, issuedby) VALUES ($1, 'Mine', 3, 1, 1) RETURNING wid",
    )
    .bind(warner)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let r = c
        .post_form(&format!("/warning/{wid}/revoke"), &[("reason", "oops")])
        .await;
    assert!(r.body.contains("cannot revoke"), "{} {}", r.status, r.body);
    assert_eq!(points(&t, warner).await, 3);
}

#[tokio::test]
async fn rebanning_a_banned_admin_is_refused() {
    let t = test_app!();
    let (warner, _, _) = setup(&t).await;
    let admin = t.create_user("bannedadmin", "Passw0rd-bannedadmin").await;
    sqlx::query("UPDATE users SET usergroup = 7 WHERE uid = $1")
        .bind(admin)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO banned (uid, gid, oldgroup, admin, dateline, lifted, reason) VALUES ($1, 7, 4, 1, 1, 0, 'permanent')")
        .bind(admin)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(warner).await;
    let r = c
        .post_form(
            "/modcp/ban",
            &[
                ("username", "bannedadmin"),
                ("reason", "shortened"),
                ("days", "1"),
                ("gid", "7"),
            ],
        )
        .await;
    assert!(r.body.contains("cannot ban"), "{} {}", r.status, r.body);
    let (reason, lifted): (String, i64) =
        sqlx::query_as("SELECT reason, lifted FROM banned WHERE uid = $1")
            .bind(admin)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((reason.as_str(), lifted), ("permanent", 0));
}

async fn warn_user(c: &common::Client, uid: i32) -> common::Response {
    c.post_form(
        &format!("/warnings/{uid}/warn"),
        &[("custom_reason", "Rude"), ("custom_points", "2")],
    )
    .await
}
