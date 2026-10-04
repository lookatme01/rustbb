mod common;

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
