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

#[tokio::test]
async fn a_partly_failed_job_is_kept_and_its_retry_skips_what_was_delivered() {
    let t = test_app!();
    for (k, v) in [
        ("enablepms", "1"),
        ("system_welcome_pm", "1"),
        ("system_welcome_subject", "Welcome"),
        ("system_welcome_message", "Hello {username}"),
    ] {
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
            .bind(k)
            .bind(v)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    t.app.invalidate(&["settings"]).await.unwrap();
    let alice = t.create_user("alice", "password123").await;
    // The second member does not exist yet, so their message cannot be created.
    let job = Job::WelcomePm {
        members: vec![(alice, "alice".into()), (alice + 1000, "bob".into())],
    };
    let mut tx = t.db.pool.begin().await.unwrap();
    outbox::enqueue(&mut tx, &job, None).await.unwrap();
    tx.commit().await.unwrap();
    let welcomes = |uid: i32| {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM privatemessages WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
    };

    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 1);
    let (status, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM outbox")
            .fetch_one(&t.db.pool)
            .await
            .expect("the failed job is kept");
    assert_eq!(status, "pending");
    assert!(last_error.unwrap().contains("uid"));
    assert_eq!(welcomes(alice).await.unwrap(), 1);

    // Bob registers; the retry welcomes him and not Alice again.
    sqlx::query(
        "INSERT INTO users (uid, username, password, email, usergroup, regdate, lastactive, lastvisit, pmfolders)
         VALUES ($1, 'bob', '', 'bob@example.com', 2, 1, 1, 1, '[]')",
    )
    .bind(alice + 1000)
    .execute(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE outbox SET available_at = now()")
        .execute(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 1);
    assert_eq!(welcomes(alice).await.unwrap(), 1, "not delivered twice");
    assert_eq!(welcomes(alice + 1000).await.unwrap(), 1);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn live_leases_are_not_claimed_again() {
    let t = test_app!();
    sqlx::query(
        "INSERT INTO outbox (kind, payload, locked_until, lease, attempts) VALUES ('hook', '{\"kind\":\"hook\",\"name\":\"x\",\"data\":{}}', now() + interval '1 minute', gen_random_uuid(), 1)",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(outbox::run_batch(&t.app).await.unwrap(), 0);
}

#[tokio::test]
async fn overlapping_cache_reloads_keep_each_others_changes() {
    let t = test_app!();
    for round in 0..30 {
        let name = format!("Board {round}");
        let custom = round % 2 == 0;
        sqlx::query("UPDATE settings SET value = $1 WHERE name = 'bbname'")
            .bind(&name)
            .execute(&t.db.pool)
            .await
            .unwrap();
        let sql = if custom {
            "INSERT INTO forumpermissions (fid, gid, perms) VALUES (2, 1, '{}') ON CONFLICT DO NOTHING"
        } else {
            "DELETE FROM forumpermissions WHERE fid = 2 AND gid = 1"
        };
        sqlx::query(sql).execute(&t.db.pool).await.unwrap();
        // Each starts from the current snapshot; neither may put back what the other replaced.
        let (a, b) = tokio::join!(
            t.app.invalidate(&["settings"]),
            t.app.invalidate(&["forumperms"])
        );
        a.unwrap();
        b.unwrap();
        let c = t.app.cache();
        assert_eq!(
            c.settings.get("bbname"),
            name,
            "round {round}: settings reverted"
        );
        assert_eq!(
            c.forum_perms.contains_key(&(2, 1)),
            custom,
            "round {round}: forum permissions reverted"
        );
    }
}

#[tokio::test]
async fn a_failed_cache_reload_marks_the_cache_stale_until_a_retry_succeeds() {
    let t = test_app!();
    assert_eq!(t.app.cache_stale_for(), None);
    sqlx::query("ALTER TABLE forumpermissions RENAME TO forumpermissions_away")
        .execute(&t.db.pool)
        .await
        .unwrap();
    assert!(t.app.invalidate(&["forumperms"]).await.is_err());
    assert!(
        t.app.cache_stale_for().is_some(),
        "stale while the reload fails"
    );
    let mut conn = t.db.pool.acquire().await.unwrap();
    rbb::infra::cluster::catch_up(&t.app, &mut conn)
        .await
        .unwrap();
    assert!(
        t.app.cache_stale_for().is_some(),
        "still stale: the retry failed too"
    );

    sqlx::query("ALTER TABLE forumpermissions_away RENAME TO forumpermissions")
        .execute(&t.db.pool)
        .await
        .unwrap();
    rbb::infra::cluster::catch_up(&t.app, &mut conn)
        .await
        .unwrap();
    assert_eq!(
        t.app.cache_stale_for(),
        None,
        "current again once the retry applies"
    );
}
