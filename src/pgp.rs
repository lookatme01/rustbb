//! OpenPGP support for private messages.
//!
//! Keys are generated and used in the browser (OpenPGP.js); the server never sees a private key
//! or passphrase. What the server does here is refuse anything it can check is wrong before it
//! reaches other members:
//!
//! * published keys must parse, carry valid self-signatures, be unexpired and unrevoked, use a
//!   modern algorithm, be able to sign and encrypt, and come with a proof-of-possession signature
//!   over a server challenge that names the account;
//! * signed messages must carry a detached signature, made by the sender's current key, over a
//!   canonical payload that matches what was submitted (sender, recipients, subject, body, time);
//! * encrypted messages must actually be encrypted to every recipient's current key.
//!
//! Recipients' browsers repeat every signature check themselves, so a verification badge never
//! depends on the server's word alone.

use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use pgp::crypto::public_key::PublicKeyAlgorithm;
use pgp::packet::{Packet, PacketParser, Signature};
use pgp::types::{KeyDetails, PublicParams};
use std::io::BufReader;

/// Largest armored public key accepted (keys with photo IDs or many certifications are larger
/// than anything the board needs; those parts are stripped anyway).
pub const MAX_KEY_LEN: usize = 64 * 1024;
/// Largest armored signature accepted.
pub const MAX_SIG_LEN: usize = 4 * 1024;
/// Largest encrypted message accepted (roughly 2× the plaintext limit, for armor and packets).
pub const MAX_MESSAGE_LEN: usize = 256 * 1024;
/// How far a signed payload's timestamp may drift from the server clock.
pub const MAX_CLOCK_SKEW: i64 = 600;

/// What the board records about a published key.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyInfo {
    /// Primary key fingerprint, upper-case hex (40 chars for v4 keys, 64 for v6).
    pub fingerprint: String,
    /// Human-readable algorithm, e.g. "Ed25519" or "RSA 4096".
    pub algorithm: String,
    pub created: i64,
    /// 0 = never expires.
    pub expires: i64,
    pub user_ids: Vec<String>,
    /// Key IDs (hex) of the subkeys (or primary) that may receive encrypted messages.
    pub encryption_key_ids: Vec<String>,
    /// The key re-armored without user attributes (photos) or third-party certifications.
    pub armored: String,
}

/// Upper-case hex key ID.
pub fn kid(id: &pgp::types::KeyId) -> String {
    hex::encode_upper(id.as_ref())
}

fn err(msg: impl Into<String>) -> String {
    msg.into()
}

/// Upper-case hex fingerprint of any key.
pub fn fingerprint_of(k: &impl KeyDetails) -> String {
    format!("{:X}", k.fingerprint())
}

/// Normalise a fingerprint typed or pasted by a person: strip spaces, upper-case.
pub fn normalize_fingerprint(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn algorithm_label(params: &PublicParams, alg: PublicKeyAlgorithm) -> Result<String, String> {
    use rsa::traits::PublicKeyParts;
    Ok(match params {
        PublicParams::RSA(p) => {
            let bits = p.key.size() * 8;
            if bits < 2048 {
                return Err(err(format!(
                    "RSA keys must be at least 2048 bits (this one is {bits})."
                )));
            }
            format!("RSA {bits}")
        }
        PublicParams::EdDSALegacy(_) | PublicParams::Ed25519(_) => "Ed25519".into(),
        PublicParams::Ed448(_) => "Ed448".into(),
        PublicParams::X25519(_) => "X25519".into(),
        PublicParams::X448(_) => "X448".into(),
        PublicParams::ECDH(p) => format!("ECDH {}", p.curve().name()),
        PublicParams::ECDSA(p) => format!("ECDSA {}", p.curve().name()),
        PublicParams::DSA(_) | PublicParams::Elgamal(_) => {
            return Err(err(format!(
                "{alg:?} keys are obsolete. Please use an Ed25519 (Curve25519) or RSA key."
            )));
        }
        #[allow(unreachable_patterns)]
        _ => format!("{alg:?}"),
    })
}

fn newest<'a>(sigs: impl Iterator<Item = &'a Signature>) -> Option<&'a Signature> {
    sigs.max_by_key(|s| s.created().map(|t| t.as_secs()).unwrap_or(0))
}

/// Parse, validate and normalise an armored public key.
pub fn parse_public_key(armored: &str, now: i64) -> Result<(SignedPublicKey, KeyInfo), String> {
    let armored = armored.trim();
    if armored.is_empty() {
        return Err(err("No public key was provided."));
    }
    if armored.len() > MAX_KEY_LEN {
        return Err(err("That key is too large."));
    }
    if armored.contains("PRIVATE KEY BLOCK") {
        return Err(err(
            "That is a private key. Only your public key is ever sent to the board.",
        ));
    }
    let (mut key, _) = SignedPublicKey::from_string(armored)
        .map_err(|e| err(format!("That doesn't look like an OpenPGP public key ({e}).")))?;
    // Drop what the board has no use for: photo IDs and certifications made by other keys.
    key.details.user_attributes.clear();
    let primary_id = key.primary_key.legacy_key_id();
    for u in key.details.users.iter_mut() {
        u.signatures
            .retain(|s| s.issuer_key_id().iter().any(|id| **id == primary_id) || s.issuer_key_id().is_empty());
    }
    key.verify_bindings()
        .map_err(|e| err(format!("The key's self-signatures are invalid ({e}).")))?;
    if !key.details.revocation_signatures.is_empty() {
        return Err(err("That key has been revoked."));
    }
    if key.details.users.is_empty() {
        return Err(err("That key has no user ID."));
    }

    let created = key.primary_key.created_at().as_secs() as i64;
    if created > now + MAX_CLOCK_SKEW {
        return Err(err("That key was created in the future. Check your device's clock."));
    }
    let algorithm = algorithm_label(key.primary_key.public_params(), key.primary_key.algorithm())?;

    // Expiry and capabilities come from the newest self-signature on the primary user ID.
    let user_sig = key
        .details
        .users
        .iter()
        .find(|u| u.is_primary())
        .or_else(|| key.details.users.first())
        .and_then(|u| newest(u.signatures.iter()))
        .ok_or_else(|| err("That key has no valid user ID self-signature."))?;
    let expires = user_sig
        .key_expiration_time()
        .map(|d| created + d.as_secs() as i64)
        .filter(|&e| e > created)
        .unwrap_or(0);
    if expires > 0 && expires <= now {
        return Err(err("That key has expired."));
    }
    let primary_flags = user_sig.key_flags();
    let mut can_sign = primary_flags.sign();
    let mut encryption_key_ids = vec![];
    if primary_flags.encrypt_comms() {
        encryption_key_ids.push(kid(&key.primary_key.legacy_key_id()));
    }
    for sk in &key.public_subkeys {
        let Some(binding) = newest(sk.signatures.iter()) else { continue };
        let sub_created = sk.key.created_at().as_secs() as i64;
        let sub_expired = binding
            .key_expiration_time()
            .map(|d| d.as_secs() as i64)
            .is_some_and(|d| d > 0 && sub_created + d <= now);
        if sub_expired {
            continue;
        }
        algorithm_label(sk.key.public_params(), sk.key.algorithm())?;
        let f = binding.key_flags();
        if f.sign() {
            can_sign = true;
        }
        if f.encrypt_comms() {
            encryption_key_ids.push(kid(&sk.key.legacy_key_id()));
        }
    }
    if !can_sign {
        return Err(err("That key can't make signatures (it has no signing key)."));
    }
    if encryption_key_ids.is_empty() {
        return Err(err(
            "That key can't receive encrypted messages (it has no encryption subkey).",
        ));
    }
    let user_ids = key
        .details
        .users
        .iter()
        .map(|u| String::from_utf8_lossy(u.id.id()).chars().take(200).collect())
        .collect();
    let normalised = key
        .to_armored_string(Default::default())
        .map_err(|e| err(format!("Could not re-encode the key ({e}).")))?;
    let info = KeyInfo {
        fingerprint: fingerprint_of(&key.primary_key),
        algorithm,
        created,
        expires,
        user_ids,
        encryption_key_ids,
        armored: normalised,
    };
    Ok((key, info))
}

/// Check a private-key backup before storing it: it must be the member's current key and every
/// secret part must be passphrase-protected, so the server never holds a usable private key.
pub fn check_backup(armored: &str, fingerprint: &str) -> Result<(), String> {
    use pgp::composed::SignedSecretKey;
    let armored = armored.trim();
    if armored.len() > MAX_KEY_LEN {
        return Err(err("That backup is too large."));
    }
    let (key, _) = SignedSecretKey::from_string(armored)
        .map_err(|e| err(format!("The key backup could not be read ({e}).")))?;
    if fingerprint_of(&key.primary_key) != fingerprint {
        return Err(err("The key backup is for a different key."));
    }
    let locked = key.primary_key.secret_params().is_encrypted()
        && key.secret_subkeys.iter().all(|k| k.key.secret_params().is_encrypted());
    if !locked {
        return Err(err(
            "Refusing to store a private key that isn't protected by a passphrase.",
        ));
    }
    Ok(())
}

/// Verify a detached signature over `data`, made by the primary key or a signing subkey of `key`.
/// The signature must be binary-mode (`createMessage({ binary })` in OpenPGP.js) or text-mode
/// over already-canonical text; both are accepted since both are unambiguous for our payloads.
pub fn verify_detached(key: &SignedPublicKey, armored_sig: &str, data: &[u8]) -> Result<(), String> {
    if armored_sig.len() > MAX_SIG_LEN {
        return Err(err("That signature is too large."));
    }
    let (sig, _) = DetachedSignature::from_string(armored_sig.trim())
        .map_err(|e| err(format!("The signature could not be read ({e}).")))?;
    let issuers: Vec<String> = sig
        .signature
        .issuer_fingerprint()
        .iter()
        .map(|f| format!("{f:X}"))
        .collect();
    let ids: Vec<String> = sig
        .signature
        .issuer_key_id()
        .iter()
        .map(|k| kid(k))
        .collect();
    let matches = |fpr: String, id: String| {
        (issuers.is_empty() && ids.is_empty()) || issuers.contains(&fpr) || ids.contains(&id)
    };
    if matches(
        fingerprint_of(&key.primary_key),
        kid(&key.primary_key.legacy_key_id()),
    ) && sig.verify(&key.primary_key, data).is_ok()
    {
        return Ok(());
    }
    for sk in &key.public_subkeys {
        let signing = newest(sk.signatures.iter()).is_some_and(|b| b.key_flags().sign());
        if signing
            && matches(fingerprint_of(&sk.key), kid(&sk.key.legacy_key_id()))
            && sig.verify(&sk.key, data).is_ok()
        {
            return Ok(());
        }
    }
    Err(err("The signature does not match."))
}

/// Parse an armored, encrypted OpenPGP message and return the key IDs (upper-case hex) it is
/// encrypted to. Fails unless it is a public-key encrypted message followed by encrypted data.
pub fn encrypted_recipients(armored: &str) -> Result<Vec<String>, String> {
    let armored = armored.trim();
    if armored.len() > MAX_MESSAGE_LEN {
        return Err(err("That encrypted message is too large."));
    }
    if !armored.starts_with("-----BEGIN PGP MESSAGE-----") {
        return Err(err("The encrypted message is not an armored OpenPGP message."));
    }
    let mut dearmor = pgp::armor::Dearmor::new(BufReader::new(armored.as_bytes()));
    dearmor
        .read_header()
        .map_err(|e| err(format!("The encrypted message is malformed ({e}).")))?;
    let mut ids = vec![];
    let mut has_data = false;
    for p in PacketParser::new(BufReader::new(dearmor)) {
        match p.map_err(|e| err(format!("The encrypted message is malformed ({e}).")))? {
            Packet::PublicKeyEncryptedSessionKey(k) => {
                if let Ok(id) = k.id() {
                    ids.push(kid(id));
                } else if let Ok(Some(f)) = k.fingerprint() {
                    // v6 PKESKs name the recipient by fingerprint; the key ID is its prefix/suffix.
                    ids.push(format!("{f:X}"));
                }
            }
            Packet::SymEncryptedProtectedData(_) => {
                has_data = true;
                break;
            }
            Packet::SymKeyEncryptedSessionKey(_) => {
                return Err(err("Password-encrypted messages are not accepted."));
            }
            _ => return Err(err("The encrypted message has unexpected contents.")),
        }
    }
    if !has_data || ids.is_empty() {
        return Err(err("The message is not encrypted to anyone."));
    }
    Ok(ids)
}

/// True if a PKESK recipient entry (key ID or fingerprint) addresses one of `key_ids`.
pub fn recipient_matches(entry: &str, key_ids: &[String]) -> bool {
    key_ids.iter().any(|id| entry == id || (entry.len() > 16 && (entry.ends_with(id.as_str()) || entry.starts_with(id.as_str()))))
}

// ------------------------------------------------------------------ signed statements
//
// Everything a member signs is a small UTF-8 document with a fixed header, so a signature made
// for one purpose can never be replayed as another. The browser builds the same bytes.

/// Proof that the uploader controls the key they are publishing, bound to this account.
pub fn key_proof_statement(board: &str, uid: i32, fingerprint: &str, challenge: &str) -> String {
    format!(
        "rbb-key-proof/v1\nboard: {board}\nuid: {uid}\nfingerprint: {fingerprint}\nchallenge: {challenge}\n"
    )
}

/// Statement by a member's previous key vouching for their new one.
pub fn key_transition_statement(board: &str, uid: i32, from: &str, to: &str) -> String {
    format!("rbb-key-transition/v1\nboard: {board}\nuid: {uid}\nfrom: {from}\nto: {to}\n")
}

/// Statement that `verifier` checked `subject`'s key out of band.
pub fn verification_statement(
    board: &str,
    verifier: i32,
    verifier_fpr: &str,
    subject: i32,
    subject_fpr: &str,
    ts: i64,
) -> String {
    format!(
        "rbb-verify/v1\nboard: {board}\nverifier: {verifier} {verifier_fpr}\nsubject: {subject} {subject_fpr}\nts: {ts}\n"
    )
}

/// The signed payload of a private message. Field order is fixed so both sides serialise it
/// identically; the browser signs `JSON.stringify` of an object built in this order.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MessagePayload {
    pub v: i32,
    pub t: String,
    pub board: String,
    pub from: i32,
    pub fpr: String,
    pub to: Vec<i32>,
    pub subject: String,
    pub body: String,
    pub ts: i64,
}

/// Line endings in submitted messages depend on the browser; signatures are over `\n`.
pub fn normalize_body(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Subjects are trimmed and cut to 120 characters (code points) before they are signed.
pub fn normalize_subject(s: &str) -> String {
    s.trim().chars().take(120).collect()
}

/// A time-limited challenge for key proofs, bound to the account: `<ts>.<mac>`.
pub fn make_challenge(secret: &str, uid: i32, ts: i64) -> String {
    let mac = crate::util::hmac_hex(secret, &format!("pgp-challenge:{uid}:{ts}"));
    format!("{ts}.{}", &mac[..32])
}

pub fn check_challenge(secret: &str, uid: i32, challenge: &str, now: i64) -> bool {
    let Some((ts, _)) = challenge.split_once('.') else { return false };
    let Ok(ts) = ts.parse::<i64>() else { return false };
    if now - ts > 1800 || ts - now > MAX_CLOCK_SKEW {
        return false;
    }
    crate::util::ct_eq(&make_challenge(secret, uid, ts), challenge)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUB: &str = include_str!("../tests/fixtures/pgp/pub.asc");
    const SIG_TEXT: &str = include_str!("../tests/fixtures/pgp/sig_text.asc");
    const SIG_BIN: &str = include_str!("../tests/fixtures/pgp/sig_bin.asc");
    const ENC: &str = include_str!("../tests/fixtures/pgp/enc.asc");
    const SEC: &str = include_str!("../tests/fixtures/pgp/sec_locked.asc");
    const FPR: &str = include_str!("../tests/fixtures/pgp/fpr.txt");
    const NOW: i64 = 1_900_000_000;

    #[test]
    fn parses_openpgpjs_key() {
        let (_, info) = parse_public_key(PUB, NOW).expect("valid key");
        assert_eq!(info.fingerprint, FPR.trim().to_ascii_uppercase());
        assert_eq!(info.algorithm, "Ed25519");
        assert_eq!(info.expires, 0);
        assert_eq!(info.encryption_key_ids.len(), 1);
        assert!(info.user_ids[0].contains("alice"));
        // The normalised key parses to the same fingerprint.
        let (_, again) = parse_public_key(&info.armored, NOW).unwrap();
        assert_eq!(again.fingerprint, info.fingerprint);
    }

    #[test]
    fn rejects_private_and_garbage() {
        assert!(parse_public_key("-----BEGIN PGP PRIVATE KEY BLOCK-----\n", NOW).is_err());
        assert!(parse_public_key("hello", NOW).is_err());
        assert!(parse_public_key("", NOW).is_err());
    }

    #[test]
    fn verifies_detached_signatures() {
        let (key, _) = parse_public_key(PUB, NOW).unwrap();
        assert!(verify_detached(&key, SIG_BIN, b"hello\nworld").is_ok());
        assert!(verify_detached(&key, SIG_BIN, b"hello\nworld!").is_err());
        // Text-mode signatures are made over CRLF-canonicalised text.
        assert!(verify_detached(&key, SIG_TEXT, b"hello\r\nworld").is_ok());
        assert!(verify_detached(&key, "junk", b"hello\nworld").is_err());
    }

    #[test]
    fn reads_encrypted_recipients() {
        let (_, info) = parse_public_key(PUB, NOW).unwrap();
        let ids = encrypted_recipients(ENC).expect("encrypted message");
        assert!(ids.iter().any(|e| recipient_matches(e, &info.encryption_key_ids)), "{ids:?} vs {:?}", info.encryption_key_ids);
        assert!(encrypted_recipients(PUB).is_err());
        assert!(encrypted_recipients("-----BEGIN PGP MESSAGE-----\n\nAAAA\n-----END PGP MESSAGE-----").is_err());
    }

    #[test]
    fn backups_must_be_locked_and_match() {
        let fpr = FPR.trim().to_ascii_uppercase();
        assert!(check_backup(SEC, &fpr).is_ok());
        assert!(check_backup(SEC, "00").is_err());
        assert!(check_backup(PUB, &fpr).is_err());
        let plain = include_str!("../tests/fixtures/pgp/sec_unlocked.asc");
        assert!(check_backup(plain, &fpr).unwrap_err().contains("passphrase"));
    }

    #[test]
    fn challenges_expire_and_bind_uid() {
        let c = make_challenge("s3cret-s3cret-s3cret-s3cret", 7, NOW);
        assert!(check_challenge("s3cret-s3cret-s3cret-s3cret", 7, &c, NOW + 60));
        assert!(!check_challenge("s3cret-s3cret-s3cret-s3cret", 8, &c, NOW + 60));
        assert!(!check_challenge("s3cret-s3cret-s3cret-s3cret", 7, &c, NOW + 3600));
        assert!(!check_challenge("s3cret-s3cret-s3cret-s3cret", 7, "garbage", NOW));
    }

    #[test]
    fn normalises_inputs() {
        assert_eq!(normalize_body("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(normalize_subject("  hi  "), "hi");
        assert_eq!(normalize_subject(&"é".repeat(130)).chars().count(), 120);
        assert_eq!(normalize_fingerprint("8d61 9b1e"), "8D619B1E");
    }
}
