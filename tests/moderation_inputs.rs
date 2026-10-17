//! Moderator forms validate what they store: inline merges skip redirects, and edited subjects
//! and prefixes obey the same rules as posting.

mod common;

use common::TestApp;

async fn forum(t: &TestApp) -> i32 {
    sqlx::query_scalar("SELECT fid FROM forums WHERE name = 'General Discussion'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

async fn thread(t: &TestApp, fid: i32, closed: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible, closed) VALUES ($1, 'Topic', 1, 'admin', 1, 1, 1, $2) RETURNING tid",
    )
    .bind(fid)
    .bind(closed)
    .fetch_one(&t.db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn inline_merge_refuses_redirects() {
    let t = test_app!();
    let fid = forum(&t).await;
    let real = thread(&t, fid, "").await;
    let stub = thread(&t, fid, &format!("moved|{real}")).await;
    let c = t.login_as(1).await;
    let r = c
        .post_form(
            "/moderation/threads",
            &[
                ("action", "merge"),
                ("fid", &fid.to_string()),
                ("tids", &format!("{stub},{real}")),
            ],
        )
        .await;
    assert!(
        r.body.contains("A redirect cannot be merged"),
        "{}",
        r.status
    );
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads WHERE tid = ANY($1)")
        .bind(vec![real, stub])
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(left, 2);
}

#[tokio::test]
async fn edit_thread_checks_subject_length_and_prefix() {
    let t = test_app!();
    let fid = forum(&t).await;
    let tid = thread(&t, fid, "").await;
    let c = t.login_as(1).await;
    let long = "x".repeat(500);
    let r = c
        .post_form(
            "/moderation/editthread",
            &[("tid", &tid.to_string()), ("subject", &long)],
        )
        .await;
    assert!(r.body.contains("subject is too long"), "{}", r.status);
    let r = c
        .post_form(
            "/moderation/editthread",
            &[
                ("tid", &tid.to_string()),
                ("subject", "Fine"),
                ("prefix", "99999"),
            ],
        )
        .await;
    assert!(r.body.contains("prefix is not available"), "{}", r.status);
    let subject: String = sqlx::query_scalar("SELECT subject FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(subject, "Topic");
    let r = c
        .post_form(
            "/moderation/editthread",
            &[("tid", &tid.to_string()), ("subject", "Renamed")],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
}
