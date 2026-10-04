mod common;

#[tokio::test]
async fn concurrent_ratings_keep_one_row_and_an_accurate_total() {
    let t = test_app!();
    let uid = t.create_user("rated", "Passw0rd-rated").await;
    let c = t.login_as(1).await;
    let path = format!("/reputation/{uid}/add");
    for r in
        futures::future::join_all((0..8).map(|_| c.post_form(&path, &[("reputation", "1")]))).await
    {
        assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    }
    let (count, total): (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), SUM(reputation)::bigint FROM reputation WHERE uid = $1")
            .bind(uid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!((count, total), (1, 1));
    let cached: i32 = sqlx::query_scalar("SELECT reputation FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(cached, 1);
    let r = c.post_form(&path, &[("reputation", "-2147483648")]).await;
    assert!(r.status.is_client_error(), "{}", r.status);
}

#[tokio::test]
async fn reputation_does_not_disclose_or_rate_hidden_posts() {
    let t = test_app!();
    let uid = t.create_user("author", "Passw0rd-author").await;
    let tid: i32 = sqlx::query_scalar("INSERT INTO threads (fid, uid, subject, dateline) VALUES (3, $1, 'Confidential zebra', 1) RETURNING tid")
        .bind(uid).fetch_one(&t.db.pool).await.unwrap();
    let pid: i32 = sqlx::query_scalar("INSERT INTO posts (tid, fid, uid, subject, message, dateline) VALUES ($1, 3, $2, 'Confidential zebra', 'Hidden message', 1) RETURNING pid")
        .bind(tid).bind(uid).fetch_one(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO reputation (uid, adduid, pid, reputation, dateline, comments) VALUES ($1, 1, $2, 1, 1, 'Thanks')")
        .bind(uid).bind(pid).execute(&t.db.pool).await.unwrap();
    let c = t.client();
    let path = format!("/reputation/{uid}");
    assert!(c.get(&path).await.body.contains("Confidential zebra"));
    sqlx::query("UPDATE posts SET visible = -1 WHERE pid = $1")
        .bind(pid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    assert!(!c.get(&path).await.body.contains("Confidential zebra"));
    let other = t.create_user("other", "Passw0rd-other").await;
    let r = t
        .login_as(other)
        .await
        .get(&format!("{path}/add?pid={pid}"))
        .await;
    assert_eq!(r.status, 404);
    sqlx::query("UPDATE posts SET visible = 1 WHERE pid = $1")
        .bind(pid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE forums SET active = FALSE WHERE fid = 3")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    assert!(!c.get(&path).await.body.contains("Confidential zebra"));
}

#[tokio::test]
async fn concurrent_ratings_cannot_exceed_the_daily_limit() {
    let t = test_app!();
    sqlx::query(
        "UPDATE usergroups SET perms = perms || '{\"maxreputationsday\":1}'::jsonb WHERE gid = 2",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let giver = t.create_user("giver", "Passw0rd-giver").await;
    let c = t.login_as(giver).await;
    let mut paths = vec![];
    for i in 0..4 {
        let uid = t
            .create_user(&format!("recipient{i}"), "Passw0rd-recipient")
            .await;
        paths.push(format!("/reputation/{uid}/add"));
    }
    let results = futures::future::join_all(
        paths
            .iter()
            .map(|path| c.post_form(path, &[("reputation", "1")])),
    )
    .await;
    assert_eq!(
        results.iter().filter(|r| r.status.is_redirection()).count(),
        1
    );
}
