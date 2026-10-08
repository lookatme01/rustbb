//! Scheduled task leases.

mod common;

#[tokio::test]
async fn a_task_runs_once_when_two_schedulers_race() {
    let t = test_app!();
    let tid: i32 = sqlx::query_scalar("SELECT tid FROM tasks WHERE key = 'dailystats'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    // Only this task is due.
    sqlx::query("UPDATE tasks SET nextrun = CASE WHEN tid = $1 THEN 0 ELSE $2 END, logging = TRUE")
        .bind(tid)
        .bind(rbb::util::now() + 86400)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let (a, b) = tokio::join!(rbb::tasks::run_due(&t.app), rbb::tasks::run_due(&t.app));
    a.unwrap();
    b.unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasklog WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(runs, 1);
    let (locked, next): (bool, i64) =
        sqlx::query_as("SELECT locked_until IS NOT NULL, nextrun FROM tasks WHERE tid = $1")
            .bind(tid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert!(!locked, "the lease is released");
    assert!(next > rbb::util::now(), "the next run is scheduled");
    // No session-level advisory lock is left behind on any pooled connection.
    let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(held, 0);
}

#[tokio::test]
async fn idle_scheduled_runs_are_not_logged_but_manual_runs_are() {
    let t = test_app!();
    let tid: i32 = sqlx::query_scalar("SELECT tid FROM tasks WHERE key = 'banlifter'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET nextrun = CASE WHEN tid = $1 THEN 0 ELSE $2 END, logging = TRUE")
        .bind(tid)
        .bind(rbb::util::now() + 86400)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let logged = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tasklog WHERE tid = $1")
            .bind(tid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    };
    // No bans to lift: the run happens but leaves no log row.
    rbb::tasks::run_due(&t.app).await.unwrap();
    let lastrun: i64 = sqlx::query_scalar("SELECT lastrun FROM tasks WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(lastrun > 0, "the task ran");
    assert_eq!(logged().await, 0);
    let msg = rbb::tasks::run_task(&t.app, tid, "banlifter", 300, true)
        .await
        .unwrap();
    assert_eq!(msg, "lifted 0 bans");
    assert_eq!(logged().await, 1);
}
