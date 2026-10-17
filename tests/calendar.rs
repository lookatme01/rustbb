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

#[tokio::test]
async fn events_with_absurd_dates_are_refused_and_cannot_stall_the_views() {
    let t = test_app!();
    let cid: i32 = sqlx::query_scalar("SELECT MIN(cid) FROM calendars")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(1).await;
    let r = c
        .post_form(
            &format!("/calendar/{cid}/addevent"),
            &[
                ("name", "Forever"),
                ("message", "Spans eight thousand years"),
                ("date", "0001-01-01"),
                ("enddate", "9999-12-31"),
                ("repeat", "daily"),
            ],
        )
        .await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    // Rows that predate the check (or arrive from an import) must not stall the views either.
    sqlx::query("INSERT INTO events (cid, uid, name, description, dateline, starttime, endtime, repeats) VALUES ($1, 1, 'Forever', 'x', 1, -62135596800, 253402300799, '{\"type\":\"daily\",\"interval\":1}')")
        .bind(cid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    for path in [
        format!("/calendar/{cid}"),
        format!("/calendar/{cid}/week/2026-10-15"),
        format!("/calendar/{cid}/day/2026-10-15"),
    ] {
        let r = c.get(&path).await;
        assert_eq!(r.status, 200, "{path}: {}", r.status);
        assert!(r.body.contains("Forever"), "{path}: the event is shown");
    }
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
}

#[tokio::test]
async fn making_a_private_event_public_sends_it_to_moderation() {
    let t = test_app!();
    sqlx::query("UPDATE calendars SET moderation = TRUE")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["calendars"]).await.unwrap();
    let cid: i32 = sqlx::query_scalar("SELECT MIN(cid) FROM calendars")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let uid = t.create_user("planner", "Passw0rd-planner").await;
    let c = t.login_as(uid).await;
    let r = c
        .post_form(
            &format!("/calendar/{cid}/addevent"),
            &[
                ("name", "Secret plan"),
                ("message", "Only for me"),
                ("date", "2026-11-01"),
                ("private", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let (eid, visible): (i32, bool) = sqlx::query_as("SELECT eid, visible FROM events")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(visible, "private events are not moderated");
    let r = c
        .post_form(
            &format!("/calendar/event/{eid}/edit"),
            &[
                ("name", "Public plan"),
                ("message", "For everyone"),
                ("date", "2026-11-01"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let (private, visible): (bool, bool) =
        sqlx::query_as("SELECT private, visible FROM events WHERE eid = $1")
            .bind(eid)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert!(!private);
    assert!(!visible, "a newly public event must wait for approval");
}

#[tokio::test]
async fn editing_a_public_event_sends_it_back_to_moderation() {
    let t = test_app!();
    sqlx::query("UPDATE calendars SET moderation = TRUE")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["calendars"]).await.unwrap();
    let cid: i32 = sqlx::query_scalar("SELECT MIN(cid) FROM calendars")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let uid = t.create_user("editor", "Passw0rd-editor").await;
    let eid: i32 = sqlx::query_scalar("INSERT INTO events (cid, uid, name, description, visible, private, dateline, starttime) VALUES ($1, $2, 'Approved', 'ok', TRUE, FALSE, 1, 1793500000) RETURNING eid")
        .bind(cid)
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    let r = c
        .post_form(
            &format!("/calendar/event/{eid}/edit"),
            &[
                ("name", "Approved"),
                ("message", "now something else entirely"),
                ("date", "2026-11-01"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);
    let visible: bool = sqlx::query_scalar("SELECT visible FROM events WHERE eid = $1")
        .bind(eid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert!(!visible, "an edited public event must wait for approval");
}
