//! API tokens are separate from browser sessions; security throttles are shared by all nodes.

mod common;

async fn token(c: &common::Client, scopes: &[&str]) -> String {
    let r = c
        .send_json(
            "POST",
            "/api/v1/auth/token",
            serde_json::json!({"username": "admin", "password": common::ADMIN_PASSWORD, "scopes": scopes}),
            &[],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn tokens_are_hashed_scoped_and_only_valid_on_the_api() {
    let t = test_app!();
    let c = t.client();
    let tok = token(&c, &["read"]).await;
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM api_tokens")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(stored, rbb::util::sha256_hex(&tok));
    assert!(!stored.contains(&tok));
    let auth = format!("Bearer {tok}");
    let h = [("authorization", auth.as_str())];
    let me = c
        .send_json("GET", "/api/v1/me", serde_json::json!({}), &h)
        .await;
    assert_eq!(me.status, 200);
    // Read scope only: no writes.
    let w = c
        .send_json(
            "POST",
            "/api/v1/threads/1/posts",
            serde_json::json!({"message": "x"}),
            &h,
        )
        .await;
    assert_eq!(w.status, 403);
    // Not a browser session.
    let ucp = c
        .send_json("GET", "/usercp", serde_json::json!({}), &h)
        .await;
    assert_ne!(ucp.status, 200);
    // A bad token is an error, not an anonymous request.
    let bad = c
        .send_json(
            "GET",
            "/api/v1/me",
            serde_json::json!({}),
            &[("authorization", "Bearer nope")],
        )
        .await;
    assert_eq!(bad.status, 401);
}

#[tokio::test]
async fn logging_out_everywhere_revokes_api_tokens() {
    let t = test_app!();
    let c = t.client();
    let tok = token(&c, &["read", "write"]).await;
    rbb::auth::destroy_all_logins(&t.app, 1, None)
        .await
        .unwrap();
    let auth = format!("Bearer {tok}");
    let r = c
        .send_json(
            "GET",
            "/api/v1/me",
            serde_json::json!({}),
            &[("authorization", auth.as_str())],
        )
        .await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn throttles_are_shared_between_nodes() {
    let t = test_app!();
    let other = rbb::app::AppState::new(common::test_config(&t.db.url), t.db.pool.clone())
        .await
        .unwrap();
    let mut allowed = 0;
    for i in 0..10 {
        let node = if i % 2 == 0 { &t.app } else { &other };
        if node.throttle("login:198.51.100.1", 5, 300).await {
            allowed += 1;
        }
    }
    assert_eq!(allowed, 5, "the limit applies to the cluster, not per node");
    let key: String = sqlx::query_scalar("SELECT key FROM ratelimits LIMIT 1")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(
        key.starts_with("login:") && !key.contains("198.51"),
        "{key}"
    );
}
