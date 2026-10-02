//! Invariants the database enforces, under concurrency.

mod common;

#[tokio::test]
async fn simultaneous_reports_open_one_report() {
    let t = test_app!();
    let pid: i32 = sqlx::query_scalar("SELECT MIN(pid) FROM posts")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let rid: i32 = sqlx::query_scalar("SELECT MIN(rid) FROM reportreasons")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let mut clients = vec![];
    for i in 0..6 {
        let uid = t
            .create_user(&format!("reporter{i}"), "Passw0rd-reporter")
            .await;
        clients.push(t.login_as(uid).await);
    }
    let pid_s = pid.to_string();
    let rid_s = rid.to_string();
    let form = [
        ("type", "post"),
        ("id", pid_s.as_str()),
        ("reason", rid_s.as_str()),
        ("comment", "spam"),
    ];
    futures::future::join_all(clients.iter().map(|c| c.post_form("/report", &form))).await;
    // The same member again changes nothing.
    clients[0].post_form("/report", &form).await;
    let open: Vec<(i32, Vec<i32>)> = sqlx::query_as(
        "SELECT reports, reporters FROM reportedcontent WHERE type = 'post' AND id = $1 AND reportstatus = 0",
    )
    .bind(pid)
    .fetch_all(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].0, 6);
    assert_eq!(open[0].1.len(), 6);
}

#[tokio::test]
async fn a_member_rates_a_thread_once() {
    let t = test_app!();
    let tid: i32 = sqlx::query_scalar("SELECT MIN(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let uid = t.create_user("rater", "Passw0rd-rater").await;
    let c = t.login_as(uid).await;
    let path = format!("/thread/{tid}/rate");
    let futs = (0..5).map(|_| c.post_form(&path, &[("rating", "4")]));
    futures::future::join_all(futs).await;
    let (n, total): (i32, i32) =
        sqlx::query_as("SELECT numratings, totalratings FROM threads WHERE tid = $1")
            .bind(tid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((n, total), (1, 4));
}
