//! `rbb migrate --check`: the rehearsal reports pending migrations, their locks and failures,
//! and never changes the database.

mod common;

use rbb::infra::migrations::{self, Outcome};
use sqlx::migrate::{Migration, MigrationType};
use std::time::Duration;

/// The migrations built into the binary plus `extra`, as a newer rbb would ship them.
fn with_extra(extra: &[(i64, &'static str)]) -> Vec<Migration> {
    let mut all: Vec<Migration> = sqlx::migrate!("./migrations").iter().cloned().collect();
    for (v, sql) in extra {
        all.push(Migration::new(
            *v,
            "extra".into(),
            MigrationType::Simple,
            (*sql).into(),
            false,
        ));
    }
    all
}

async fn column_exists(pool: &sqlx::PgPool, table: &str, col: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = $1 AND column_name = $2)",
    )
    .bind(table)
    .bind(col)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn up_to_date_database_has_nothing_pending() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let r = migrations::check(&db.pool, &with_extra(&[]), true, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(r.ok());
    assert!(r.steps.is_empty());
    assert!(r.problems.is_empty());
}

#[tokio::test]
async fn rehearsal_reports_locks_and_rolls_back() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let ms = with_extra(&[
        (
            9001,
            "ALTER TABLE posts ADD COLUMN rehearsal_probe int NOT NULL DEFAULT 0",
        ),
        (9002, "CREATE TABLE rehearsal_new (id int PRIMARY KEY)"),
    ]);
    let r = migrations::check(&db.pool, &ms, true, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.steps.len(), 2);
    let Outcome::Applied { locks, .. } = &r.steps[0].outcome else {
        panic!("{:?}", r.steps[0])
    };
    assert!(
        locks
            .iter()
            .any(|l| l.table == "posts" && l.mode == "AccessExclusiveLock"),
        "{locks:?}"
    );
    // A table the migration creates blocks no one.
    let Outcome::Applied { locks, .. } = &r.steps[1].outcome else {
        panic!("{:?}", r.steps[1])
    };
    assert!(
        locks.iter().all(|l| l.table != "rehearsal_new"),
        "{locks:?}"
    );

    assert!(!column_exists(&db.pool, "posts", "rehearsal_probe").await);
    let created: bool = sqlx::query_scalar("SELECT to_regclass('rehearsal_new') IS NOT NULL")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(!created);
    let recorded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version > 9000")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(recorded, 0);
}

#[tokio::test]
async fn failing_migration_stops_the_rehearsal() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let ms = with_extra(&[
        (9001, "ALTER TABLE posts ADD COLUMN probe_a int"),
        (9002, "ALTER TABLE no_such_table ADD COLUMN x int"),
        (9003, "ALTER TABLE posts ADD COLUMN probe_b int"),
    ]);
    let r = migrations::check(&db.pool, &ms, true, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(!r.ok());
    assert!(matches!(r.steps[0].outcome, Outcome::Applied { .. }));
    assert!(matches!(&r.steps[1].outcome, Outcome::Failed(e) if e.contains("no_such_table")));
    assert_eq!(r.steps[2].outcome, Outcome::Skipped);
    assert!(!column_exists(&db.pool, "posts", "probe_a").await);
}

#[tokio::test]
async fn busy_table_times_out_instead_of_waiting() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    // Another session holds a lock that conflicts with ALTER TABLE.
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE posts IN ACCESS SHARE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let ms = with_extra(&[(9001, "ALTER TABLE posts ADD COLUMN probe int")]);
    let r = migrations::check(&db.pool, &ms, true, Duration::from_millis(200))
        .await
        .unwrap();
    assert!(
        matches!(&r.steps[0].outcome, Outcome::Failed(e) if e.contains("lock timeout")),
        "{:?}",
        r.steps[0]
    );
    holder.rollback().await.unwrap();
}

#[tokio::test]
async fn database_from_a_newer_binary_is_a_problem_and_is_not_rehearsed() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let mut ms = with_extra(&[]);
    let newest = ms.pop().unwrap();
    let ms = {
        let mut v = ms;
        v.push(Migration::new(
            9001,
            "extra".into(),
            MigrationType::Simple,
            "SELECT 1".into(),
            false,
        ));
        v
    };
    let r = migrations::check(&db.pool, &ms, true, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(!r.ok());
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains(&newest.version.to_string()) && p.contains("newer rbb")),
        "{:?}",
        r.problems
    );
    assert_eq!(r.steps[0].outcome, Outcome::Skipped);
}
