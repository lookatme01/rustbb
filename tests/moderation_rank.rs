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

#[tokio::test]
async fn a_warning_level_can_only_ban_into_a_banned_group() {
    let t = test_app!();
    let (warner, _, plain) = setup(&t).await;
    // A level saved with the Administrators group as its "ban" group.
    sqlx::query("INSERT INTO warninglevels (percentage, action) VALUES (10, '{\"type\":\"ban\",\"length\":0,\"usergroup\":4}')")
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(warner).await;
    let r = c
        .post_form(
            &format!("/warnings/{plain}/warn"),
            &[("custom_reason", "Rude"), ("custom_points", "10")],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let group: i32 = sqlx::query_scalar("SELECT usergroup FROM users WHERE uid = $1")
        .bind(plain)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(group, 7, "the member is banned, not promoted");
}

#[tokio::test]
async fn a_warning_can_only_cite_a_post_the_member_wrote() {
    let t = test_app!();
    let (warner, _, plain) = setup(&t).await;
    let fid: i32 = sqlx::query_scalar("SELECT MIN(fid) FROM forums WHERE type = 'f'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let other = t.create_user("someoneelse", "Passw0rd-someoneelse").await;
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, 'Elsewhere', $2, 'someoneelse', 1, 1, 1) RETURNING tid",
    )
    .bind(fid)
    .bind(other)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible) VALUES ($1, $2, 'Elsewhere', $3, 'someoneelse', 1, 'text', 1) RETURNING pid",
    )
    .bind(tid)
    .bind(fid)
    .bind(other)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let c = t.login_as(warner).await;
    let r = c
        .post_form(
            &format!("/warnings/{plain}/warn"),
            &[
                ("custom_reason", "Rude"),
                ("custom_points", "2"),
                ("pid", &pid.to_string()),
            ],
        )
        .await;
    assert!(
        r.body.contains("not written by this member"),
        "{} {}",
        r.status,
        r.body
    );
    assert_eq!(points(&t, plain).await, 0);
}

#[tokio::test]
async fn ip_search_lists_only_posts_the_moderator_may_see() {
    let t = test_app!();
    let (warner, _, plain) = setup(&t).await;
    sqlx::query(
        "UPDATE usergroups SET perms = perms || '{\"canuseipsearch\":true}'::jsonb WHERE gid = 30",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let fid: i32 = sqlx::query_scalar("SELECT MIN(fid) FROM forums WHERE type = 'f'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, 'Open thread', $2, 'plainuser', 1, 1, 1) RETURNING tid",
    )
    .bind(fid)
    .bind(plain)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    for (subject, visible) in [("Shown subject", 1), ("Removed subject", -1)] {
        sqlx::query("INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible, ipaddress) VALUES ($1, $2, $3, $4, 'plainuser', 1, 'text', $5, '203.0.113.5')")
            .bind(tid)
            .bind(fid)
            .bind(subject)
            .bind(plain)
            .bind(visible)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    let c = t.login_as(warner).await;
    let r = c.get("/modcp/ipsearch?ip=203.0.113.5").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("Shown subject"), "{}", r.body);
    assert!(
        !r.body.contains("Removed subject"),
        "a soft-deleted post's subject leaked"
    );
}

#[tokio::test]
async fn a_moderator_can_only_cancel_scheduled_actions_they_could_have_scheduled() {
    let t = test_app!();
    let fid: i32 = sqlx::query_scalar("SELECT MIN(fid) FROM forums WHERE type = 'f'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let author = t.create_user("delayauthor", "Passw0rd-delayauthor").await;
    let moderator = t.create_user("openclosemod", "Passw0rd-openclosemod").await;
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, FALSE, '{\"canopenclosethreads\":true,\"canmanagethreads\":false,\"candeletethreads\":false}')")
        .bind(fid)
        .bind(moderator)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, 'Scheduled', $2, 'delayauthor', 1, 1, 1) RETURNING tid",
    )
    .bind(fid)
    .bind(author)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let mut dids = vec![];
    for kind in ["delete", "close"] {
        let did: i32 = sqlx::query_scalar(
            "INSERT INTO delayedmoderation (type, delaydateline, uid, fid, tids, dateline) VALUES ($1, 4102444800, 1, $2, $3, 1) RETURNING did",
        )
        .bind(kind)
        .bind(fid)
        .bind(vec![tid])
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        dids.push(did);
    }
    let c = t.login_as(moderator).await;
    for did in &dids {
        c.post_form("/moderation/delayed/cancel", &[("did", &did.to_string())])
            .await;
    }
    let left: Vec<String> = sqlx::query_scalar("SELECT type FROM delayedmoderation ORDER BY did")
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(
        left,
        vec!["delete".to_string()],
        "only the close could be cancelled"
    );
}

#[tokio::test]
async fn moderator_assignments_can_withhold_report_handling_and_the_mod_log() {
    let t = test_app!();
    let fid: i32 = sqlx::query_scalar("SELECT MIN(fid) FROM forums WHERE type = 'f'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let limited = t.create_user("limitedstaff", "Passw0rd-limitedstaff").await;
    let full = t.create_user("fullstaff", "Passw0rd-fullstaff").await;
    for (uid, perms) in [
        (
            limited,
            "{\"canmanagereportedposts\":false,\"canviewmodlog\":false}",
        ),
        (full, "{}"),
    ] {
        sqlx::query(
            "INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, FALSE, $3::jsonb)",
        )
        .bind(fid)
        .bind(uid)
        .bind(perms)
        .execute(&t.db.pool)
        .await
        .unwrap();
    }
    t.app.invalidate(&["moderators"]).await.unwrap();
    for path in ["/modcp/reports", "/modcp/modlogs"] {
        let denied = t.login_as(limited).await.get(path).await;
        assert!(denied.status.is_client_error(), "{path}: {}", denied.status);
        let allowed = t.login_as(full).await.get(path).await;
        assert_eq!(allowed.status, 200, "{path}: {}", allowed.status);
    }
}

#[tokio::test]
async fn removing_redirects_only_touches_forums_the_moderator_manages() {
    let t = test_app!();
    let fids: Vec<i32> =
        sqlx::query_scalar("SELECT fid FROM forums WHERE type = 'f' ORDER BY fid LIMIT 2")
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    let (origin, home) = (fids[0], fids[1]);
    let author = t.create_user("redirauthor", "Passw0rd-redirauthor").await;
    let moderator = t.create_user("homemod", "Passw0rd-homemod").await;
    // Moderates only the forum the thread now lives in.
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, FALSE, '{}')")
        .bind(home)
        .bind(moderator)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, 'Moved here', $2, 'redirauthor', 1, 1, 1) RETURNING tid",
    )
    .bind(home)
    .bind(author)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let stub: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible, closed) VALUES ($1, 'Moved here', $2, 'redirauthor', 1, 1, 1, $3) RETURNING tid",
    )
    .bind(origin)
    .bind(author)
    .bind(format!("moved|{tid}"))
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let c = t.login_as(moderator).await;
    c.post_form(
        "/moderation/threads",
        &[("action", "removeredirects"), ("tids", &tid.to_string())],
    )
    .await;
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads WHERE tid = $1")
        .bind(stub)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(
        kept, 1,
        "the stub sits in a forum this moderator does not manage"
    );
    // Once they manage that forum too, the same request removes it.
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, FALSE, '{}')")
        .bind(origin)
        .bind(moderator)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let c = t.login_as(moderator).await;
    c.post_form(
        "/moderation/threads",
        &[("action", "removeredirects"), ("tids", &tid.to_string())],
    )
    .await;
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads WHERE tid = $1")
        .bind(stub)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(kept, 0);
}
