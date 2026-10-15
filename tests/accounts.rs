//! Registration, activation and password reset against a real database.

mod common;

use common::TestApp;

#[tokio::test]
async fn concurrent_failed_logins_do_not_lose_attempts_or_clear_a_lockout() {
    let t = test_app!();
    set(&t, &[("failedlogincount", "5"), ("failedlogintime", "15")]).await;
    let uid = t.create_user("locked", "Correct-Passw0rd").await;
    let c = t.client();
    // API login uses the same credential checker without browser CSRF setup.
    let responses = futures::future::join_all((0..8).map(|_| {
        c.send_json(
            "POST",
            "/api/v1/auth/token",
            serde_json::json!({"username":"locked", "password":"wrong"}),
            &[],
        )
    }))
    .await;
    for r in responses {
        assert!(r.status.is_client_error(), "{}", r.status);
    }
    let (attempts, expiry): (i32, i64) =
        sqlx::query_as("SELECT loginattempts, loginlockoutexpiry FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0);
    assert!(expiry > rbb::util::now());
    let r = c
        .send_json(
            "POST",
            "/api/v1/auth/token",
            serde_json::json!({"username":"locked", "password":"Correct-Passw0rd"}),
            &[],
        )
        .await;
    assert!(r.status.is_client_error());
}

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

fn formtoken(t: &TestApp) -> String {
    let ts = rbb::util::now() - 30;
    let sig = rbb::util::hmac_hex(&t.app.cfg.secret, &format!("reg:{ts}"));
    format!("{ts}.{}", &sig[..16])
}

async fn open_registration(t: &TestApp, regtype: &str) {
    set(
        t,
        &[
            ("regtype", regtype),
            ("captchaimage", "0"),
            ("securityquestion", "0"),
            ("minregtime", "0"),
            ("honeypot", "0"),
            ("disableregs", "0"),
            ("maxregsbetweentime", "0"),
        ],
    )
    .await;
}

async fn register(t: &TestApp, name: &str) -> common::Response {
    let c = t.client();
    let page = c.get("/member/register").await;
    let key = common::form_key(&page.body);
    let token = formtoken(t);
    c.post_form(
        "/member/register",
        &[
            ("my_post_key", &key),
            ("username", name),
            ("password", "Corr3ct-Horse-Battery"),
            ("password2", "Corr3ct-Horse-Battery"),
            ("email", &format!("{name}@example.org")),
            ("email2", &format!("{name}@example.org")),
            ("agree", "1"),
            ("formtoken", &token),
        ],
    )
    .await
}

#[tokio::test]
async fn registration_writes_audit_row_in_the_same_transaction() {
    let t = test_app!();
    open_registration(&t, "instant").await;
    let r = register(&t, "newbie").await;
    assert!(
        r.status.is_redirection(),
        "status {} body {}",
        r.status,
        &r.body[..r.body.len().min(2000)]
    );
    let uid: i32 = sqlx::query_scalar("SELECT uid FROM users WHERE username = 'newbie'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND action = 'registered'",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "the registration must be audited");
}

#[tokio::test]
async fn registration_confirm_fields_are_optional_but_checked_when_sent() {
    let t = test_app!();
    open_registration(&t, "instant").await;
    let post = |name: &'static str, extra: Vec<(&'static str, String)>| {
        let t = &t;
        async move {
            let c = t.client();
            let key = common::form_key(&c.get("/member/register").await.body);
            let mut f = vec![
                ("my_post_key", key),
                ("username", name.to_string()),
                ("password", "Corr3ct-Horse-Battery".to_string()),
                ("email", format!("{name}@example.org")),
                ("agree", "1".to_string()),
                ("formtoken", formtoken(t)),
            ];
            f.extend(extra);
            let f: Vec<(&str, &str)> = f.iter().map(|(a, b)| (*a, b.as_str())).collect();
            c.post_form("/member/register", &f).await
        }
    };
    // The current form sends neither confirm field.
    let r = post("noconfirm", vec![]).await;
    assert!(r.status.is_redirection(), "status {}", r.status);
    // Older themes still send them, and a mismatch is still an error.
    let r = post("badconfirm", vec![("password2", "something-else".into())]).await;
    assert!(r.body.contains("passwords you entered do not match"));
    let r = post("bademail", vec![("email2", "other@example.org".into())]).await;
    assert!(r.body.contains("email addresses you entered do not match"));
}

#[tokio::test]
async fn activation_code_can_be_used_once() {
    let t = test_app!();
    open_registration(&t, "verify").await;
    let r = register(&t, "pending").await;
    assert!(r.status.is_redirection());
    // The link comes from the queued email (only a hash of the code is stored).
    let body: String =
        sqlx::query_scalar("SELECT message FROM mailqueue WHERE mailto = 'pending@example.org'")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    let link = body
        .lines()
        .find(|l| l.contains("/member/activate?uid="))
        .unwrap();
    let q = &link[link.find('?').unwrap() + 1..];
    let uid: i32 = q
        .split('&')
        .next()
        .unwrap()
        .trim_start_matches("uid=")
        .parse()
        .unwrap();
    let code = q.split("code=").nth(1).unwrap().trim().to_string();
    let stored: String = sqlx::query_scalar("SELECT code FROM awaitingactivation WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(
        stored,
        rbb::util::sha256_hex(&code),
        "only the hash is stored"
    );
    let url = format!("/member/activate?uid={uid}&code={code}");
    let c = t.client();
    // Concurrent use of the same code: exactly one succeeds.
    let (a, b) = tokio::join!(c.get(&url), c.get(&url));
    let ok = [&a, &b]
        .iter()
        .filter(|r| r.status.is_redirection())
        .count();
    assert_eq!(ok, 1, "statuses {} and {}", a.status, b.status);
    let group: i32 = sqlx::query_scalar("SELECT usergroup FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(group, 2);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND action = 'activated'",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn password_reset_token_is_consumed_atomically() {
    let t = test_app!();
    let uid = t.create_user("forgetful", "Old-Passw0rd-123").await;
    let code = "reset-code-for-test-0123456789";
    sqlx::query(
        "INSERT INTO awaitingactivation (uid, dateline, code, type) VALUES ($1, $2, $3, 'p')",
    )
    .bind(uid)
    .bind(rbb::util::now())
    .bind(rbb::util::sha256_hex(code))
    .execute(&t.db.pool)
    .await
    .unwrap();
    let c1 = t.client();
    let c2 = t.client();
    let k1 = common::form_key(
        &c1.get(&format!("/member/resetpw?uid={uid}&code={code}"))
            .await
            .body,
    );
    let k2 = common::form_key(
        &c2.get(&format!("/member/resetpw?uid={uid}&code={code}"))
            .await
            .body,
    );
    let uid_s = uid.to_string();
    let f = |key: &str, pw: &str| {
        vec![
            ("my_post_key", key.to_string()),
            ("uid", uid_s.clone()),
            ("code", code.to_string()),
            ("password", pw.to_string()),
            ("password2", pw.to_string()),
        ]
    };
    let f1 = f(&k1, "First-New-Passw0rd");
    let f2 = f(&k2, "Second-New-Passw0rd");
    let f1: Vec<(&str, &str)> = f1.iter().map(|(a, b)| (*a, b.as_str())).collect();
    let f2: Vec<(&str, &str)> = f2.iter().map(|(a, b)| (*a, b.as_str())).collect();
    let (a, b) = tokio::join!(
        c1.post_form("/member/resetpw", &f1),
        c2.post_form("/member/resetpw", &f2)
    );
    let ok = [&a, &b]
        .iter()
        .filter(|r| r.status.is_redirection() && r.location().starts_with("/member/login"))
        .count();
    assert_eq!(
        ok, 1,
        "exactly one reset may succeed ({} / {})",
        a.status, b.status
    );
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM awaitingactivation WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND action = 'password_reset'",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn control_panel_shows_the_members_own_email() {
    let t = test_app!();
    let uid = t.create_user("mailcheck", "Passw0rd-mailcheck").await;
    let r = t.login_as(uid).await.get("/usercp").await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("mailcheck@example.com"), "{}", r.body);
}

#[tokio::test]
async fn instant_registration_lands_on_the_welcome_page() {
    let t = test_app!();
    open_registration(&t, "instant").await;
    let c = t.client();
    let key = common::form_key(&c.get("/member/register").await.body);
    let token = formtoken(&t);
    let r = c
        .post_form(
            "/member/register",
            &[
                ("my_post_key", key.as_str()),
                ("username", "greeted"),
                ("password", "Corr3ct-Horse-Battery"),
                ("email", "greeted@example.org"),
                ("agree", "1"),
                ("formtoken", token.as_str()),
            ],
        )
        .await;
    assert_eq!(r.location(), "/member/welcome", "status {}", r.status);
    let w = c.get("/member/welcome").await;
    assert_eq!(w.status, 200);
    assert!(w.body.contains("Welcome, greeted"), "{}", w.body);
    assert!(w.body.contains("Start reading"));
    // Guests are sent to log in.
    assert_ne!(t.client().get("/member/welcome").await.status, 200);
}

#[tokio::test]
async fn activity_flushes_rewrite_a_members_row_at_most_once_a_minute() {
    use rbb::app::Activity;
    let t = test_app!();
    let uid = t.create_user("browsing", "Passw0rd-browsing").await;
    let base = rbb::util::now() - 3600;
    let visit = |time: i64| {
        t.app.activity.insert(
            "sid-browsing".into(),
            Activity {
                uid,
                ip: "203.0.113.9".into(),
                time,
                location: "index".into(),
                useragent: "test".into(),
                anonymous: false,
                location1: 0,
                location2: 0,
                bot: String::new(),
            },
        );
    };
    let row = || async {
        sqlx::query_as::<_, (i64, i64)>("SELECT lastactive, timeonline FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    };
    visit(base);
    rbb::app::flush_activity(&t.app).await.unwrap();
    assert_eq!(row().await, (base, 0));
    // Ten seconds later: the session row updates, the users row is left alone.
    visit(base + 10);
    rbb::app::flush_activity(&t.app).await.unwrap();
    assert_eq!(row().await, (base, 0));
    // A minute on, lastactive moves and the whole gap counts as time online.
    visit(base + 70);
    rbb::app::flush_activity(&t.app).await.unwrap();
    assert_eq!(row().await, (base + 70, 70));
}

#[tokio::test]
async fn a_forged_undo_cookie_cannot_aim_the_csrf_token_off_site() {
    let t = test_app!();
    let c = t.login_as(1).await;
    let page = |action: &str| {
        let json = serde_json::json!({"action": action, "fields": []}).to_string();
        c.set_cookie("rbb_flash", "Done.");
        c.set_cookie("rbb_undo", &percent_encode(&json));
        c.get("/")
    };
    let ok = page("/thread/1/undo").await;
    assert!(
        ok.body.contains("class=\"js-undo\""),
        "a local undo is still offered"
    );
    for bad in [
        "/\\evil.example/x",
        "//evil.example/x",
        "https://evil.example/x",
    ] {
        let r = page(bad).await;
        assert!(!r.body.contains("evil.example"), "{bad}");
    }
}

fn percent_encode(s: &str) -> String {
    s.bytes().map(|b| format!("%{b:02X}")).collect()
}

async fn pending(t: &common::TestApp, uid: i32, kind: &str, code: &str, age: i64, misc: &str) {
    sqlx::query("DELETE FROM awaitingactivation WHERE uid = $1 AND type = $2")
        .bind(uid)
        .bind(kind)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO awaitingactivation (uid, dateline, code, type, misc) VALUES ($1, $2, $3, $4, $5)")
        .bind(uid)
        .bind(rbb::util::now() - age)
        .bind(rbb::util::sha256_hex(code))
        .bind(kind)
        .bind(misc)
        .execute(&t.db.pool)
        .await
        .unwrap();
}

async fn email_of(t: &common::TestApp, uid: i32) -> String {
    sqlx::query_scalar("SELECT email FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn email_change_links_expire_and_die_with_the_credentials_that_requested_them() {
    let t = test_app!();
    let uid = t.create_user("changer", "Passw0rd-changer").await;
    let before = email_of(&t, uid).await;
    let c = t.client();
    // A link older than a day does nothing.
    pending(
        &t,
        uid,
        "e",
        "stalecode123",
        2 * 86400,
        "stale@example.test",
    )
    .await;
    let r = c
        .get(&format!("/member/activate?uid={uid}&code=stalecode123"))
        .await;
    assert!(
        r.status.is_client_error() || r.body.contains("invalid"),
        "{} {}",
        r.status,
        r.body
    );
    assert_eq!(email_of(&t, uid).await, before);
    // A fresh one works.
    pending(&t, uid, "e", "freshcode123", 60, "fresh@example.test").await;
    c.get(&format!("/member/activate?uid={uid}&code=freshcode123"))
        .await;
    assert_eq!(email_of(&t, uid).await, "fresh@example.test");
    // A pending change and a pending reset are discarded when sessions are revoked.
    pending(&t, uid, "e", "pendingcode1", 60, "attacker@example.test").await;
    pending(&t, uid, "p", "resetcode123", 60, "").await;
    rbb::auth::destroy_all_logins(&t.app, uid, None)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM awaitingactivation WHERE uid = $1 AND type IN ('e', 'p')",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

fn totp_now(secret: &str) -> String {
    let bytes = totp_rs::Secret::Encoded(secret.to_string())
        .to_bytes()
        .unwrap();
    totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some("rbb".into()),
        "user".into(),
    )
    .unwrap()
    .generate_current()
    .unwrap()
}

#[tokio::test]
async fn two_factor_changes_end_other_sessions_and_cannot_be_replaced_with_just_the_password() {
    const SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";
    let t = test_app!();
    let uid = t.create_user("secured", "Passw0rd-secured").await;
    sqlx::query("UPDATE users SET totp_secret = $2 WHERE uid = $1")
        .bind(uid)
        .bind(SECRET)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let here = t.login_as(uid).await;
    let _elsewhere = t.login_as(uid).await;
    let sessions = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM logins WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    };
    assert_eq!(sessions().await, 2);
    // Setting up again while it is on is refused, whatever secret is offered.
    let r = here
        .post_form(
            "/usercp/security/2fa",
            &[
                ("password", "Passw0rd-secured"),
                ("secret", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
                ("code", &totp_now("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")),
            ],
        )
        .await;
    assert!(r.body.contains("already on"), "{} {}", r.status, r.body);
    // Turning it off signs the other session out.
    let r = here
        .post_form(
            "/usercp/security/2fa",
            &[
                ("password", "Passw0rd-secured"),
                ("disable", "1"),
                ("code", &totp_now(SECRET)),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    assert_eq!(sessions().await, 1);
}

#[tokio::test]
async fn captcha_refresh_is_rate_limited_per_address() {
    let t = test_app!();
    let c = t.client();
    let mut last = 200;
    for _ in 0..31 {
        last = c.get("/captcha/refresh").await.status.as_u16();
    }
    assert_eq!(last, 429);
}

#[tokio::test]
async fn an_address_registered_as_a_username_cannot_take_over_email_sign_in() {
    let t = test_app!();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('usernamemethod', '2') ON CONFLICT (name) DO UPDATE SET value = '2'")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let victim = t.create_user("victim", "Passw0rd-victim").await;
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE uid = $1")
        .bind(victim)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    // The imposter's username is the victim's address.
    let imposter = t.create_user("imposter", "Passw0rd-imposter").await;
    sqlx::query("UPDATE users SET username = $2 WHERE uid = $1")
        .bind(imposter)
        .bind(&email)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.client();
    let key = common::form_key(&c.get("/member/login").await.body);
    let r = c
        .post_form(
            "/member/login",
            &[
                ("my_post_key", &key),
                ("username", &email),
                ("password", "Passw0rd-victim"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{}", r.status);
    assert!(
        c.cookie(rbb::ctx::AUTH_COOKIE).is_some(),
        "the owner signed in"
    );
}
