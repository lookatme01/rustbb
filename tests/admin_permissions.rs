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
    sqlx::query_scalar(
        "SELECT COALESCE((perms->>'cancp')::bool, false) FROM usergroups WHERE gid = $1",
    )
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
    assert!(
        r.body.contains("without Admin Permissions restrictions"),
        "{}",
        r.status
    );
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
async fn a_restricted_admin_cannot_route_members_into_an_admin_group() {
    let t = test_app!();
    let c = restricted_admin(&t).await;
    t.create_user("alt", "Passw0rd-alt-account").await;
    sqlx::query("UPDATE adminoptions SET permissions = '{\"users\": false, \"groups\": true, \"promotions\": true}' WHERE uid = 1")
        .execute(&t.db.pool)
        .await
        .unwrap();
    // Group leaders: adding one also places the account in the group.
    let r = c
        .post_form("/admin/groups/4/leaders", &[("username", "alt")])
        .await;
    assert!(
        r.body.contains("without Admin Permissions restrictions"),
        "{}",
        r.status
    );
    let gid: i32 = sqlx::query_scalar("SELECT usergroup FROM users WHERE username = 'alt'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_ne!(gid, 4);
    // Promotions: the background task would move matching members into the target group.
    let r = c
        .post_form(
            "/admin/promotions/edit",
            &[
                ("pid", "0"),
                ("title", "Sneaky"),
                ("req_posts", "1"),
                ("op_posts", ">="),
                ("val_posts", "0"),
                ("newusergroup", "4"),
            ],
        )
        .await;
    assert!(
        r.body.contains("without Admin Permissions restrictions"),
        "{}",
        r.status
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM promotions WHERE title = 'Sneaky'")
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

#[tokio::test]
async fn only_an_unrestricted_admin_can_download_a_database_backup() {
    let t = test_app!();
    let c = restricted_admin(&t).await;
    sqlx::query(
        "UPDATE adminoptions SET permissions = '{\"users\": false, \"tools\": true}' WHERE uid = 1",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    let r = c.post_form("/admin/tools/backup", &[]).await;
    assert!(
        r.body.contains("without Admin Permissions restrictions"),
        "{} {}",
        r.status,
        r.body
    );
}

#[tokio::test]
async fn ban_filters_and_other_select_columns_save_through_the_generic_editor() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let r = c
        .post_form(
            "/admin/crud/banfilters/save",
            &[("filter", "203.0.113.7"), ("type", "1")],
        )
        .await;
    let msg = r
        .body
        .split("<main")
        .nth(1)
        .unwrap_or("")
        .chars()
        .take(300)
        .collect::<String>();
    assert!(r.status.is_redirection(), "{} {msg}", r.status);
    let (filter, kind): (String, i16) = sqlx::query_as("SELECT filter, type FROM banfilters")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!((filter.as_str(), kind), ("203.0.113.7", 1));
}

/// Every field of a settings group at its default, with `overrides` applied, as the form posts them.
fn settings_form(group: &str, overrides: &[(&str, &str)]) -> Vec<(String, String)> {
    rbb::settings::DEFS
        .iter()
        .filter(|d| d.group == group && d.kind != "password")
        .map(|d| {
            let v = overrides
                .iter()
                .find(|(k, _)| *k == d.name)
                .map(|(_, v)| v.to_string())
                .unwrap_or_else(|| d.default.to_string());
            (d.name.to_string(), v)
        })
        .collect()
}

async fn setting_value(t: &TestApp, name: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM settings WHERE name = $1")
        .bind(name)
        .fetch_optional(&t.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_rejected_settings_save_writes_nothing_and_absurd_values_are_refused() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let before = setting_value(&t, "postsperpage").await;
    for bad in ["0", "-3", "100000"] {
        let r = post_settings(
            &c,
            "showthread",
            settings_form("showthread", &[("postsperpage", bad)]),
        )
        .await;
        assert!(
            r.body.contains("must be between"),
            "{bad}: {} {}",
            r.status,
            r.body
        );
        assert_eq!(setting_value(&t, "postsperpage").await, before, "{bad}");
    }
    let r = post_settings(
        &c,
        "showthread",
        settings_form("showthread", &[("postsperpage", "25")]),
    )
    .await;
    assert!(r.status.is_redirection(), "{}", r.status);
    assert_eq!(
        setting_value(&t, "postsperpage").await.as_deref(),
        Some("25")
    );
}

#[tokio::test]
async fn an_admin_cannot_require_two_factor_before_having_it() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let fields = settings_form("security", &[("acp2fa", "1")]);
    let refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let r = c.post_form("/admin/settings/security", &refs).await;
    assert!(r.body.contains("two-factor"), "{} {}", r.status, r.body);
    assert_ne!(setting_value(&t, "acp2fa").await.as_deref(), Some("1"));
}

async fn post_settings(
    c: &common::Client,
    group: &str,
    fields: Vec<(String, String)>,
) -> common::Response {
    let refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    c.post_form(&format!("/admin/settings/{group}"), &refs)
        .await
}

#[tokio::test]
async fn deleting_a_forum_takes_its_moderators_out_of_the_moderators_group() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO forums (fid, name, description, type, pid, parentlist, disporder) VALUES (40, 'Doomed', '', 'f', 0, '{40}', 40), (41, 'Kept', '', 'f', 0, '{41}', 41)")
        .execute(&t.db.pool)
        .await
        .unwrap();
    let only = t.create_user("onlymod", "Passw0rd-onlymod").await;
    let both = t.create_user("bothmod", "Passw0rd-bothmod").await;
    for uid in [only, both] {
        sqlx::query("UPDATE users SET additionalgroups = '{6}' WHERE uid = $1")
            .bind(uid)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES (40, $1, FALSE, '{}'), (40, $2, FALSE, '{}'), (41, $2, FALSE, '{}')")
        .bind(only)
        .bind(both)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums", "moderators"]).await.unwrap();
    let r = c.post_form("/admin/forums/delete", &[("fid", "40")]).await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    // The purge runs in the background.
    for _ in 0..100 {
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM forums WHERE fid = 40")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        if left == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let groups = |uid: i32| {
        let pool = t.db.pool.clone();
        async move {
            sqlx::query_scalar::<_, Vec<i32>>("SELECT additionalgroups FROM users WHERE uid = $1")
                .bind(uid)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(groups(only).await.is_empty(), "no forum left to moderate");
    assert_eq!(
        groups(both).await,
        vec![6],
        "still moderates the other forum"
    );
}

#[tokio::test]
async fn a_restricted_admin_cannot_reset_the_security_of_staff_accounts() {
    let t = test_app!();
    let c = restricted_admin(&t).await;
    // "users" stays allowed so the module is reachable; another module is what restricts them.
    sqlx::query(
        "UPDATE adminoptions SET permissions = '{\"users\": true, \"tools\": false}' WHERE uid = 1",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    let moderator = t.create_user("modwith2fa", "Passw0rd-modwith2fa").await;
    sqlx::query("UPDATE users SET usergroup = 6, totp_secret = 'JBSWY3DPEHPK3PXP' WHERE uid = $1")
        .bind(moderator)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let r = c
        .post_form(
            &format!("/admin/users/{moderator}/security"),
            &[("reset2fa", "1")],
        )
        .await;
    assert!(r.body.contains("staff account"), "{} {}", r.status, r.body);
    let secret: String = sqlx::query_scalar("SELECT totp_secret FROM users WHERE uid = $1")
        .bind(moderator)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(!secret.is_empty(), "the second factor was removed");
}

#[tokio::test]
async fn the_generic_editor_refuses_numbers_that_do_not_fit_instead_of_wrapping_them() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let save = |order: &'static str| {
        let c = &c;
        async move {
            c.post_form(
                "/admin/crud/smilies/save",
                &[
                    ("name", "grin"),
                    ("find", ":D"),
                    ("image", "/static/grin.svg"),
                    ("disporder", order),
                ],
            )
            .await
        }
    };
    // 4294967297 used to become 1 through `as i32`.
    let r = save("4294967297").await;
    assert!(r.body.contains("whole number"), "{} {}", r.status, r.body);
    let r = save("7").await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let order: i32 = sqlx::query_scalar("SELECT disporder FROM smilies WHERE name = 'grin'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(order, 7);
}

#[tokio::test]
async fn merging_users_leaves_no_self_reputation_and_one_vote_and_rating_per_person() {
    let t = test_app!();
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let src = t.create_user("mergesrc", "Passw0rd-mergesrc").await;
    let dst = t.create_user("mergedst", "Passw0rd-mergedst").await;
    let fid: i32 = sqlx::query_scalar("SELECT MIN(fid) FROM forums WHERE type = 'f'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible, numratings, totalratings) VALUES ($1, 'Merged', 1, 'admin', 1, 1, 1, 2, 9) RETURNING tid",
    )
    .bind(fid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let poll: i32 = sqlx::query_scalar(
        "INSERT INTO polls (tid, question, dateline, options, votes, numvotes) VALUES ($1, 'Q', 1, '{A,B}', '{1,1}', 2) RETURNING pid",
    )
    .bind(tid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    for (uid, option) in [(src, 1), (dst, 2)] {
        sqlx::query(
            "INSERT INTO pollvotes (pid, uid, voteoption, dateline) VALUES ($1, $2, $3, 1)",
        )
        .bind(poll)
        .bind(uid)
        .bind(option)
        .execute(&t.db.pool)
        .await
        .unwrap();
    }
    for (uid, rating) in [(src, 4), (dst, 5)] {
        sqlx::query("INSERT INTO threadratings (tid, uid, rating) VALUES ($1, $2, $3)")
            .bind(tid)
            .bind(uid)
            .bind(rating)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO reputation (uid, adduid, reputation, dateline) VALUES ($1, $2, 1, 1), ($2, $1, 1, 1)")
        .bind(src)
        .bind(dst)
        .execute(&t.db.pool)
        .await
        .unwrap();
    // A post of the source's in a soft-deleted thread: it must not be counted for the destination.
    let hidden: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, 'Hidden', $2, 'mergesrc', 1, 1, -1) RETURNING tid",
    )
    .bind(fid)
    .bind(src)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible) VALUES ($1, $2, 'Hidden', $3, 'mergesrc', 1, 'text', 1)")
        .bind(hidden)
        .bind(fid)
        .bind(src)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let r = c
        .post_form(
            "/admin/users/merge",
            &[("source", "mergesrc"), ("destination", "mergedst")],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let (votes, numvotes): (Vec<i32>, i32) =
        sqlx::query_as("SELECT votes, numvotes FROM polls WHERE pid = $1")
            .bind(poll)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((votes, numvotes), (vec![0, 1], 1));
    let (n, total): (i32, i32) =
        sqlx::query_as("SELECT numratings, totalratings FROM threads WHERE tid = $1")
            .bind(tid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((n, total), (1, 5));
    let self_rep: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reputation WHERE uid = adduid")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(self_rep, 0);
    let user_problems: Vec<String> = rbb::ops::check_counters(&t.db.pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.starts_with("user "))
        .collect();
    assert!(user_problems.is_empty(), "{user_problems:?}");
}
