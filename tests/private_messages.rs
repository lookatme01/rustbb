mod common;

#[tokio::test]
async fn tracking_distinguishes_deliveries_in_the_same_second_and_cancels_all_copies() {
    let t = test_app!();
    let to = t.create_user("recipient", "Passw0rd-recipient").await;
    let bcc = t.create_user("hiddenrecipient", "Passw0rd-recipient").await;
    let sender = t.login_as(1).await;
    for subject in ["First", "Second"] {
        let r = sender
            .post_form(
                "/pm/send",
                &[
                    ("to", "recipient"),
                    ("bcc", "hiddenrecipient"),
                    ("subject", subject),
                    ("message", "Message body"),
                    ("savecopy", "1"),
                    ("receipt", "1"),
                ],
            )
            .await;
        assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    }
    sqlx::query("UPDATE privatemessages SET dateline = 100")
        .execute(&t.db.pool)
        .await
        .unwrap();
    let first: i32 =
        sqlx::query_scalar("SELECT pmid FROM privatemessages WHERE uid = $1 AND subject = 'First'")
            .bind(to)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(
        t.login_as(to)
            .await
            .get(&format!("/pm/read/{first}"))
            .await
            .status,
        200
    );
    let receipts: Vec<i16> =
        sqlx::query_scalar("SELECT receipt FROM privatemessages WHERE folder = 2 ORDER BY pmid")
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(receipts, vec![2, 1]);
    let second: i32 =
        sqlx::query_scalar("SELECT pmid FROM privatemessages WHERE uid = 1 AND subject = 'Second'")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    let r = sender
        .post_form(
            "/pm/tracking",
            &[("action", "cancel"), ("pmids", &second.to_string())],
        )
        .await;
    assert!(r.status.is_redirection());
    let remaining: Vec<(i32, String)> =
        sqlx::query_as("SELECT uid, subject FROM privatemessages WHERE uid <> 1 ORDER BY uid")
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(remaining, vec![(to, "First".into()), (bcc, "First".into())]);
    assert_counts(&t, to).await;
    assert_counts(&t, bcc).await;
    assert_counts(&t, 1).await;
}

async fn assert_counts(t: &common::TestApp, uid: i32) {
    let cached: (i32, i32) = sqlx::query_as("SELECT totalpms, unreadpms FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let actual: (i32, i32) = sqlx::query_as("SELECT COUNT(*)::int, COUNT(*) FILTER (WHERE status = 0 AND folder NOT IN (2,3))::int FROM privatemessages WHERE uid = $1")
        .bind(uid).fetch_one(&t.db.pool).await.unwrap();
    assert_eq!(cached, actual);
}

#[tokio::test]
async fn drafting_editing_and_sending_keep_pm_counts_correct() {
    let t = test_app!();
    let uid = t.create_user("recipient", "Passw0rd-recipient").await;
    let c = t.login_as(1).await;
    let r = c
        .post_form(
            "/pm/send",
            &[
                ("subject", "Draft"),
                ("message", "Message body"),
                ("savedraft", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    assert_counts(&t, 1).await;
    let pmid: i32 =
        sqlx::query_scalar("SELECT pmid FROM privatemessages WHERE uid = 1 AND folder = 3")
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    let pmid = pmid.to_string();
    let r = c
        .post_form(
            "/pm/send",
            &[
                ("pmid", &pmid),
                ("subject", "Edited draft"),
                ("message", "Edited body"),
                ("savedraft", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection());
    assert_counts(&t, 1).await;
    let r = c
        .post_form(
            "/pm/send",
            &[
                ("pmid", &pmid),
                ("to", "recipient"),
                ("subject", "Sent draft"),
                ("message", "Edited body"),
                ("savecopy", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    assert_counts(&t, 1).await;
    assert_counts(&t, uid).await;
}

#[tokio::test]
async fn concurrent_reads_only_decrement_unread_once() {
    let t = test_app!();
    let uid = t.create_user("reader", "Passw0rd-reader").await;
    for i in 0..3 {
        rbb::routes::private::send_system_pm(&t.app, uid, &format!("Message {i}"), "Read me")
            .await
            .unwrap();
    }
    let pmid: i32 = sqlx::query_scalar("SELECT MIN(pmid) FROM privatemessages WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    let path = format!("/pm/read/{pmid}");
    let prefetch = axum::http::Request::builder()
        .uri(&path)
        .header("sec-purpose", "prefetch")
        .header(
            "cookie",
            format!(
                "{}={}",
                rbb::ctx::AUTH_COOKIE,
                c.cookie(rbb::ctx::AUTH_COOKIE).unwrap()
            ),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(c.request(prefetch).await.status, 200);
    let unread: i32 = sqlx::query_scalar("SELECT unreadpms FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(unread, 3, "hovering a message must not mark it read");
    for r in futures::future::join_all((0..8).map(|_| c.get(&path))).await {
        assert_eq!(r.status, 200, "{}", r.body);
    }
    assert_counts(&t, uid).await;
    let unread: i32 = sqlx::query_scalar("SELECT unreadpms FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(unread, 2);
}
