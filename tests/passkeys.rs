//! Passkeys end to end: 1Password's software authenticator (passkey-rs) plays the browser and
//! the device, against the real endpoints.

mod common;

use common::{Client, TestApp};
use passkey::authenticator::{Authenticator, UiHint, UserCheck, UserValidationMethod};
use passkey::client::{Client as WebAuthnClient, DefaultClientData};
use passkey::crypto::rust_crypto::RustCryptoBackend;
use passkey::types::Passkey;
use passkey::types::ctap2::{Aaguid, Ctap2Error};
use passkey::types::webauthn::{CredentialCreationOptions, CredentialRequestOptions};

const BOARD: &str = "https://forum.example.org";
const PASSWORD: &str = "Passw0rd-passkeys";

/// A device whose owner always unlocks it.
struct Unlocked;

#[async_trait::async_trait]
impl UserValidationMethod for Unlocked {
    type PasskeyItem = Passkey;
    async fn check_user<'a>(
        &self,
        _hint: UiHint<'a, Passkey>,
        presence: bool,
        verification: bool,
    ) -> Result<UserCheck, Ctap2Error> {
        Ok(UserCheck {
            presence,
            verification,
        })
    }
    fn is_presence_enabled(&self) -> bool {
        true
    }
    fn is_verification_enabled(&self) -> Option<bool> {
        Some(true)
    }
}

type Device = WebAuthnClient<
    Option<Passkey>,
    Unlocked,
    RustCryptoBackend,
    public_suffix::PublicSuffixList,
    (),
>;

fn device() -> Device {
    WebAuthnClient::new(Authenticator::new(
        Aaguid::new_empty(),
        None,
        Unlocked,
        RustCryptoBackend,
    ))
}

fn origin(url: &str) -> url::Url {
    url::Url::parse(url).unwrap()
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

fn json(r: &common::Response) -> serde_json::Value {
    serde_json::from_str(&r.body).unwrap_or_else(|_| panic!("not JSON ({}): {}", r.status, r.body))
}

/// Add a passkey for the signed-in member on `dev`. Returns the finish response.
async fn add(c: &Client, dev: &mut Device, at: &str) -> common::Response {
    let r = c
        .post_form("/usercp/passkeys/begin", &[("password", PASSWORD)])
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let options: CredentialCreationOptions =
        serde_json::from_value(serde_json::json!({ "publicKey": json(&r) })).unwrap();
    let cred = dev
        .register(&origin(at), options, DefaultClientData)
        .await
        .expect("the device creates a passkey");
    c.post_form(
        "/usercp/passkeys/finish",
        &[
            ("credential", &serde_json::to_string(&cred).unwrap()),
            ("name", "Test phone"),
        ],
    )
    .await
}

/// A guest signs in with `dev`. Returns the client and the finish response.
async fn sign_in(t: &TestApp, dev: &mut Device, at: &str) -> (Client, common::Response, String) {
    let g = t.client();
    let page = g.get("/member/login").await;
    assert!(page.body.contains("Sign in with a passkey"));
    let key = common::form_key(&page.body);
    let r = g
        .post_form("/member/login/passkey/begin", &[("my_post_key", &key)])
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let options: CredentialRequestOptions =
        serde_json::from_value(serde_json::json!({ "publicKey": json(&r) })).unwrap();
    let assertion = dev
        .authenticate(&origin(at), options, DefaultClientData)
        .await
        .expect("the device signs");
    let assertion = serde_json::to_string(&assertion).unwrap();
    let r = g
        .post_form(
            "/member/login/passkey/finish",
            &[
                ("my_post_key", &key),
                ("credential", &assertion),
                ("remember", "1"),
            ],
        )
        .await;
    (g, r, assertion)
}

#[tokio::test]
async fn members_add_a_passkey_and_sign_in_with_it() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let uid = t.create_user("keyholder", PASSWORD).await;
    let c = t.login_as(uid).await;
    let mut dev = device();

    // The password is required to add one.
    let r = c
        .post_form("/usercp/passkeys/begin", &[("password", "wrong")])
        .await;
    assert_eq!(r.status, 422);
    assert!(json(&r)["error"].as_str().unwrap().contains("password"));

    let r = add(&c, &mut dev, BOARD).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(json(&r)["redirect"], "/usercp/security");
    let (count, name): (i64, String) =
        sqlx::query_as("SELECT COUNT(*), MAX(name) FROM passkeys WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((count, name.as_str()), (1, "Test phone"));
    let page = c.get("/usercp/security").await;
    assert!(page.body.contains("Test phone"));
    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND action = 'passkey_added'",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // Sign in as a guest with nothing but the device.
    let (g, r, assertion) = sign_in(&t, &mut dev, BOARD).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(json(&r)["redirect"], "/");
    let home = g.get("/usercp").await;
    assert_eq!(home.status, 200, "signed in");
    assert!(home.body.contains("keyholder"));
    let last_used: i64 = sqlx::query_scalar("SELECT last_used FROM passkeys WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(last_used > 0);

    // The same signed response can't be used twice: its challenge is gone.
    let other = t.client();
    let page = other.get("/member/login").await;
    let key = common::form_key(&page.body);
    let r = other
        .post_form(
            "/member/login/passkey/finish",
            &[("my_post_key", &key), ("credential", &assertion)],
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(
        json(&r)["error"]
            .as_str()
            .unwrap()
            .contains("expired or was already used")
    );

    // After removing it, the device can't sign in any more.
    let id: i32 = sqlx::query_scalar("SELECT id FROM passkeys WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let r = c
        .post_form("/usercp/passkeys/remove", &[("id", &id.to_string())])
        .await;
    assert_eq!(r.status, 303);
    let (_, r, _) = sign_in(&t, &mut dev, BOARD).await;
    assert_eq!(r.status, 422);
    assert!(
        json(&r)["error"]
            .as_str()
            .unwrap()
            .contains("isn't registered")
    );
}

#[tokio::test]
async fn a_passkey_made_for_another_site_is_rejected() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let uid = t.create_user("keyholder", PASSWORD).await;
    let c = t.login_as(uid).await;
    let mut dev = device();
    add(&c, &mut dev, BOARD).await;
    // A response signed for the real site is relayed to a board at another address (what a
    // phishing proxy would do): its origin and RP ID don't match, so it's refused.
    let g = t.client();
    let page = g.get("/member/login").await;
    let key = common::form_key(&page.body);
    let r = g
        .post_form("/member/login/passkey/begin", &[("my_post_key", &key)])
        .await;
    let options: CredentialRequestOptions =
        serde_json::from_value(serde_json::json!({ "publicKey": json(&r) })).unwrap();
    let assertion = dev
        .authenticate(&origin(BOARD), options, DefaultClientData)
        .await
        .unwrap();
    set(&t, &[("bburl", "https://new.example.org")]).await;
    let r = g
        .post_form(
            "/member/login/passkey/finish",
            &[
                ("my_post_key", &key),
                ("credential", &serde_json::to_string(&assertion).unwrap()),
            ],
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(
        json(&r)["error"]
            .as_str()
            .unwrap()
            .contains("couldn't be verified")
    );
    assert!(g.get("/usercp").await.status != 200, "not signed in");
}

#[tokio::test]
async fn passkeys_are_hidden_where_browsers_cannot_use_them() {
    let t = test_app!();
    // The test board's URL is http://127.0.0.1: an IP address, so no passkeys.
    let page = t.client().get("/member/login").await;
    assert!(!page.body.contains("Sign in with a passkey"));
    let uid = t.create_user("keyholder", PASSWORD).await;
    let c = t.login_as(uid).await;
    let page = c.get("/usercp/security").await;
    assert!(page.body.contains("IP address"), "explains why");
    let r = c
        .post_form("/usercp/passkeys/begin", &[("password", PASSWORD)])
        .await;
    assert_eq!(r.status, 422);
    // And when an admin turns them off.
    set(&t, &[("bburl", BOARD), ("enablepasskeys", "0")]).await;
    let page = t.client().get("/member/login").await;
    assert!(!page.body.contains("Sign in with a passkey"));
}

async fn admin_account(t: &TestApp, name: &str) -> (i32, Client, Device) {
    let uid = t.create_user(name, PASSWORD).await;
    sqlx::query("UPDATE users SET usergroup = 4 WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    let mut dev = device();
    assert_eq!(add(&c, &mut dev, BOARD).await.status, 200);
    (uid, c, dev)
}

async fn assertion(c: &Client, dev: &mut Device, endpoint: &str) -> String {
    let r = c.post_form(endpoint, &[]).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let options: CredentialRequestOptions =
        serde_json::from_value(serde_json::json!({"publicKey": json(&r)})).unwrap();
    let signed = dev
        .authenticate(&origin(BOARD), options, DefaultClientData)
        .await
        .unwrap();
    serde_json::to_string(&signed).unwrap()
}

async fn verified(t: &TestApp, uid: i32) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(MAX(acp_verified), 0) FROM logins WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn administrators_can_confirm_with_a_passkey_and_replay_is_rejected() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let (uid, c, mut dev) = admin_account(&t, "passkeyadmin").await;
    // Keep the board's TOTP-enrollment policy while allowing user-verified passkey confirmation.
    sqlx::query("UPDATE users SET totp_secret = 'JBSWY3DPEHPK3PXP' WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    set(&t, &[("acp2fa", "1")]).await;
    let page = c.get("/admin/verify").await;
    assert!(page.body.contains("Continue with a passkey"));
    assert!(page.body.contains("Confirm your password"));
    let old_cookie = c.cookie(rbb::ctx::AUTH_COOKIE);
    let signed = assertion(&c, &mut dev, "/admin/verify/passkey/begin").await;
    let r = c
        .post_form(
            "/admin/verify/passkey/finish",
            &[("credential", &signed), ("return_to", "/admin/users")],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(json(&r)["redirect"], "/admin/users");
    assert!(verified(&t, uid).await > 0);
    assert_ne!(
        c.cookie(rbb::ctx::AUTH_COOKIE),
        old_cookie,
        "session rotated"
    );
    assert_eq!(c.get("/admin").await.status, 200);
    let r = c
        .post_form("/admin/verify/passkey/finish", &[("credential", &signed)])
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM adminlog WHERE uid = $1 AND action = 'Admin CP login' AND data->>'passkey' = 'true'")
        .bind(uid).fetch_one(&t.db.pool).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn admin_passkeys_must_match_the_account_session_and_purpose() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let (uid, c, mut dev) = admin_account(&t, "firstadmin").await;
    let (other_uid, other, mut other_dev) = admin_account(&t, "otheradmin").await;
    // A valid passkey for a different administrator must not elevate this session.
    let wrong_account = assertion(&c, &mut other_dev, "/admin/verify/passkey/begin").await;
    let r = c
        .post_form(
            "/admin/verify/passkey/finish",
            &[("credential", &wrong_account)],
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert_eq!(verified(&t, uid).await, 0);
    // Even another session for the same account cannot finish the ceremony.
    let same_account = t.login_as(uid).await;
    let signed = assertion(&c, &mut dev, "/admin/verify/passkey/begin").await;
    let r = same_account
        .post_form("/admin/verify/passkey/finish", &[("credential", &signed)])
        .await;
    assert_eq!(r.status, 422);
    assert_eq!(verified(&t, uid).await, 0);
    // An Admin CP challenge cannot be exchanged at the normal member login endpoint.
    let guest = t.client();
    let page = guest.get("/member/login").await;
    let key = common::form_key(&page.body);
    let r = guest
        .post_form(
            "/member/login/passkey/finish",
            &[("my_post_key", &key), ("credential", &signed)],
        )
        .await;
    assert_eq!(r.status, 422);
    // A normal login assertion cannot be exchanged for Admin CP verification either.
    let r = guest
        .post_form("/member/login/passkey/begin", &[("my_post_key", &key)])
        .await;
    let options: CredentialRequestOptions =
        serde_json::from_value(serde_json::json!({"publicKey": json(&r)})).unwrap();
    let signed = dev
        .authenticate(&origin(BOARD), options, DefaultClientData)
        .await
        .unwrap();
    let signed = serde_json::to_string(&signed).unwrap();
    let r = c
        .post_form("/admin/verify/passkey/finish", &[("credential", &signed)])
        .await;
    assert_eq!(r.status, 422);
    assert_eq!(verified(&t, uid).await, 0);
    assert_eq!(verified(&t, other_uid).await, 0);
    assert_eq!(other.get("/admin/verify").await.status, 200);
}

#[tokio::test]
async fn admin_passkey_endpoints_keep_permission_csrf_expiry_and_setting_gates() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let (uid, c, mut dev) = admin_account(&t, "gatedadmin").await;
    let r = t
        .client()
        .post_form("/admin/verify/passkey/begin", &[])
        .await;
    assert_ne!(r.status, 200);
    let member_uid = t.create_user("regularmember", PASSWORD).await;
    let member = t.login_as(member_uid).await;
    assert_eq!(
        member
            .post_form("/admin/verify/passkey/begin", &[])
            .await
            .status,
        403
    );
    assert_eq!(
        c.post_form("/admin/verify/passkey/begin", &[("my_post_key", "wrong")])
            .await
            .status,
        403
    );
    let signed = assertion(&c, &mut dev, "/admin/verify/passkey/begin").await;
    sqlx::query("UPDATE webauthn_ceremonies SET expires_at = now() - interval '1 second'")
        .execute(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(
        c.post_form("/admin/verify/passkey/finish", &[("credential", &signed)])
            .await
            .status,
        422
    );
    assert_eq!(verified(&t, uid).await, 0);
    let signed = assertion(&c, &mut dev, "/admin/verify/passkey/begin").await;
    set(&t, &[("enablepasskeys", "0")]).await;
    assert!(
        !c.get("/admin/verify")
            .await
            .body
            .contains("Continue with a passkey")
    );
    assert_eq!(
        c.post_form("/admin/verify/passkey/finish", &[("credential", &signed)])
            .await
            .status,
        422
    );
    assert_eq!(verified(&t, uid).await, 0);
}

#[tokio::test]
async fn admin_passkey_redirects_stay_in_the_admin_cp() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let (_, c, mut dev) = admin_account(&t, "redirectadmin").await;
    let signed = assertion(&c, &mut dev, "/admin/verify/passkey/begin").await;
    let r = c
        .post_form(
            "/admin/verify/passkey/finish",
            &[
                ("credential", &signed),
                ("return_to", "//evil.example/admin"),
            ],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(json(&r)["redirect"], "/admin");
}

#[tokio::test]
async fn admin_passkey_assertions_without_user_verification_are_rejected() {
    use base64::Engine;
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let (uid, c, mut dev) = admin_account(&t, "uvadmin").await;
    let r = c.post_form("/admin/verify/passkey/begin", &[]).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let mut options = json(&r);
    assert_eq!(options["userVerification"], "required");
    // An untrusted client can alter the options given to its authenticator. Produce a real,
    // signed assertion with UV=false while the server retains its original UV requirement.
    options["userVerification"] = serde_json::json!("discouraged");
    let options: CredentialRequestOptions =
        serde_json::from_value(serde_json::json!({"publicKey": options})).unwrap();
    let signed = dev
        .authenticate(&origin(BOARD), options, DefaultClientData)
        .await
        .unwrap();
    let signed = serde_json::to_value(&signed).unwrap();
    let auth_data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signed["response"]["authenticatorData"].as_str().unwrap())
        .unwrap();
    assert_eq!(auth_data[32] & 0x04, 0, "the signed assertion has UV=false");
    let old_cookie = c.cookie(rbb::ctx::AUTH_COOKIE);
    let r = c
        .post_form(
            "/admin/verify/passkey/finish",
            &[("credential", &signed.to_string())],
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(
        json(&r)["error"]
            .as_str()
            .unwrap()
            .contains("couldn't be verified")
    );
    assert_eq!(verified(&t, uid).await, 0);
    assert_eq!(c.cookie(rbb::ctx::AUTH_COOKIE), old_cookie);
    assert_eq!(c.get("/admin").await.status, 303, "Admin CP remains locked");
}

#[tokio::test]
async fn admins_without_passkeys_see_only_password_confirmation_even_after_errors() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let uid = t.create_user("passwordadmin", PASSWORD).await;
    sqlx::query("UPDATE users SET usergroup = 4 WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    let page = c.get("/admin/verify").await;
    assert!(page.body.contains("Confirm your password"));
    assert!(!page.body.contains("Continue with a passkey"));
    let r = c.post_form("/admin/verify", &[("password", "wrong")]).await;
    assert!(r.body.contains("incorrect"));
    assert!(!r.body.contains("Continue with a passkey"));
    let mut dev = device();
    assert_eq!(add(&c, &mut dev, BOARD).await.status, 200);
    assert!(
        c.get("/admin/verify")
            .await
            .body
            .contains("Continue with a passkey")
    );
    let r = c.post_form("/admin/verify", &[("password", "wrong")]).await;
    assert!(r.body.contains("Continue with a passkey"));
}

#[tokio::test]
async fn passkey_clicks_do_not_lock_out_password_confirmation() {
    let t = test_app!();
    set(&t, &[("bburl", BOARD)]).await;
    let (uid, c, _) = admin_account(&t, "throttleadmin").await;
    for _ in 0..10 {
        assert_eq!(
            c.post_form("/admin/verify/passkey/begin", &[]).await.status,
            200
        );
    }
    assert_eq!(
        c.post_form("/admin/verify/passkey/begin", &[]).await.status,
        429
    );
    let r = c
        .post_form("/admin/verify", &[("password", PASSWORD)])
        .await;
    assert_eq!(r.status, 303, "{}", r.body);
    assert!(verified(&t, uid).await > 0);
}

#[tokio::test]
async fn password_confirmation_redirects_stay_in_the_admin_cp() {
    let t = test_app!();
    let uid = t.create_user("passwordredirectadmin", PASSWORD).await;
    sqlx::query("UPDATE users SET usergroup = 4 WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    for (requested, expected) in [
        ("/admin/users", "/admin/users"),
        ("/admin?tab=home", "/admin?tab=home"),
        ("/admin#x", "/admin"),
        ("/adminfoo", "/admin"),
        ("//evil.example/admin", "/admin"),
        ("/admin/\\evil.example", "/admin"),
    ] {
        let r = c
            .post_form(
                "/admin/verify",
                &[("password", PASSWORD), ("return_to", requested)],
            )
            .await;
        assert_eq!(r.status, 303, "{}", r.body);
        assert_eq!(r.location(), expected, "redirect for {requested}");
    }
}
