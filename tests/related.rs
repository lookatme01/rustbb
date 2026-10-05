//! Related threads under the reply box: ranked by shared words, topped up from the same forum,
//! and never showing threads the viewer can't read.

mod common;

use common::TestApp;

async fn thread(t: &TestApp, fid: i32, subject: &str, message: &str, lastpost: i64) -> i32 {
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, lastposter, visible)
         VALUES ($1, $2, 1, 'admin', $3, $3, 'admin', 1) RETURNING tid",
    )
    .bind(fid)
    .bind(subject)
    .bind(lastpost)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible)
         VALUES ($1, $2, $3, 1, 'admin', $4, $5, 1) RETURNING pid",
    )
    .bind(tid)
    .bind(fid)
    .bind(subject)
    .bind(lastpost)
    .bind(message)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE threads SET firstpost = $2 WHERE tid = $1")
        .bind(tid)
        .bind(pid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    tid
}

/// The thread page once related threads are cached (an uncached lookup may finish after the
/// first page is rendered).
async fn page_with_related(c: &common::Client, tid: i32) -> String {
    for _ in 0..50 {
        let r = c.get(&format!("/thread/{tid}")).await;
        assert_eq!(r.status, 200);
        if r.body.contains("Related threads") {
            return r.body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no related threads on thread {tid}");
}

#[tokio::test]
async fn related_threads_rank_by_content_and_respect_permissions() {
    let t = test_app!();
    sqlx::query(
        "INSERT INTO forums (fid, name, description, type, pid, parentlist, disporder) VALUES (8, 'Back Room', '', 'f', 1, '{1,8}', 9)",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO forumpermissions (fid, gid, perms) VALUES (8, 1, '{\"canview\":false}')",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    let a = thread(
        &t,
        3,
        "Sourdough starter keeps dying",
        "My sourdough starter smells sour and the bread will not rise.",
        100,
    )
    .await;
    thread(
        &t,
        6,
        "Flat sourdough bread",
        "Any tips for a lazy sourdough starter? The bread comes out flat.",
        90,
    )
    .await;
    thread(
        &t,
        3,
        "Favourite hiking trails",
        "Where do you walk on weekends?",
        200,
    )
    .await;
    thread(&t, 6, "Knitting patterns", "Show off your scarves.", 300).await;
    thread(
        &t,
        8,
        "Secret sourdough starter recipe",
        "The best sourdough starter, staff only.",
        400,
    )
    .await;
    rbb::ops::rebuild_all_counters(&t.app).await.unwrap();
    t.app.invalidate(&["forums", "forumperms"]).await.unwrap();

    let body = page_with_related(&t.client(), a).await;
    let related = body.split("Related threads").nth(1).unwrap();
    let flat = related
        .find("Flat sourdough bread")
        .expect("content match listed");
    let hiking = related
        .find("Favourite hiking trails")
        .expect("same-forum top-up listed");
    assert!(flat < hiking, "content matches come before the top-up");
    assert!(
        !related.contains("Knitting patterns"),
        "unrelated threads elsewhere stay out"
    );
    assert!(
        !related.contains("Secret sourdough"),
        "hidden forum leaked to a guest"
    );
    // The section sits under the reply box.
    let reply = body.find("id=\"quickreply\"");
    if let Some(reply) = reply {
        assert!(reply < body.find("Related threads").unwrap());
    }

    // The administrator may read the hidden forum, so the same cached list shows it to them.
    let admin = t.login_as(1).await;
    let body = page_with_related(&admin, a).await;
    assert!(
        body.split("Related threads")
            .nth(1)
            .unwrap()
            .contains("Secret sourdough")
    );
}

#[tokio::test]
async fn related_threads_can_be_switched_off() {
    let t = test_app!();
    let a = thread(&t, 3, "Sourdough starter", "sourdough starter", 100).await;
    thread(&t, 3, "More sourdough", "sourdough starter again", 90).await;
    rbb::ops::rebuild_all_counters(&t.app).await.unwrap();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('showsimilarthreads', '0') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let r = t.client().get(&format!("/thread/{a}")).await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains("Related threads"));
}
