//! Outgoing email: messages are queued in `mailqueue` (normally inside the transaction of the
//! change they report) and delivered by background workers as a leased job queue:
//!
//! 1. claim: one short statement leases a batch of due messages (`FOR UPDATE SKIP LOCKED`, so
//!    several workers never take the same message) and commits — no lock is held while sending;
//! 2. send: every claimed message at once (a batch is only as large as the worker sends in
//!    parallel), each with a hard timeout, so none waits out its lease before being sent;
//! 3. acknowledge: delete what was sent; reschedule failures with exponential backoff, or mark
//!    them `dead` after too many attempts or a permanent error (bad address, SMTP 5xx). Both
//!    check the lease token, so a worker that lost its lease leaves the message to its new owner.
//!
//! A worker that dies mid-batch leaves its lease to expire and the messages are claimed again.
//! The "log" handler writes mail to the server log instead of sending (local development).

use crate::app::App;
use crate::util::now;
use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;

/// Messages claimed and sent at once.
const BATCH: i64 = 8;
const LEASE_SECS: f64 = 300.0;
/// Each of connecting and sending is bounded by this, so a message finishes well inside its lease.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_ATTEMPTS: i32 = 8;

pub async fn queue(app: &App, to: &str, subject: &str, body: &str) {
    if let Err(e) = deliver(app, None, to, subject, body).await {
        tracing::error!("failed to queue mail: {e}");
    }
}

/// Queue a message. With a delivery key, a message already queued under that key (even one
/// sent and gone from the queue since) is not queued again.
pub async fn deliver(
    app: &App,
    key: Option<&str>,
    to: &str,
    subject: &str,
    body: &str,
) -> sqlx::Result<()> {
    let mut tx = app.db.begin().await?;
    if let Some(key) = key
        && !crate::infra::outbox::first_delivery(&mut tx, key).await?
    {
        return Ok(());
    }
    queue_in(&mut tx, to, subject, body).await?;
    tx.commit().await
}

/// Queue a message on `conn`, normally the transaction of the change it reports, so it is sent
/// if and only if that change commits.
pub async fn queue_in(
    conn: &mut sqlx::PgConnection,
    to: &str,
    subject: &str,
    body: &str,
) -> sqlx::Result<()> {
    if to.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO mailqueue (mailto, subject, message, dateline, idempotency_key) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(to)
    .bind(subject)
    .bind(body)
    .bind(now())
    .bind(uuid::Uuid::new_v4().simple().to_string())
    .execute(&mut *conn)
    .await?;
    sqlx::query("SELECT pg_notify('rbb_outbox', '')")
        .execute(conn)
        .await?;
    Ok(())
}

pub fn spawn_worker(app: App) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = app.shutdown.subscribe();
        loop {
            match deliver_batch(&app).await {
                Ok(n) if n as i64 == BATCH => continue,
                Ok(_) => {}
                Err(e) => tracing::warn!("mail delivery error: {e:#}"),
            }
            tokio::select! {
                _ = app.mail_wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                _ = stop.wait_for(|s| *s) => break,
            }
        }
    })
}

struct Claimed {
    mid: i64,
    to: String,
    subject: String,
    body: String,
    attempts: i32,
    key: String,
}

/// Lease a batch of due messages in one statement (its own short transaction).
async fn claim(app: &App, lease: uuid::Uuid) -> sqlx::Result<Vec<Claimed>> {
    let rows: Vec<(i64, String, String, String, i32, String)> = sqlx::query_as(
        "UPDATE mailqueue SET locked_until = now() + make_interval(secs => $2), lease = $3, attempts = attempts + 1
         WHERE mid IN (
             SELECT mid FROM mailqueue
             WHERE status = 'pending' AND available_at <= now() AND (locked_until IS NULL OR locked_until < now())
             ORDER BY available_at, mid LIMIT $1 FOR UPDATE SKIP LOCKED)
         RETURNING mid, mailto, subject, message, attempts, idempotency_key",
    )
    .bind(BATCH)
    .bind(LEASE_SECS)
    .bind(lease)
    .fetch_all(&app.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(mid, to, subject, body, attempts, key)| Claimed {
            mid,
            to,
            subject,
            body,
            attempts,
            key,
        })
        .collect())
}

/// Why a send failed: worth retrying, or never going to work.
enum SendError {
    Transient(String),
    Permanent(String),
}

type Transport = AsyncSmtpTransport<Tokio1Executor>;

fn transport(s: &crate::settings::Settings) -> anyhow::Result<Option<Transport>> {
    if s.get("mail_handler") != "smtp" {
        return Ok(None);
    }
    let host = s.get("smtp_host").to_string();
    let port = s.int("smtp_port") as u16;
    let builder = match s.get("secure_smtp") {
        "tls" => Transport::relay(&host)?.port(port),
        "starttls" => Transport::starttls_relay(&host)?.port(port),
        _ => Transport::builder_dangerous(&host).port(port),
    };
    let builder = if !s.get("smtp_user").is_empty() {
        // Anything but tls/starttls above is plaintext SMTP; never put a password on the wire
        // in the clear to a remote host.
        if !matches!(s.get("secure_smtp"), "tls" | "starttls") && !is_loopback_host(&host) {
            anyhow::bail!(
                "refusing to send SMTP credentials to {host} without encryption (set SMTP Encryption to STARTTLS or TLS)"
            );
        }
        builder.credentials(Credentials::new(
            s.get("smtp_user").to_string(),
            s.get("smtp_pass").to_string(),
        ))
    } else {
        builder
    };
    Ok(Some(builder.timeout(Some(SEND_TIMEOUT)).build()))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn send(
    t: Option<&Transport>,
    from: &Mailbox,
    domain: &str,
    m: &Claimed,
) -> Result<(), SendError> {
    let Some(t) = t else {
        // The body can carry reset and activation codes, so it is only logged on request (debug).
        tracing::info!(target: "mail", "[mail:log] To: {} Subject: {}", m.to, m.subject);
        tracing::debug!(target: "mail", "[mail:log] Body for {}:\n{}", m.to, m.body);
        return Ok(());
    };
    let to: Mailbox =
        m.to.parse()
            .map_err(|e| SendError::Permanent(format!("invalid recipient: {e}")))?;
    let msg = Message::builder()
        .from(from.clone())
        .to(to)
        .subject(m.subject.clone())
        .message_id(Some(format!("<{}@{domain}>", m.key)))
        .header(ContentType::TEXT_PLAIN)
        .body(m.body.clone())
        .map_err(|e| SendError::Permanent(e.to_string()))?;
    match tokio::time::timeout(SEND_TIMEOUT, t.send(msg)).await {
        Err(_) => Err(SendError::Transient("timed out".into())),
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) if e.is_permanent() => Err(SendError::Permanent(e.to_string())),
        Ok(Err(e)) => Err(SendError::Transient(e.to_string())),
    }
}

/// Record the result of sending, if this worker still holds the message's lease.
async fn acknowledge(
    app: &App,
    lease: uuid::Uuid,
    m: &Claimed,
    r: Result<(), SendError>,
) -> sqlx::Result<()> {
    let (error, dead) = match r {
        Ok(()) => {
            crate::infra::metrics::counter_with("rbb_mail_total", &[("result", "sent")], 1);
            sqlx::query("DELETE FROM mailqueue WHERE mid = $1 AND lease = $2")
                .bind(m.mid)
                .bind(lease)
                .execute(&app.db)
                .await?;
            return Ok(());
        }
        Err(SendError::Permanent(e)) => (e, true),
        Err(SendError::Transient(e)) => (e, m.attempts >= MAX_ATTEMPTS),
    };
    let result = if dead { "dead" } else { "retry" };
    crate::infra::metrics::counter_with("rbb_mail_total", &[("result", result)], 1);
    tracing::warn!(
        mid = m.mid,
        attempts = m.attempts,
        dead,
        "mail delivery failed: {error}"
    );
    sqlx::query(
        "UPDATE mailqueue SET locked_until = NULL, lease = NULL, lasterror = $2,
            available_at = now() + make_interval(secs => $3),
            status = CASE WHEN $4 THEN 'dead' ELSE 'pending' END
         WHERE mid = $1 AND lease = $5",
    )
    .bind(m.mid)
    .bind(error)
    .bind(crate::infra::outbox::backoff(m.attempts).as_secs_f64())
    .bind(dead)
    .bind(lease)
    .execute(&app.db)
    .await?;
    Ok(())
}

/// Claim, send and acknowledge one batch. Returns how many messages were claimed.
pub async fn deliver_batch(app: &App) -> anyhow::Result<usize> {
    use futures::StreamExt;
    let lease = uuid::Uuid::new_v4();
    let batch = claim(app, lease).await?;
    if batch.is_empty() {
        return Ok(0);
    }
    let s = app.cache().settings.clone();
    let from: Mailbox = format!(
        "{} <{}>",
        s.get("bbname").replace(['<', '>', '"'], ""),
        s.get("adminemail")
    )
    .parse()
    .unwrap_or_else(|_| "rbb <noreply@localhost>".parse().unwrap());
    let domain = from.email.domain().to_string();
    // The batch is already claimed (attempts bumped, lease held), so a bad configuration must go
    // through acknowledge() for backoff and eventual `dead`, not return early and be re-leased.
    let transport = transport(&s).map_err(|e| format!("{e:#}"));
    let n = batch.len();
    futures::stream::iter(batch)
        .for_each_concurrent(None, |m| {
            let (transport, from, domain) = (&transport, &from, &domain);
            async move {
                let r = match transport {
                    Ok(t) => send(t.as_ref(), from, domain, &m).await,
                    Err(e) => Err(SendError::Transient(e.clone())),
                };
                if let Err(e) = acknowledge(app, lease, &m, r).await {
                    // The lease expires and the message is retried.
                    tracing::warn!(mid = m.mid, "could not record mail result: {e}");
                }
            }
        })
        .await;
    Ok(n)
}

/// (pending, dead, age in seconds of the oldest pending message).
pub async fn stats(db: &sqlx::PgPool) -> sqlx::Result<(i64, i64, f64)> {
    sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status = 'pending'), COUNT(*) FILTER (WHERE status = 'dead'),
                COALESCE(EXTRACT(EPOCH FROM now() - to_timestamp(MIN(dateline) FILTER (WHERE status = 'pending'))), 0)::float8
         FROM mailqueue",
    )
    .fetch_one(db)
    .await
}
