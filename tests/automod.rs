//! Automated moderation: durable quarantine and rollback against a real database.

mod common;

use rbb::app::App;
use rbb::automod::{Policy, process, run, undo};
use rbb::ops;
use rbb::util::now;

async fn visibility(app: &App, pid: i32) -> i16 {
    sqlx::query_scalar("SELECT visible FROM posts WHERE pid=$1")
        .bind(pid)
        .fetch_one(&app.db)
        .await
        .unwrap()
}
async fn last_action(app: &App, pid: i32) -> i64 {
    sqlx::query_scalar("SELECT id FROM automod_actions WHERE pid=$1 ORDER BY id DESC LIMIT 1")
        .bind(pid)
        .fetch_one(&app.db)
        .await
        .unwrap()
}
async fn counters(app: &App) {
    let errors = ops::check_counters(&app.db).await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");
}
async fn post(app: &App, tid: i32, uid: i32, message: &str) -> i32 {
    sqlx::query_scalar("INSERT INTO posts (tid,fid,uid,username,dateline,message) VALUES ($1,3,$2,'test',$3,$4) RETURNING pid")
        .bind(tid).bind(uid).bind(now()).bind(message).fetch_one(&app.db).await.unwrap()
}
async fn mode(app: &App, mode: &str) {
    sqlx::query("UPDATE automod_config SET policy=jsonb_set(policy,'{mode}',to_jsonb($1::text))")
        .bind(mode)
        .execute(&app.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn durable_quarantine_and_rollback() {
    let t = test_app!();
    let app = t.app.clone();
    let db = t.db.pool.clone();
    let uid:i32 = sqlx::query_scalar("INSERT INTO users (username,password,email,usergroup,regdate) VALUES ('modtest','!','modtest@invalid',2,$1) RETURNING uid").bind(now()).fetch_one(&db).await.unwrap();
    let tid:i32 = sqlx::query_scalar("INSERT INTO threads (fid,subject,uid,username,dateline) VALUES (3,'Test', $1,'modtest',$2) RETURNING tid").bind(uid).bind(now()-1).fetch_one(&db).await.unwrap();
    let first = post(&app, tid, uid, "A useful opening post").await;
    // Seed first post strictly before replies.
    sqlx::query("UPDATE posts SET dateline=$2 WHERE pid=$1")
        .bind(first)
        .bind(now() - 1)
        .execute(&db)
        .await
        .unwrap();
    let reply = post(&app, tid, uid, "buy spam now").await;
    ops::rebuild_all_counters(&app).await.unwrap();
    sqlx::query("UPDATE automod_config SET policy=jsonb_set(policy,'{phrases}','[\"buy spam\"]')")
        .execute(&db)
        .await
        .unwrap();
    run(&app).await.unwrap();
    assert_eq!(visibility(&app, reply).await, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM automod_actions WHERE pid=$1")
        .bind(reply)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(status, "observed");
    assert!(!undo(&app, last_action(&app, reply).await, 1).await.unwrap());
    mode(&app, "quarantine").await;
    sqlx::query("UPDATE posts SET message='BUY SPAM again' WHERE pid=$1")
        .bind(reply)
        .execute(&db)
        .await
        .unwrap();
    let (a, b) = tokio::join!(run(&app), run(&app));
    a.unwrap();
    b.unwrap();
    assert_eq!(visibility(&app, reply).await, 0);
    counters(&app).await;
    let action = last_action(&app, reply).await;
    let bot: i32 = sqlx::query_scalar("SELECT uid FROM automod_actions WHERE id=$1")
        .bind(action)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(bot, app.cache().system_uid);
    assert!(undo(&app, action, 1).await.unwrap());
    assert!(!undo(&app, action, 1).await.unwrap());
    counters(&app).await;
    sqlx::query("UPDATE posts SET message='buy spam edited after undo' WHERE pid=$1")
        .bind(reply)
        .execute(&db)
        .await
        .unwrap();
    run(&app).await.unwrap();
    assert_eq!(visibility(&app, reply).await, 1);
    // Undo reply actions survives a subsequent System thread quarantine and undo.
    let earlier_reply = post(&app, tid, uid, "buy spam earlier reply").await;
    ops::rebuild_all_counters(&app).await.unwrap();
    run(&app).await.unwrap();
    let earlier_action = last_action(&app, earlier_reply).await;
    // Whole-thread quarantine also restores reply/user/forum counters.
    sqlx::query("UPDATE posts SET message='buy spam opening' WHERE pid=$1")
        .bind(first)
        .execute(&db)
        .await
        .unwrap();
    run(&app).await.unwrap();
    assert_eq!(visibility(&app, first).await, 0);
    counters(&app).await;
    assert!(undo(&app, last_action(&app, first).await, 1).await.unwrap());
    counters(&app).await;
    assert!(undo(&app, earlier_action, 1).await.unwrap());
    counters(&app).await;
    // Human changes make undo conflict, rather than clobbering the later edit.
    let conflict = post(&app, tid, uid, "buy spam conflict").await;
    ops::rebuild_all_counters(&app).await.unwrap();
    run(&app).await.unwrap();
    sqlx::query("UPDATE posts SET message='human edit' WHERE pid=$1")
        .bind(conflict)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        !undo(&app, last_action(&app, conflict).await, 1)
            .await
            .unwrap()
    );
    // A database failure rolls back visibility, audit, queue consumption and counters.
    let retry = post(&app, tid, uid, "buy spam retry").await;
    ops::rebuild_all_counters(&app).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION fail_automod() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test failure'; END $$; CREATE TRIGGER fail_automod BEFORE INSERT ON automod_actions FOR EACH ROW EXECUTE FUNCTION fail_automod();").execute(&db).await.unwrap();
    assert!(process(&app, retry).await.is_err());
    assert_eq!(visibility(&app, retry).await, 1);
    counters(&app).await;
    let queued: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM automod_queue WHERE pid=$1)")
            .bind(retry)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(queued);
    sqlx::raw_sql("DROP TRIGGER fail_automod ON automod_actions; DROP FUNCTION fail_automod();")
        .execute(&db)
        .await
        .unwrap();
    process(&app, retry).await.unwrap();
    assert_eq!(visibility(&app, retry).await, 0);
    counters(&app).await;
    // Staff and System remain exempt even from explicit blocked phrases.
    let staff = post(&app, tid, 1, "buy spam staff").await;
    let system = post(&app, tid, app.cache().system_uid, "buy spam system").await;
    ops::rebuild_all_counters(&app).await.unwrap();
    run(&app).await.unwrap();
    assert_eq!(visibility(&app, staff).await, 1);
    assert_eq!(visibility(&app, system).await, 1);
    counters(&app).await;
    mode(&app, "off").await;
    let off = post(&app, tid, uid, "buy spam off").await;
    ops::rebuild_all_counters(&app).await.unwrap();
    run(&app).await.unwrap();
    assert_eq!(visibility(&app, off).await, 1);
    counters(&app).await;
    let policy: Policy = serde_json::from_value(
        sqlx::query_scalar("SELECT policy FROM automod_config WHERE id=1")
            .fetch_one(&db)
            .await
            .unwrap(),
    )
    .unwrap();
    app.tpl.render(1,"admin/automod.html",minijinja::context! {themes=>Vec::<String>::new(),languages=>Vec::<String>::new(),path=>"/admin/automod", current_url=>"/admin/automod", policy=>policy, revision=>1, phrases=>"<script>example</script>", list=>Vec::<serde_json::Value>::new(),history=>Vec::<serde_json::Value>::new(), queued=>0,pending=>0}).unwrap();
}
