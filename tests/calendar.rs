mod common;

#[tokio::test]
async fn nonexistent_dst_times_are_rejected_instead_of_becoming_epoch_dates() {
    let t = test_app!();
    sqlx::query("UPDATE users SET timezone = 'America/Chicago' WHERE uid = 1")
        .execute(&t.db.pool)
        .await
        .unwrap();
    let cid: i32 = sqlx::query_scalar("SELECT MIN(cid) FROM calendars")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(1).await;
    let path = format!("/calendar/{cid}/addevent");
    let r = c
        .post_form(
            &path,
            &[
                ("name", "Spring forward"),
                ("message", "Event description"),
                ("date", "2026-03-08"),
                ("starttime", "02:30"),
            ],
        )
        .await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let r = c
        .post_form(
            &path,
            &[
                ("name", "Spring forward"),
                ("message", "Event description"),
                ("date", "2026-03-08"),
                ("starttime", "03:30"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let start: i64 = sqlx::query_scalar("SELECT starttime FROM events")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(
        chrono::DateTime::from_timestamp(start, 0)
            .unwrap()
            .to_rfc3339(),
        "2026-03-08T08:30:00+00:00"
    );
}

#[tokio::test]
async fn absurd_years_in_day_and_week_views_are_not_found_not_a_panic() {
    let t = test_app!();
    let cid: i32 = sqlx::query_scalar("SELECT MIN(cid) FROM calendars")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(1).await;
    for view in ["day", "week"] {
        let ok = c.get(&format!("/calendar/{cid}/{view}/2026-10-15")).await;
        assert_eq!(ok.status, 200, "{view}: {}", ok.body);
        let r = c
            .get(&format!("/calendar/{cid}/{view}/+262142-12-31"))
            .await;
        assert_eq!(r.status, 404, "{view}: {}", r.status);
    }
}
