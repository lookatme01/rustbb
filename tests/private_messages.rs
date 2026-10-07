mod common;

#[tokio::test]
async fn tracking_migration_only_links_unambiguous_historical_deliveries() {
    let t = test_app!();
    let uid = t.create_user("legacyrecipient", "Passw0rd-recipient").await;
    for subject in ["Unique", "Ambiguous", "Ambiguous"] {
        sqlx::query("INSERT INTO privatemessages (uid, fromid, toid, folder, subject, message, dateline, status) VALUES (1, 1, $1, 2, $2, 'Body', 100, 1)")
            .bind(uid).bind(subject).execute(&t.db.pool).await.unwrap();
    }
    for subject in ["Unique", "Ambiguous"] {
        sqlx::query("INSERT INTO privatemessages (uid, fromid, toid, folder, subject, message, dateline, status) VALUES ($1, 1, $1, 1, $2, 'Body', 100, 0)")
            .bind(uid).bind(subject).execute(&t.db.pool).await.unwrap();
    }
    // Recreate the pre-migration shape in this disposable test database.
    sqlx::query("ALTER TABLE privatemessages DROP COLUMN sent_pmid")
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0027_pm_tracking.sql"))
        .execute(&t.db.pool)
        .await
        .unwrap();
    let rows: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT subject, sent_pmid FROM privatemessages WHERE uid = $1 ORDER BY subject",
    )
    .bind(uid)
    .fetch_all(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(rows[0], ("Ambiguous".into(), None));
    assert_eq!(rows[1].0, "Unique");
    assert!(rows[1].1.is_some());
}

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

/// Sends "sender" -> "reporter" a message, has the reporter report it, and returns the report id.
async fn reported_pm(t: &common::TestApp, pgp: i16) -> (i32, i32) {
    let sender = t.create_user("pmsender", "Passw0rd-sender").await;
    let reporter = t.create_user("pmreporter", "Passw0rd-reporter").await;
    let pmid: i32 = sqlx::query_scalar("INSERT INTO privatemessages (uid, fromid, toid, folder, subject, message, dateline, status, pgp) VALUES ($1, $2, $1, 1, 'Nasty subject', 'Evidence body [b]text[/b]', 100, 0, $3) RETURNING pmid")
        .bind(reporter).bind(sender).bind(pgp).fetch_one(&t.db.pool).await.unwrap();
    let reason: i32 = sqlx::query_scalar(
        "SELECT rid FROM reportreasons WHERE appliesto = 'all' AND NOT extra LIMIT 1",
    )
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["reportreasons"]).await.unwrap();
    let c = t.login_as(reporter).await;
    let page = c.get(&format!("/report?type=pm&id={pmid}")).await;
    assert_eq!(page.status, 200);
    let r = c
        .post_form(
            "/report",
            &[
                ("type", "pm"),
                ("id", &pmid.to_string()),
                ("reason", &reason.to_string()),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    // The reporter deletes the evidence.
    sqlx::query("DELETE FROM privatemessages WHERE pmid = $1")
        .bind(pmid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let rid = sqlx::query_scalar("SELECT rid FROM reportedcontent WHERE type = 'pm' AND id = $1")
        .bind(pmid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    (rid, sender)
}

#[tokio::test]
async fn reported_messages_survive_deletion_and_stay_staff_only() {
    let t = test_app!();
    let (rid, _) = reported_pm(&t, 0).await;
    let admin = t.login_as(1).await;
    let r = admin.get(&format!("/modcp/reports/{rid}")).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Reported message"), "{}", r.body);
    assert!(r.body.contains("Nasty subject"));
    assert!(r.body.contains("Evidence body"));
    assert!(
        r.body.contains(">text</strong>"),
        "body not rendered as MyCode"
    );
    // A forum-only moderator cannot see the report, or its text in the queue.
    let mod_uid = t.create_user("forummod", "Passw0rd-forummod").await;
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES (4, $1, FALSE, '{}')")
        .bind(mod_uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["moderators"]).await.unwrap();
    let m = t.login_as(mod_uid).await;
    assert!(
        m.get(&format!("/modcp/reports/{rid}"))
            .await
            .status
            .is_client_error()
    );
    let list = m.get("/modcp/reports").await;
    assert!(!list.body.contains("Evidence body") && !list.body.contains("Nasty subject"));
}

#[tokio::test]
async fn encrypted_reported_messages_keep_no_body() {
    let t = test_app!();
    let (rid, _) = reported_pm(&t, 2).await;
    let stored: (String, bool) =
        sqlx::query_as("SELECT message, encrypted FROM report_pm_snapshots WHERE rid = $1")
            .bind(rid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(stored, (String::new(), true));
    let r = t
        .login_as(1)
        .await
        .get(&format!("/modcp/reports/{rid}"))
        .await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("end-to-end encrypted"));
    assert!(!r.body.contains("Evidence body"));
}

#[tokio::test]
async fn reports_without_a_snapshot_say_so() {
    let t = test_app!();
    let rid: i32 = sqlx::query_scalar("INSERT INTO reportedcontent (id, id2, uid, type, reason, dateline, lastreport) VALUES (1, 1, 1, 'pm', '', 1, 1) RETURNING rid")
        .fetch_one(&t.db.pool).await.unwrap();
    let r = t
        .login_as(1)
        .await
        .get(&format!("/modcp/reports/{rid}"))
        .await;
    assert!(r.body.contains("No copy of this message was kept"));
}
