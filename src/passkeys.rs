//! Passkeys: WebAuthn registration and discoverable ("username-less") sign-in.
//!
//! The relying party is the board URL (`bburl`): its host is the RP ID and its origin the only
//! origin accepted. Ceremony state lives in `webauthn_ceremonies`, so any node can finish a
//! ceremony another node started, and each challenge is consumed exactly once.

use crate::app::App;
use crate::error::{AppError, AppResult};
use crate::util::now;
use webauthn_rp::bin::{Decode, Encode};
use webauthn_rp::request::auth::AuthenticationVerificationOptions;
use webauthn_rp::request::register::{
    Nickname, PublicKeyCredentialUserEntity, RegistrationVerificationOptions, UserHandle64,
    Username,
};
use webauthn_rp::request::{AsciiDomain, PublicKeyCredentialDescriptor, RpId, TimedCeremony};
use webauthn_rp::response::register::{CompressedPubKey, DynamicState, StaticState};
use webauthn_rp::response::{AuthTransports, CredentialId};
use webauthn_rp::{
    AuthenticatedCredential, DiscoverableAuthentication64, DiscoverableAuthenticationServerState,
    DiscoverableCredentialRequestOptions, PublicKeyCredentialCreationOptions, Registration,
    RegistrationServerState,
};

/// Passkeys a member may register.
pub const MAX_PER_MEMBER: i64 = 20;

/// The relying party derived from the board URL.
pub struct RelyingParty {
    pub id: RpId,
    pub origin: String,
}

/// The relying party for `board_url`, or why passkeys can't work there. Browsers only offer
/// passkeys to secure origins (HTTPS, or `localhost` for development) named by a domain.
pub fn relying_party(board_url: &str) -> Result<RelyingParty, String> {
    let url = url::Url::parse(board_url.trim())
        .map_err(|_| format!("the board URL “{board_url}” isn't a valid URL"))?;
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
        Some(_) => {
            return Err(
                "the board URL uses an IP address; passkeys need a domain name".to_string(),
            );
        }
        None => return Err("the board URL has no host".to_string()),
    };
    if url.scheme() != "https" && host != "localhost" {
        return Err(
            "the board URL isn't HTTPS; browsers only offer passkeys to secure sites".into(),
        );
    }
    let id = AsciiDomain::try_from(host.clone())
        .map(RpId::Domain)
        .map_err(|_| format!("“{host}” can't be used as a passkey domain"))?;
    Ok(RelyingParty {
        id,
        origin: url.origin().ascii_serialization(),
    })
}

/// The relying party if passkeys are enabled and possible on this board.
pub fn available(app: &App) -> Result<RelyingParty, String> {
    let cache = app.cache();
    if !cache.settings.bool("enablepasskeys") {
        return Err("passkeys are turned off".into());
    }
    relying_party(cache.settings.get("bburl"))
}

fn internal(msg: impl std::fmt::Display) -> AppError {
    AppError::Other(anyhow::anyhow!("passkeys: {msg}"))
}

fn unavailable(why: String) -> AppError {
    AppError::User(format!("Passkeys aren't available on this board: {why}."))
}

fn bad(what: &str) -> AppError {
    AppError::User(format!(
        "{what} Please try again; if it keeps failing, try another browser or device."
    ))
}

fn challenge_key(c: webauthn_rp::response::SentChallenge) -> Vec<u8> {
    c.0.to_be_bytes().to_vec()
}

async fn save_ceremony(
    app: &App,
    challenge: Vec<u8>,
    uid: Option<i32>,
    state: Vec<u8>,
    expires: std::time::SystemTime,
) -> AppResult<()> {
    sqlx::query("INSERT INTO webauthn_ceremonies (challenge, uid, state, expires_at) VALUES ($1, $2, $3, $4)")
        .bind(challenge)
        .bind(uid)
        .bind(state)
        .bind(chrono::DateTime::<chrono::Utc>::from(expires))
        .execute(&app.db)
        .await?;
    Ok(())
}

/// Consume a ceremony: it works once, only before it expires, and only for whoever started it.
async fn take_ceremony(app: &App, challenge: Vec<u8>, uid: Option<i32>) -> AppResult<Vec<u8>> {
    let state: Option<Vec<u8>> = sqlx::query_scalar(
        "DELETE FROM webauthn_ceremonies WHERE challenge = $1 AND uid IS NOT DISTINCT FROM $2 AND expires_at > now() RETURNING state",
    )
    .bind(challenge)
    .bind(uid)
    .fetch_optional(&app.db)
    .await?;
    state.ok_or_else(|| bad("That passkey request expired or was already used."))
}

/// Remove expired ceremonies (hourly cleanup).
pub async fn prune(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    Ok(
        sqlx::query("DELETE FROM webauthn_ceremonies WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// The member's WebAuthn user handle, created on first use.
async fn user_handle(app: &App, uid: i32) -> AppResult<UserHandle64> {
    let Ok(fresh) = UserHandle64::new().encode();
    sqlx::query("UPDATE users SET webauthn_handle = $2 WHERE uid = $1 AND webauthn_handle IS NULL")
        .bind(uid)
        .bind(fresh.as_slice())
        .execute(&app.db)
        .await?;
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT webauthn_handle FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&app.db)
        .await?;
    let bytes: [u8; 64] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| internal("bad webauthn handle"))?;
    let Ok(handle) = UserHandle64::decode(bytes);
    Ok(handle)
}

/// Start adding a passkey: the options for `navigator.credentials.create()`.
pub async fn begin_registration(
    app: &App,
    uid: i32,
    username: &str,
) -> AppResult<serde_json::Value> {
    let rp = available(app).map_err(unavailable)?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM passkeys WHERE uid = $1")
        .bind(uid)
        .fetch_one(&app.db)
        .await?;
    if count >= MAX_PER_MEMBER {
        return Err(AppError::User(format!(
            "You can have at most {MAX_PER_MEMBER} passkeys. Remove one you no longer use first."
        )));
    }
    let handle = user_handle(app, uid).await?;
    // Existing passkeys, so the device doesn't create a second one for this account.
    let existing: Vec<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT credential_id, transports FROM passkeys WHERE uid = $1")
            .bind(uid)
            .fetch_all(&app.db)
            .await?;
    let exclude: Vec<PublicKeyCredentialDescriptor<Vec<u8>>> = existing
        .into_iter()
        .filter_map(|(id, tr)| {
            Some(PublicKeyCredentialDescriptor {
                id: CredentialId::decode(id).ok()?,
                transports: AuthTransports::decode(tr.first().copied().unwrap_or(0)).ok()?,
            })
        })
        .collect();
    let fallback = format!("member{uid}");
    let name = Username::try_from(username)
        .or_else(|_| Username::try_from(fallback.as_str()))
        .map_err(|_| internal("username"))?;
    let display_name = Nickname::try_from(username).ok();
    let (server, client) = PublicKeyCredentialCreationOptions::passkey(
        &rp.id,
        PublicKeyCredentialUserEntity {
            name,
            id: &handle,
            display_name,
        },
        exclude,
    )
    .start_ceremony()
    .map_err(|e| internal(format!("passkey options: {e}")))?;
    let json =
        serde_json::to_value(&client).map_err(|e| internal(format!("passkey options: {e}")))?;
    let state = server
        .encode()
        .map_err(|e| internal(format!("passkey state: {e}")))?;
    save_ceremony(
        app,
        challenge_key(server.sent_challenge()),
        Some(uid),
        state,
        server.expiration(),
    )
    .await?;
    Ok(json)
}

/// Finish adding a passkey with the browser's response (`PublicKeyCredential.toJSON()`).
pub async fn finish_registration(
    app: &App,
    uid: i32,
    credential: &str,
    name: &str,
) -> AppResult<()> {
    let rp = available(app).map_err(unavailable)?;
    let reg = Registration::from_json_relaxed(credential.as_bytes())
        .map_err(|_| bad("Your browser sent a passkey we couldn't read."))?;
    let challenge = reg
        .challenge_relaxed()
        .map_err(|_| bad("Your browser sent a passkey we couldn't read."))?;
    let state = take_ceremony(app, challenge_key(challenge), Some(uid)).await?;
    let server = RegistrationServerState::<64>::decode(state.as_slice())
        .map_err(|_| internal("passkey state"))?;
    let origins = [rp.origin.as_str()];
    let cred = server
        .verify(
            &rp.id,
            &reg,
            &RegistrationVerificationOptions::<&str, &str> {
                allowed_origins: &origins,
                ..Default::default()
            },
        )
        .map_err(|e| {
            tracing::info!(uid, error = %e, "passkey registration rejected");
            bad("The passkey couldn't be verified.")
        })?;
    let name: String = name.trim().chars().take(60).collect();
    let name = if name.is_empty() {
        "Passkey".to_string()
    } else {
        name
    };
    let static_state = cred
        .static_state()
        .encode()
        .map_err(|e| internal(format!("passkey key: {e}")))?;
    let dynamic_state = cred
        .dynamic_state()
        .encode()
        .map_err(|e| internal(format!("passkey state: {e}")))?;
    let Ok(transports) = cred.transports().encode();
    let r = sqlx::query(
        "INSERT INTO passkeys (uid, credential_id, name, transports, static_state, dynamic_state, created)
         SELECT $1, $2, $3, $4, $5, $6, $7 WHERE (SELECT COUNT(*) FROM passkeys WHERE uid = $1) < $8
         ON CONFLICT (credential_id) DO NOTHING",
    )
    .bind(uid)
    .bind(cred.id().as_ref())
    .bind(&name)
    .bind([transports].as_slice())
    .bind(static_state)
    .bind(dynamic_state.as_slice())
    .bind(now())
    .bind(MAX_PER_MEMBER)
    .execute(&app.db)
    .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::user(
            "That passkey is already registered, or you have the maximum number of passkeys.",
        ));
    }
    Ok(())
}

/// Start signing in: the options for `navigator.credentials.get()`.
pub async fn begin_sign_in(app: &App) -> AppResult<serde_json::Value> {
    let rp = available(app).map_err(unavailable)?;
    let (server, client) = DiscoverableCredentialRequestOptions::passkey(&rp.id)
        .start_ceremony()
        .map_err(|e| internal(format!("passkey options: {e}")))?;
    let json =
        serde_json::to_value(&client).map_err(|e| internal(format!("passkey options: {e}")))?;
    let state = server
        .encode()
        .map_err(|e| internal(format!("passkey state: {e}")))?;
    save_ceremony(
        app,
        challenge_key(server.sent_challenge()),
        None,
        state,
        server.expiration(),
    )
    .await?;
    Ok(json)
}

/// Finish signing in. Returns the member the passkey belongs to.
pub async fn finish_sign_in(app: &App, credential: &str) -> AppResult<i32> {
    let rp = available(app).map_err(unavailable)?;
    let auth = DiscoverableAuthentication64::from_json_relaxed(credential.as_bytes())
        .map_err(|_| bad("Your browser sent a passkey we couldn't read."))?;
    let challenge = auth
        .challenge_relaxed()
        .map_err(|_| bad("Your browser sent a passkey we couldn't read."))?;
    let state = take_ceremony(app, challenge_key(challenge), None).await?;
    let server = DiscoverableAuthenticationServerState::decode(state.as_slice())
        .map_err(|_| internal("passkey state"))?;
    let row: Option<(i32, i32, Vec<u8>, Vec<u8>, Option<Vec<u8>>, bool)> = sqlx::query_as(
        "SELECT p.id, p.uid, p.static_state, p.dynamic_state, u.webauthn_handle, u.is_system
         FROM passkeys p JOIN users u ON u.uid = p.uid WHERE p.credential_id = $1",
    )
    .bind(auth.raw_id().as_ref())
    .fetch_optional(&app.db)
    .await?;
    let not_here = || {
        AppError::user(
            "That passkey isn't registered on this board. Sign in with your password, then add it in User CP → Security.",
        )
    };
    let (id, uid, static_state, dynamic_state, handle, is_system) = row.ok_or_else(not_here)?;
    if is_system {
        return Err(not_here());
    }
    let handle: [u8; 64] = handle
        .and_then(|h| h.as_slice().try_into().ok())
        .ok_or_else(not_here)?;
    let Ok(handle) = UserHandle64::decode(handle);
    if auth.response().user_handle() != &handle {
        return Err(not_here());
    }
    let static_state =
        StaticState::<CompressedPubKey<[u8; 32], [u8; 32], [u8; 48], Vec<u8>>>::decode(
            static_state.as_slice(),
        )
        .map_err(|_| internal("passkey key"))?;
    let dynamic_state = DynamicState::decode(
        dynamic_state
            .as_slice()
            .try_into()
            .map_err(|_| internal("passkey state"))?,
    )
    .map_err(|_| internal("passkey state"))?;
    let mut cred =
        AuthenticatedCredential::new(auth.raw_id(), &handle, static_state, dynamic_state)
            .map_err(|_| internal("passkey credential"))?;
    let origins = [rp.origin.as_str()];
    server
        .verify(
            &rp.id,
            &auth,
            &mut cred,
            &AuthenticationVerificationOptions::<&str, &str> {
                allowed_origins: &origins,
                ..Default::default()
            },
        )
        .map_err(|e| {
            tracing::info!(uid, error = %e, "passkey sign-in rejected");
            bad("The passkey couldn't be verified.")
        })?;
    let dynamic_state = cred
        .dynamic_state()
        .encode()
        .map_err(|e| internal(format!("passkey state: {e}")))?;
    sqlx::query("UPDATE passkeys SET dynamic_state = $2, last_used = $3 WHERE id = $1")
        .bind(id)
        .bind(dynamic_state.as_slice())
        .bind(now())
        .execute(&app.db)
        .await?;
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use super::relying_party;

    #[test]
    fn the_board_url_decides_where_passkeys_work() {
        let rp = relying_party("https://forum.example.org/community/").unwrap();
        assert_eq!(rp.origin, "https://forum.example.org");
        let rp = relying_party("https://Forum.Example.org:8443").unwrap();
        assert_eq!(rp.origin, "https://forum.example.org:8443");
        assert!(relying_party("http://localhost:8088").is_ok());
        assert!(
            relying_party("http://forum.example.org").is_err(),
            "not secure"
        );
        assert!(relying_party("https://203.0.113.5").is_err(), "IP address");
        assert!(relying_party("http://127.0.0.1:8080").is_err());
        assert!(relying_party("not a url").is_err());
    }
}
