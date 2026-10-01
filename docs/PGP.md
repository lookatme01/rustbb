# End-to-end identity for private messages

Members can give their account an OpenPGP identity key, then sign and encrypt private messages
and verify the people they talk to. This page covers how that works and what it does and doesn't
protect against.

## Pieces

| Where | What |
|---|---|
| `static/js/pgp/core.mjs` | Key storage (IndexedDB), unlocking, signing/encryption, signed statements, safety numbers, and a DOM-only MyCode renderer for decrypted messages. |
| `static/js/pgp/ui.mjs`, `static/js/pgp.mjs` | Dialogs and the page controllers: User CP settings, verify screen, compose, read. |
| `static/js/vendor/` | OpenPGP.js 6 and qrcode-generator, vendored (see the README there). |
| `src/pgp.rs` | Server-side checks with rPGP: key validation, detached signatures, encrypted-message recipients, backups, challenges. |
| `src/routes/pgp.rs` | `/pgp/*` JSON API, `/usercp/pgp`, `/pm/verify/{uid}`, `/user/{uid}/pgp.asc`. |
| `src/routes/private.rs` | Accepts signed (`pgp = 1`) and encrypted (`pgp = 2`) messages. |
| `migrations/0007_pgp.sql` | `pgp_keys` (all keys, current and past), `pgp_verifications`, message columns. |

## Keys

* Generated in the browser: Curve25519 (Ed25519 + X25519, v4 format, so GnuPG can import it). The
  private key is locked with the member's passphrase using Argon2 S2K + AEAD and kept in IndexedDB.
* *Remember on this device* also keeps an unlocked copy sealed with a **non-extractable** AES-GCM
  WebCrypto key, so it works without the passphrase but can't be copied out of browser storage.
* Publishing sends the public key plus a signature over
  `rbb-key-proof/v1 · board · uid · fingerprint · challenge`, where the challenge is an HMAC token
  bound to the account and valid for 30 minutes. The server parses the key, checks its
  self-signatures, expiry, revocation, algorithm (no DSA/ElGamal, RSA ≥ 2048), that it can sign and
  encrypt, and that it isn't another account's active key. Photo IDs and third-party
  certifications are stripped.
* The optional backup is the passphrase-locked private key. The server stores it only if it
  matches the active key and every secret part is encrypted.
* Replacing a key keeps the old one in history (so old signatures still verify). If the old key
  is still unlocked, it signs `rbb-key-transition/v1` vouching for the new one. Everyone who
  verified the member gets an alert. Revoking needs the account password.

## Messages

The signed payload is canonical JSON:
`{"v":1,"t":"rbb-pm","board","from","fpr","to":[sorted uids],"subject","body","ts"}`.

* **Signed** (`pgp = 1`): the body is stored in plain text (and still rendered as MyCode by the server),
  with the payload and a detached binary signature. On send the server requires that the payload
  matches the submitted sender, current key, To list, subject, body (line endings normalised) and a
  timestamp within ten minutes, and that the signature verifies.
* **Encrypted** (`pgp = 2`): the message column holds an armored OpenPGP message whose signed
  plaintext is the payload. The server checks that it is encrypted to every recipient's current key
  and to the sender's own key (for the Sent copy). The browser decrypts it and renders it with a safe
  MyCode subset. Subjects are not encrypted. Replies, forwards and drafts decrypt and quote in the browser,
  and the editor's server preview and local autosave are turned off while encrypting.
* BCC isn't available for protected messages, because the signed recipient list would reveal BCC recipients.

When reading, the browser fetches the sender's key history and checks everything itself: the
signature, that the signing key belongs to the sender, and that the payload matches the displayed
sender, subject and body, is addressed to the reader, is for this board, and was signed when it was
sent. The result is one of: **Verified**, **Signed** (not yet verified), **Key changed**,
or **Signature problem**.

## Verification

Both members open `/pm/verify/{uid}`. Each browser derives a 60-digit safety number from both
members' uid and fingerprint (iterated SHA-512, as in Signal), which is identical on both sides only
if each sees the other's real key. They compare it aloud, scan each other's QR code
(`BarcodeDetector`, where supported), or paste the code sent over another channel. *Mark as
verified* signs `rbb-verify/v1 · board · verifier uid+fpr · subject uid+fpr · ts` with the
verifier's key. The server checks that signature, and the verifier's browser re-checks it before
showing anyone as verified, so the server can't invent verifications. Browsers also pin the first key
they see for each contact, and warn if it changes without a verification.

## Threat model

Protects against: reading or editing stored messages (database access, backups, rogue staff),
impersonation through a stolen account password (no passphrase means no signing key), messages
replayed to other people or boards, and an attacker who swaps keys, provided the members verified
each other.

Doesn't protect against: a compromised server that ships malicious JavaScript to a member's
browser. This is inherent to web-based cryptography, so members who need more assurance can export
their key and use a desktop OpenPGP app. It also doesn't hide subjects, who talks to whom, or
when. Pages that load the crypto code get `script-src 'self' 'wasm-unsafe-eval'` (for Argon2's
WebAssembly); JavaScript `eval` stays blocked.

## Tests

* `cargo test pgp::` covers key parsing and validation, signatures, recipients, backups and challenges
  against OpenPGP.js fixtures in `tests/fixtures/pgp/`.
* `node tests/pgp_e2e.mjs [base_url]` drives a running server with the shipped browser module:
  publishing, forgery and replay attempts, signed/encrypted messages, tampering detection,
  verification, rotation and revocation. It needs members `alice_pgp`, `bob_pgp` and `mallory_pgp`,
  and a local `psql` to reset them.
