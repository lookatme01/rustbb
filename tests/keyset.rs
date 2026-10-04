//! Keyset pagination returns every row exactly once, in order.

mod common;

#[tokio::test]
async fn api_cursors_walk_a_thread_and_a_forum() {
    let t = test_app!();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('postfloodcheck', '0'), ('postmergemins', '0') ON CONFLICT (name) DO UPDATE SET value = '0'")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let uid = t.create_user("walker", "Passw0rd-walker").await;
    let c = t.login_as(uid).await;
    for i in 0..7 {
        c.post_form(
            "/newthread/3",
            &[
                ("subject", &format!("Keyset {i}")),
                ("message", "Opening post text."),
            ],
        )
        .await;
    }
    let tid: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    for i in 0..9 {
        c.post_form(
            &format!("/newreply/{tid}"),
            &[("message", &format!("reply {i}"))],
        )
        .await;
    }
    // Posts of the thread, 4 at a time.
    let mut seen = vec![];
    let mut after = String::new();
    loop {
        let r = c
            .get(&format!("/api/v1/threads/{tid}?per_page=4&after={after}"))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        seen.extend(
            v["posts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["pid"].as_i64().unwrap()),
        );
        match v["next"].as_str() {
            Some(n) => after = n.to_string(),
            None => break,
        }
    }
    let all: Vec<i64> =
        sqlx::query_scalar("SELECT pid::bigint FROM posts WHERE tid = $1 ORDER BY dateline, pid")
            .bind(tid)
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(seen, all);
    // Threads of the forum, 3 at a time.
    let mut seen = vec![];
    let mut after = String::new();
    loop {
        let r = c
            .get(&format!(
                "/api/v1/forums/3/threads?per_page=3&after={after}"
            ))
            .await;
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        seen.extend(
            v["threads"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["tid"].as_i64().unwrap()),
        );
        match v["next"].as_str() {
            Some(n) => after = n.to_string(),
            None => break,
        }
    }
    let all: Vec<i64> = sqlx::query_scalar("SELECT tid::bigint FROM threads WHERE fid = 3 AND visible = 1 ORDER BY sticky DESC, lastpost DESC, tid DESC")
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(seen, all);
    assert_eq!(
        c.get("/api/v1/forums/3/threads?after=garbage").await.status,
        422
    );
}
