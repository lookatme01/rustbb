//! Registration, activation and password reset against a real database.

mod common;

use common::TestApp;

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
