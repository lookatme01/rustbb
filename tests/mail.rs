//! The leased mail queue.

mod common;

async fn queue(t: &common::TestApp, to: &str) {
    let mut c = t.db.pool.acquire().await.unwrap();
    rbb::mail::queue_in(&mut c, to, "Hello", "Body")
        .await
        .unwrap();
}

async fn set(t: &common::TestApp, k: &str, v: &str) {
    sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .bind(k)
        .bind(v)
        .execute(&t.db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn delivered_messages_are_removed() {
    let t = test_app!();
    set(&t, "mail_handler", "log").await;
    t.app.invalidate(&["settings"]).await.unwrap();
    queue(&t, "a@example.org").await;
    queue(&t, "b@example.org").await;
    assert_eq!(rbb::mail::deliver_batch(&t.app).await.unwrap(), 2);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mailqueue")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn unreachable_server_backs_off_without_holding_locks() {
    let t = test_app!();
    for (k, v) in [
        ("mail_handler", "smtp"),
        ("smtp_host", "127.0.0.1"),
        ("smtp_port", "1"),
        ("secure_smtp", "none"),
    ] {
        set(&t, k, v).await;
    }
    t.app.invalidate(&["settings"]).await.unwrap();
    queue(&t, "a@example.org").await;
    assert_eq!(rbb::mail::deliver_batch(&t.app).await.unwrap(), 1);
    let (attempts, status, later, err): (i32, String, bool, String) =
        sqlx::query_as("SELECT attempts, status, available_at > now(), lasterror FROM mailqueue")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(
        (attempts, status.as_str(), later),
        (1, "pending", true),
        "error: {err}"
    );
    // Nothing is locked while waiting for the retry.
    let locked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation WHERE c.relname = 'mailqueue' AND l.mode = 'RowShareLock'",
    )
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(locked, 0);
}

#[tokio::test]
async fn invalid_recipient_is_dead_at_once() {
    let t = test_app!();
    for (k, v) in [
        ("mail_handler", "smtp"),
        ("smtp_host", "127.0.0.1"),
        ("smtp_port", "1"),
        ("secure_smtp", "none"),
    ] {
        set(&t, k, v).await;
    }
    t.app.invalidate(&["settings"]).await.unwrap();
    queue(&t, "not an address").await;
    rbb::mail::deliver_batch(&t.app).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM mailqueue")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "dead");
}

#[tokio::test]
async fn concurrent_workers_never_claim_the_same_message() {
    let t = test_app!();
    set(&t, "mail_handler", "log").await;
    t.app.invalidate(&["settings"]).await.unwrap();
    for i in 0..120 {
        queue(&t, &format!("u{i}@example.org")).await;
    }
    // Each worker drains the queue a batch at a time; together they claim every message once.
    let drain = || async {
        let mut n = 0;
        loop {
            match rbb::mail::deliver_batch(&t.app).await.unwrap() {
                0 => return n,
                k => n += k,
            }
        }
    };
    let (a, b, c) = tokio::join!(drain(), drain(), drain());
    assert_eq!(a + b + c, 120);
}
