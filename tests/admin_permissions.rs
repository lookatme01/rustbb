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
