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
