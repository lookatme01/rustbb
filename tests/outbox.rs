//! The transactional outbox and the cluster invalidation log.

mod common;

use rbb::infra::outbox::{self, Job};

#[tokio::test]
async fn jobs_exist_only_when_their_transaction_commits() {
    let t = test_app!();
    let job = Job::Hook {
        name: "nothing_listens".into(),
        data: serde_json::json!({}),
    };
    let mut tx = t.db.pool.begin().await.unwrap();
    outbox::enqueue(&mut tx, &job, None).await.unwrap();
    tx.rollback().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);

    let mut tx = t.db.pool.begin().await.unwrap();
    outbox::enqueue(&mut tx, &job, Some("once")).await.unwrap();
    outbox::enqueue(&mut tx, &job, Some("once")).await.unwrap();
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "the idempotency key deduplicates");

    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 1);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "finished jobs are removed");
}

#[tokio::test]
async fn failing_jobs_back_off_and_eventually_die() {
    let t = test_app!();
    sqlx::query("INSERT INTO outbox (kind, payload) VALUES ('bogus', '{\"kind\": \"bogus\"}')")
        .execute(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 1);
    let (attempts, status, later, locked): (i32, String, bool, bool) = sqlx::query_as(
        "SELECT attempts, status, available_at > now(), locked_until IS NOT NULL FROM outbox",
    )
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (attempts, status.as_str(), later, locked),
        (1, "pending", true, false)
    );
    // Not due yet: nothing claimed.
    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 0);
    sqlx::query("UPDATE outbox SET available_at = now(), attempts = $1")
        .bind(outbox::MAX_ATTEMPTS - 1)
        .execute(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 1);
    let status: String = sqlx::query_scalar("SELECT status FROM outbox")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "dead");
}

#[tokio::test]
async fn expired_leases_are_reclaimed() {
    let t = test_app!();
    sqlx::query(
        "INSERT INTO outbox (kind, payload, locked_until, attempts) VALUES ('hook', '{\"kind\":\"hook\",\"name\":\"x\",\"data\":{}}', now() - interval '1 second', 1)",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(
        outbox::run_batch(&t.app).await.unwrap(),
        1,
        "a crashed worker's job runs again"
    );
}

#[tokio::test]
async fn cache_invalidations_are_logged_for_other_nodes() {
    let t = test_app!();
    t.app.invalidate(&["forums"]).await.unwrap();
    let (origin, kind, payload): (String, String, serde_json::Value) =
        sqlx::query_as("SELECT origin, kind, payload FROM cluster_events ORDER BY id DESC LIMIT 1")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(origin, t.app.node_id);
    assert_eq!(kind, "cache");
    assert_eq!(payload["parts"], serde_json::json!(["forums"]));
}
