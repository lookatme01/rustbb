//! Retention: IP addresses are shortened in budgeted runs that pick up where they left off.

mod common;

use std::time::Duration;

#[tokio::test]
async fn shortening_stops_at_its_time_budget_and_a_later_run_finishes() {
    let t = test_app!();
    let uid = t.create_user("oldposter", "Passw0rd-oldposter").await;
    let fid: i32 = sqlx::query_scalar("SELECT MIN(fid) FROM forums WHERE type = 'f'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, 'Old', $2, 'oldposter', 1, 1, 1) RETURNING tid",
    )
    .bind(fid)
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible, ipaddress) VALUES ($1, $2, 'Old', $3, 'oldposter', 100, 'text', 1, '203.0.113.77')")
        .bind(tid)
        .bind(fid)
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let ip = || async {
        sqlx::query_scalar::<_, String>("SELECT host(ipaddress) FROM posts WHERE dateline = 100")
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    };
    let cut = rbb::util::now() - 1000;
    // No time at all: nothing is touched and the run says it is not finished.
    let (n, complete) = rbb::privacy::shorten_ips_within(&t.db.pool, cut, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!((n, complete), (0, false));
    assert_eq!(ip().await, "203.0.113.77");
    // The next run, with time, deals with it and says so.
    let (n, complete) = rbb::privacy::shorten_ips_within(&t.db.pool, cut, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(complete && n >= 1, "{n} {complete}");
    assert_ne!(
        ip().await,
        "203.0.113.77",
        "the address no longer identifies a host"
    );
}
