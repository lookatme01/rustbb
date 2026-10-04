mod common;

async fn setup() -> Option<(common::TestApp, common::Client, i32)> {
    let t = common::TestApp::new().await?;
    let c = t.login_as(1).await;
    let tid: i32 = sqlx::query_scalar("SELECT MIN(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    Some((t, c, tid))
}

async fn poll(t: &common::TestApp, c: &common::Client, tid: i32) -> i32 {
    let r = c
        .post_form(
            &format!("/thread/{tid}/poll/new"),
            &[
                ("question", "Which one?"),
                ("options", "Alpha\nBeta\nGamma"),
                ("multiple", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    sqlx::query_scalar("SELECT poll FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn merging_threads_keeps_only_the_selected_poll() {
    let Some((t, c, tid)) = setup().await else {
        return;
    };
    let keep = poll(&t, &c, tid).await;
    let r = c
        .post_form(
            "/newthread/3",
            &[("subject", "Source"), ("message", "Source opening post")],
        )
        .await;
    assert!(r.status.is_redirection());
    let from: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    poll(&t, &c, from).await;
    rbb::ops::merge_threads(&t.app, tid, from, None)
        .await
        .unwrap();
    let remaining: Vec<i32> = sqlx::query_scalar("SELECT pid FROM polls WHERE tid = $1")
        .bind(tid)
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(remaining, vec![keep]);

    // An empty destination instead adopts the source poll.
    let r = c
        .post_form(
            "/newthread/3",
            &[
                ("subject", "Destination"),
                ("message", "Destination opening post"),
            ],
        )
        .await;
    assert!(r.status.is_redirection());
    let into: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    rbb::ops::merge_threads(&t.app, into, tid, None)
        .await
        .unwrap();
    let adopted: i32 = sqlx::query_scalar("SELECT poll FROM threads WHERE tid = $1")
        .bind(into)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(adopted, keep);
    let poll_tid: i32 = sqlx::query_scalar("SELECT tid FROM polls WHERE pid = $1")
        .bind(keep)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(poll_tid, into);
}

#[tokio::test]
async fn concurrent_creation_adds_exactly_one_poll() {
    let Some((t, c, tid)) = setup().await else {
        return;
    };
    let path = format!("/thread/{tid}/poll/new");
    let responses = futures::future::join_all((0..8).map(|_| {
        c.post_form(
            &path,
            &[("question", "Pick one"), ("options", "Alpha\nBeta")],
        )
    }))
    .await;
    assert_eq!(
        responses
            .iter()
            .filter(|r| r.status.is_redirection())
            .count(),
        1
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM polls WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn editing_options_removes_stale_ballots_and_recounts_voters() {
    let Some((t, c, tid)) = setup().await else {
        return;
    };
    let pid = poll(&t, &c, tid).await;
    let uid = t.create_user("voter", "Passw0rd-voter").await;
    let voter = t.login_as(uid).await;
    assert!(
        c.post_form(&format!("/thread/{tid}/poll/vote"), &[("option", "1")])
            .await
            .status
            .is_redirection()
    );
    assert!(
        voter
            .post_form(
                &format!("/thread/{tid}/poll/vote"),
                &[("option", "2"), ("option", "3")]
            )
            .await
            .status
            .is_redirection()
    );
    let r = c
        .post_form(
            &format!("/thread/{tid}/poll/edit"),
            &[
                ("question", "Updated"),
                ("options", "Alpha\nDelta"),
                ("multiple", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection());
    let (votes, numvotes): (Vec<i32>, i32) =
        sqlx::query_as("SELECT votes, numvotes FROM polls WHERE pid = $1")
            .bind(pid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(votes, vec![1, 0]);
    assert_eq!(numvotes, 1);
    let ballots: Vec<(i32, i32)> =
        sqlx::query_as("SELECT uid, voteoption FROM pollvotes WHERE pid = $1")
            .bind(pid)
            .fetch_all(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(ballots, vec![(1, 1)]);
    assert!(
        voter
            .post_form(&format!("/thread/{tid}/poll/vote"), &[("option", "2")])
            .await
            .status
            .is_redirection()
    );
}

#[tokio::test]
async fn closed_expired_and_closed_thread_polls_reject_vote_withdrawal() {
    let Some((t, c, tid)) = setup().await else {
        return;
    };
    let pid = poll(&t, &c, tid).await;
    assert!(
        c.post_form(&format!("/thread/{tid}/poll/vote"), &[("option", "1")])
            .await
            .status
            .is_redirection()
    );
    for (closed, timeout, thread_closed) in [(true, 0i64, ""), (false, 1, ""), (false, 0, "1")] {
        sqlx::query("UPDATE polls SET closed = $2, timeout = $3 WHERE pid = $1")
            .bind(pid)
            .bind(closed)
            .bind(timeout)
            .execute(&t.db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE threads SET closed = $2 WHERE tid = $1")
            .bind(tid)
            .bind(thread_closed)
            .execute(&t.db.pool)
            .await
            .unwrap();
        let r = c.post_form(&format!("/thread/{tid}/poll/undo"), &[]).await;
        assert!(r.status.is_client_error(), "{}", r.status);
        let n: i32 = sqlx::query_scalar("SELECT numvotes FROM polls WHERE pid = $1")
            .bind(pid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}

#[tokio::test]
async fn oversized_poll_duration_is_rejected_without_creating_a_poll() {
    let Some((t, c, tid)) = setup().await else {
        return;
    };
    let r = c
        .post_form(
            &format!("/thread/{tid}/poll/new"),
            &[
                ("question", "Pick one"),
                ("options", "Alpha\nBeta"),
                ("timeout", "9223372036854775807"),
            ],
        )
        .await;
    assert!(r.status.is_client_error(), "{}", r.status);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM polls WHERE tid = $1")
        .bind(tid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
