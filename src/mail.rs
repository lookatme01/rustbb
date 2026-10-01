//! Outgoing email: messages are queued in `mailqueue` and delivered by a background worker
//! (SKIP LOCKED makes this safe with multiple nodes). The "log" handler writes mail to the
//! server log instead of sending — useful for local development.

use crate::app::App;
use crate::util::now;
use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;

pub async fn queue(app: &App, to: &str, subject: &str, body: &str) {
    if to.is_empty() {
        return;
    }
    let r = sqlx::query(
        "INSERT INTO mailqueue (mailto, subject, message, dateline) VALUES ($1, $2, $3, $4)",
    )
    .bind(to)
    .bind(subject)
    .bind(body)
    .bind(now())
    .execute(&app.db)
    .await;
    if let Err(e) = r {
        tracing::error!("failed to queue mail: {e}");
    }
}

pub fn spawn_worker(app: App) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            if let Err(e) = deliver_batch(&app).await {
                tracing::warn!("mail delivery error: {e:#}");
            }
        }
    });
}

async fn deliver_batch(app: &App) -> anyhow::Result<()> {
    let mut tx = app.db.begin().await?;
    let rows: Vec<(i64, String, String, String, i32)> = sqlx::query_as(
        "SELECT mid, mailto, subject, message, attempts FROM mailqueue WHERE attempts < 5 ORDER BY mid LIMIT 50 FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let s = app.cache().settings.clone();
    let from: Mailbox = format!(
        "{} <{}>",
        s.get("bbname").replace(['<', '>', '"'], ""),
        s.get("adminemail")
    )
    .parse()
    .unwrap_or_else(|_| "rbb <noreply@localhost>".parse().unwrap());
    let transport = if s.get("mail_handler") == "smtp" {
        let host = s.get("smtp_host").to_string();
        let port = s.int("smtp_port") as u16;
        let builder = match s.get("secure_smtp") {
            "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?.port(port),
            "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?.port(port),
            _ => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host).port(port),
        };
        let builder = if !s.get("smtp_user").is_empty() {
            builder.credentials(Credentials::new(
                s.get("smtp_user").to_string(),
                s.get("smtp_pass").to_string(),
            ))
        } else {
            builder
        };
        Some(builder.timeout(Some(Duration::from_secs(20))).build())
    } else {
        None
    };
    for (mid, to, subject, body, _attempts) in rows {
        let result: anyhow::Result<()> = async {
            match &transport {
                None => {
                    tracing::info!(target: "mail", "[mail:log] To: {to}\nSubject: {subject}\n\n{body}");
                    Ok(())
                }
                Some(t) => {
                    let msg = Message::builder()
                        .from(from.clone())
                        .to(to.parse()?)
                        .subject(subject.clone())
                        .header(ContentType::TEXT_PLAIN)
                        .body(body.clone())?;
                    t.send(msg).await?;
                    Ok(())
                }
            }
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("DELETE FROM mailqueue WHERE mid = $1")
                    .bind(mid)
                    .execute(&mut *tx)
                    .await?;
            }
            Err(e) => {
                sqlx::query(
                    "UPDATE mailqueue SET attempts = attempts + 1, lasterror = $2 WHERE mid = $1",
                )
                .bind(mid)
                .bind(e.to_string())
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}
