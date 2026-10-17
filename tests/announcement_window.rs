//! Announcements honour their start and end dates on the direct link, not just in listings.

mod common;

async fn announcement(t: &common::TestApp, subject: &str, start: i64, end: i64) -> i32 {
    let aid = sqlx::query_scalar("INSERT INTO announcements (fid, uid, subject, message, startdate, enddate) VALUES (-1, 1, $1, 'body', $2, $3) RETURNING aid")
        .bind(subject)
        .bind(start)
        .bind(end)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["announcements"]).await.unwrap();
    aid
}

#[tokio::test]
async fn scheduled_and_expired_announcements_are_staff_only() {
    let t = test_app!();
    let now = rbb::util::now();
    let live = announcement(&t, "Live", now - 100, 0).await;
    let future = announcement(&t, "Future", now + 10_000, 0).await;
    let expired = announcement(&t, "Expired", now - 10_000, now - 100).await;
    let uid = t.create_user("reader", "Passw0rd-reader").await;
    let member = t.login_as(uid).await;
    assert_eq!(
        member.get(&format!("/announcement/{live}")).await.status,
        200
    );
    for aid in [future, expired] {
        let r = member.get(&format!("/announcement/{aid}")).await;
        assert_eq!(r.status, 404, "aid {aid}");
    }
    let admin = t.login_as(1).await;
    for aid in [live, future, expired] {
        assert_eq!(admin.get(&format!("/announcement/{aid}")).await.status, 200);
    }
}
