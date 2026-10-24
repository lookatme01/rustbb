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
    // No session-level advisory lock is left behind on any pooled connection. pg_locks is
    // cluster-wide, so only look at this test's database: tests running in parallel take their
    // own (transaction-scoped) advisory locks.
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory'
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
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
    let msg = rbb::tasks::run_task(&t.app, tid, "banlifter", true)
        .await
        .unwrap();
    assert_eq!(msg, "lifted 0 bans");
    assert_eq!(logged().await, 1);
}

#[tokio::test]
async fn expiry_tasks_restore_groups_and_take_warning_points_off() {
    let t = test_app!();
    let uid = t.create_user("expiring", "Passw0rd-expiring").await;
    let now = rbb::util::now();
    // Banned until an hour ago: the banned group comes off and the old group comes back.
    sqlx::query("UPDATE users SET usergroup = 7, warningpoints = 15 WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO banned (uid, gid, oldgroup, admin, dateline, lifted) VALUES ($1, 7, 2, 1, $2, $3)")
        .bind(uid)
        .bind(now - 86400)
        .bind(now - 3600)
        .execute(&t.db.pool)
        .await
        .unwrap();
    // Two warnings: one has run out, one has not.
    sqlx::query("INSERT INTO warnings (uid, title, points, dateline, issuedby, expires) VALUES ($1, 'old', 5, $2, 1, $3), ($1, 'live', 10, $2, 1, $4)")
        .bind(uid)
        .bind(now - 86400)
        .bind(now - 60)
        .bind(now + 86400)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let run = |key: &'static str| {
        let (app, pool) = (t.app.clone(), t.db.pool.clone());
        async move {
            let tid: i32 = sqlx::query_scalar("SELECT tid FROM tasks WHERE key = $1")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
            rbb::tasks::run_task(&app, tid, key, true).await.unwrap()
        }
    };
    assert_eq!(run("banlifter").await, "lifted 1 bans");
    assert_eq!(run("warnings").await, "expired 1 warnings");
    let (group, points): (i32, i32) =
        sqlx::query_as("SELECT usergroup, warningpoints FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((group, points), (2, 10));
    let bans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banned WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(bans, 0);
    // Running again changes nothing.
    assert_eq!(run("warnings").await, "expired 0 warnings");
}

#[tokio::test]
async fn pruning_unactivated_accounts_keeps_the_board_counters_pointing_at_real_members() {
    let t = test_app!();
    let now = rbb::util::now();
    let keeper = t.create_user("activated", "Passw0rd-activated").await;
    let stale = t.create_user("neveractivated", "Passw0rd-stale").await;
    let with_thread = t.create_user("hasthread", "Passw0rd-hasthread").await;
    for uid in [stale, with_thread] {
        sqlx::query("UPDATE users SET usergroup = 5, regdate = $2 WHERE uid = $1")
            .bind(uid)
            .bind(now - 40 * 86400)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    // A pending thread by an unactivated account: its author must not vanish from under it.
    sqlx::query("INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES (2, 'Pending', $1, 'hasthread', 1, 1, 0)")
        .bind(with_thread)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE counters SET lastuid = $1, lastusername = 'neveractivated' WHERE id = 1")
        .bind(stale)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let tid: i32 = sqlx::query_scalar("SELECT tid FROM tasks WHERE key = 'userpruning'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let msg = rbb::tasks::run_task(&t.app, tid, "userpruning", true)
        .await
        .unwrap();
    assert_eq!(msg, "pruned 1 unactivated users");
    let (lastuid, lastname): (i32, String) =
        sqlx::query_as("SELECT lastuid, lastusername FROM counters WHERE id = 1")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_ne!(lastuid, stale);
    assert!(
        !lastname.is_empty() && lastname != "neveractivated",
        "{lastname}"
    );
    let survivors: Vec<i32> =
        sqlx::query_scalar("SELECT uid FROM users WHERE uid = ANY($1) ORDER BY uid")
            .bind(vec![keeper, stale, with_thread])
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(survivors, vec![keeper, with_thread]);
}
