//! Badges: earned by the hourly task, awarded and revoked by staff, shown on posts and profiles.

mod common;

use common::TestApp;
use rbb::util::now;

const DAY: i64 = 86400;

async fn set(t: &TestApp, pairs: &[(&str, &str)]) {
    for (k, v) in pairs {
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
            .bind(k)
            .bind(v)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    t.app.invalidate(&["settings"]).await.unwrap();
}

async fn badge_id(t: &TestApp, name: &str) -> i32 {
    sqlx::query_scalar("SELECT bid FROM badges WHERE name = $1")
        .bind(name)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

async fn has(t: &TestApp, uid: i32, bid: i32) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_badges WHERE uid = $1 AND bid = $2)")
        .bind(uid)
        .bind(bid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

async fn member(t: &TestApp, name: &str, days_ago: i64, group: i32) -> i32 {
    let uid = t.create_user(name, "Passw0rd-badges").await;
    sqlx::query("UPDATE users SET regdate = $2, usergroup = $3 WHERE uid = $1")
        .bind(uid)
        .bind(now() - days_ago * DAY)
        .bind(group)
        .execute(&t.db.pool)
        .await
        .unwrap();
    uid
}

/// An administrator's client that has passed the Admin CP password check.
async fn admin(t: &TestApp) -> common::Client {
    let c = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    c
}

#[tokio::test]
async fn the_task_awards_service_badges_once_to_members_who_qualify() {
    let t = test_app!();
    let year = badge_id(&t, "1 Year of Service").await;
    let two = badge_id(&t, "2 Years of Service").await;
    let old = member(&t, "oldtimer", 400, 2).await;
    let new = member(&t, "newcomer", 10, 2).await;
    let banned = member(&t, "banned", 400, 7).await;
    let waiting = member(&t, "waiting", 400, 5).await;
    let system: i32 = sqlx::query_scalar("SELECT uid FROM users WHERE is_system")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET regdate = $1 WHERE is_system")
        .bind(now() - 400 * DAY)
        .execute(&t.db.pool)
        .await
        .unwrap();

    let msg = rbb::badges::run(&t.app).await.unwrap();
    assert!(msg.starts_with("awarded"), "{msg}");
    assert!(has(&t, old, year).await);
    assert!(!has(&t, old, two).await, "400 days is not two years");
    assert!(!has(&t, new, year).await);
    assert!(!has(&t, banned, year).await);
    assert!(!has(&t, waiting, year).await);
    assert!(!has(&t, system, year).await);

    // An alert from System links to the badge.
    let (kind, object, from): (String, i32, i32) = sqlx::query_as(
        "SELECT kind, object_id, from_uid FROM alerts WHERE uid = $1 AND kind = 'badge' AND object_id = $2",
    )
    .bind(old)
    .bind(year)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!((kind.as_str(), object, from), ("badge", year, system));
    let unread: i32 = sqlx::query_scalar("SELECT unreadalerts FROM users WHERE uid = $1")
        .bind(old)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(unread >= 1);

    // A second run awards nothing new and sends no new alerts.
    let alerts_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alerts")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(rbb::badges::run(&t.app).await.unwrap(), "awarded 0 badges");
    let alerts_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alerts")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(alerts_before, alerts_after);
}

#[tokio::test]
async fn disabled_badges_are_not_awarded_or_shown() {
    let t = test_app!();
    let year = badge_id(&t, "1 Year of Service").await;
    sqlx::query("UPDATE badges SET enabled = FALSE WHERE bid = $1")
        .bind(year)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["badges"]).await.unwrap();
    let old = member(&t, "oldtimer", 400, 2).await;
    rbb::badges::run(&t.app).await.unwrap();
    assert!(!has(&t, old, year).await);
    let page = t.client().get("/badges").await;
    assert_eq!(page.status, 200);
    assert!(!page.body.contains("1 Year of Service"));
    assert!(page.body.contains("2 Years of Service"));
}

#[tokio::test]
async fn badges_appear_on_profiles_posts_and_the_badge_pages() {
    let t = test_app!();
    let old = member(&t, "oldtimer", 400, 2).await;
    set(&t, &[("postfloodcheck", "0"), ("postmergemins", "0")]).await;
    let c = t.login_as(old).await;
    c.post_form(
        "/newthread/3",
        &[("subject", "Badge test"), ("message", "Hello there.")],
    )
    .await;
    rbb::badges::run(&t.app).await.unwrap();
    let year = badge_id(&t, "1 Year of Service").await;
    let first = badge_id(&t, "First Post").await;
    assert!(has(&t, old, year).await && has(&t, old, first).await);

    let profile = c.get(&format!("/user/{old}")).await;
    assert_eq!(profile.status, 200);
    assert!(profile.body.contains("profile-badges"));
    assert!(profile.body.contains("1 Year of Service"));
    assert!(profile.body.contains("First Post"));

    let tid: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let thread = c.get(&format!("/thread/{tid}")).await;
    assert!(thread.body.contains(&format!("href=\"/badges/{year}\"")));
    // Limited by the setting: 1 shows only the first in display order (1 Year before First Post).
    set(&t, &[("badgespostbit", "1")]).await;
    let thread = c.get(&format!("/thread/{tid}")).await;
    assert!(thread.body.contains(&format!("href=\"/badges/{year}\"")));
    assert!(!thread.body.contains(&format!("href=\"/badges/{first}\"")));

    let list = c.get("/badges").await;
    assert!(list.body.contains("Earned"));
    let holders = c.get(&format!("/badges/{year}")).await;
    assert_eq!(holders.status, 200);
    assert!(holders.body.contains("oldtimer"));

    let api = c.get(&format!("/api/v1/users/{old}")).await;
    assert!(api.body.contains("\"badges\""), "{}", api.body);
}

#[tokio::test]
async fn staff_create_award_and_revoke_badges() {
    let t = test_app!();
    let helper = member(&t, "helper", 5, 2).await;
    let a = admin(&t).await;
    let r = a
        .post_form(
            "/admin/badges/edit",
            &[
                ("bid", "0"),
                ("name", "Helpful"),
                ("description", "Goes out of their way to help."),
                ("icon", "shield"),
                ("color", "<script>"),
                ("disporder", "5"),
                ("enabled", "1"),
            ],
        )
        .await;
    assert_eq!(r.status, 303, "{}", r.body);
    let (bid, color, reqs): (i32, String, serde_json::Value) =
        sqlx::query_as("SELECT bid, color, requirements FROM badges WHERE name = 'Helpful'")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(color, "bronze", "unknown colours fall back to the first");
    assert_eq!(
        reqs,
        serde_json::json!({}),
        "no requirements: awarded by hand"
    );
    // The task never awards a hand-only badge.
    rbb::badges::run(&t.app).await.unwrap();
    assert!(!has(&t, helper, bid).await);

    let r = a
        .post_form(
            "/admin/badges/award",
            &[
                ("bid", &bid.to_string()),
                ("username", "HELPER"),
                ("reason", "Answered every question this week"),
            ],
        )
        .await;
    assert_eq!(r.status, 303);
    let (manual, by, reason): (bool, Option<i32>, String) = sqlx::query_as(
        "SELECT manual, awarded_by, reason FROM user_badges WHERE uid = $1 AND bid = $2",
    )
    .bind(helper)
    .bind(bid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert!(manual);
    assert_eq!(by, Some(1));
    assert_eq!(reason, "Answered every question this week");
    let page = a.get(&format!("/admin/badges/holders?bid={bid}")).await;
    assert!(page.body.contains("Answered every question this week"));

    let r = a
        .post_form(
            "/admin/badges/revoke",
            &[("bid", &bid.to_string()), ("uid", &helper.to_string())],
        )
        .await;
    assert_eq!(r.status, 303);
    assert!(!has(&t, helper, bid).await);
    let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM adminlog WHERE module = 'badges'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(logged, 3, "saved, awarded, revoked");

    // Members can't reach the Admin CP pages.
    let m = t.login_as(helper).await;
    let r = m.get("/admin/badges").await;
    assert_ne!(r.status, 200);
}
