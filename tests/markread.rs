mod common;

/// Read the undo cookie the way the Undo button does: its action and fields, as posted.
fn undo_form(c: &common::Client) -> (String, Vec<(String, String)>) {
    let raw = c.cookie("rbb_undo").expect("undo cookie");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let fields = v["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f[0].as_str().unwrap().to_string(),
                f[1].as_str().unwrap().to_string(),
            )
        })
        .collect();
    (v["action"].as_str().unwrap().to_string(), fields)
}

#[tokio::test]
async fn undo_restores_read_markers_and_last_visit() {
    let t = test_app!();
    let uid = t.create_user("reader", "Passw0rd-reader").await;
    sqlx::query("UPDATE users SET lastvisit = 500 WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO forumsread (fid, uid, dateline) VALUES (3, $1, 100)")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;

    let r = c.post_form("/markread", &[]).await;
    assert!(r.status.is_redirection(), "{}", r.status);
    let (action, fields) = undo_form(&c);
    assert_eq!(action, "/markread/undo");

    let marked: Vec<(i32, i64)> =
        sqlx::query_as("SELECT fid, dateline FROM forumsread WHERE uid = $1 ORDER BY fid")
            .bind(uid)
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    let at = marked.iter().find(|(fid, _)| *fid == 3).unwrap().1;
    assert!(at > 500, "{marked:?}");
    let lastvisit: i64 = sqlx::query_scalar("SELECT lastvisit FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(lastvisit, at);

    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let r = c.post_form(&action, &fields).await;
    assert!(r.status.is_redirection(), "{}", r.status);

    let restored: Vec<(i32, i64)> =
        sqlx::query_as("SELECT fid, dateline FROM forumsread WHERE uid = $1 ORDER BY fid")
            .bind(uid)
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(restored, vec![(3, 100)]);
    let lastvisit: i64 = sqlx::query_scalar("SELECT lastvisit FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(lastvisit, 500);
}

#[tokio::test]
async fn undo_leaves_markers_the_member_changed_since() {
    let t = test_app!();
    let uid = t.create_user("sprinter", "Passw0rd-sprinter").await;
    let c = t.login_as(uid).await;
    assert!(c.post_form("/markread", &[]).await.status.is_redirection());
    let (action, fields) = undo_form(&c);

    // A forum marked after the mark-all is not part of the undo.
    sqlx::query("UPDATE forumsread SET dateline = 1 WHERE uid = $1 AND fid = 3")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert!(c.post_form(&action, &fields).await.status.is_redirection());

    let dateline: Option<i64> =
        sqlx::query_scalar("SELECT dateline FROM forumsread WHERE uid = $1 AND fid = 3")
            .bind(uid)
            .fetch_optional(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(dateline, Some(1));
}

#[tokio::test]
async fn undo_requires_login_and_csrf() {
    let t = test_app!();
    let uid = t.create_user("nosy", "Passw0rd-nosy").await;
    let c = t.login_as(uid).await;
    let r = t
        .client()
        .post_form("/markread/undo", &[("at", "1"), ("lastvisit", "1")])
        .await;
    assert!(!r.status.is_success(), "{}", r.status);
    let r = c
        .request(
            axum::http::Request::builder()
                .method("POST")
                .uri("/markread/undo")
                .header("x-forwarded-for", "203.0.113.10")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .header(
                    axum::http::header::COOKIE,
                    format!(
                        "{}={}",
                        rbb::ctx::AUTH_COOKIE,
                        c.cookie(rbb::ctx::AUTH_COOKIE).unwrap()
                    ),
                )
                .body(axum::body::Body::from("at=1&lastvisit=1"))
                .unwrap(),
        )
        .await;
    assert!(r.status.is_client_error(), "{}", r.status);
}
