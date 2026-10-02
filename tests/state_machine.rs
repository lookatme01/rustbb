//! Property test: any sequence of moderation operations keeps every denormalized counter
//! (thread replies, forum totals, member post counts, first/last post) equal to the data.

mod common;

use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    PostVisibility(usize, i16),
    ThreadVisibility(usize, i16),
    DeletePost(usize),
    DeleteThread(usize),
    Move(usize, bool),
    MergeThreads(usize, usize),
    Split(usize),
    MergePosts(usize),
    Copy(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let vis = prop_oneof![Just(1i16), Just(0i16), Just(-1i16)];
    prop_oneof![
        (any::<usize>(), vis.clone()).prop_map(|(i, v)| Op::PostVisibility(i, v)),
        (any::<usize>(), vis).prop_map(|(i, v)| Op::ThreadVisibility(i, v)),
        any::<usize>().prop_map(Op::DeletePost),
        any::<usize>().prop_map(Op::DeleteThread),
        (any::<usize>(), any::<bool>()).prop_map(|(i, r)| Op::Move(i, r)),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Op::MergeThreads(a, b)),
        any::<usize>().prop_map(Op::Split),
        any::<usize>().prop_map(Op::MergePosts),
        any::<usize>().prop_map(Op::Copy),
    ]
}

async fn ids(t: &common::TestApp, sql: &str) -> Vec<i32> {
    sqlx::query_scalar(sql).fetch_all(&t.db.pool).await.unwrap()
}

fn pick(v: &[i32], i: usize) -> Option<i32> {
    (!v.is_empty()).then(|| v[i % v.len()])
}

async fn run(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let Some(t) = common::TestApp::new().await else {
        return Ok(());
    };
    for k in ["postfloodcheck", "postmergemins"] {
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, '0') ON CONFLICT (name) DO UPDATE SET value = '0'")
            .bind(k)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    t.app.invalidate(&["settings"]).await.unwrap();
    let uids = [
        t.create_user("alice", "Passw0rd-alice").await,
        t.create_user("bob", "Passw0rd-bob").await,
    ];
    for (i, uid) in uids.iter().enumerate() {
        let c = t.login_as(*uid).await;
        for n in 0..2 {
            let r = c
                .post_form(
                    "/newthread/3",
                    &[
                        ("subject", &format!("Thread {i}{n}")),
                        ("message", "Opening post text."),
                    ],
                )
                .await;
            assert!(r.status.is_redirection());
            let tid: i32 = sqlx::query_scalar("SELECT MAX(tid) FROM threads")
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
            for m in 0..2 {
                c.post_form(
                    &format!("/newreply/{tid}"),
                    &[("message", &format!("Reply {m} text."))],
                )
                .await;
            }
        }
    }
    for op in ops {
        let threads = ids(
            &t,
            "SELECT tid FROM threads WHERE closed NOT LIKE 'moved|%' ORDER BY tid",
        )
        .await;
        let posts = ids(&t, "SELECT pid FROM posts ORDER BY pid").await;
        let app = &t.app;
        let r = match op.clone() {
            Op::PostVisibility(i, v) => match pick(&posts, i) {
                Some(p) => rbb::ops::set_posts_visibility(app, &[p], v).await,
                None => Ok(()),
            },
            Op::ThreadVisibility(i, v) => match pick(&threads, i) {
                Some(x) => rbb::ops::set_threads_visibility(app, &[x], v).await,
                None => Ok(()),
            },
            Op::DeletePost(i) => match pick(&posts, i) {
                Some(p) => rbb::ops::delete_posts(app, &[p]).await,
                None => Ok(()),
            },
            Op::DeleteThread(i) => match pick(&threads, i) {
                Some(x) => rbb::ops::delete_threads(app, &[x]).await,
                None => Ok(()),
            },
            Op::Move(i, redirect) => match pick(&threads, i) {
                Some(x) => {
                    let fid: i32 = sqlx::query_scalar("SELECT fid FROM threads WHERE tid = $1")
                        .bind(x)
                        .fetch_one(&t.db.pool)
                        .await
                        .unwrap();
                    let to = if fid == 3 { 4 } else { 3 };
                    rbb::ops::move_threads(app, &[x], to, redirect.then_some(0)).await
                }
                None => Ok(()),
            },
            Op::MergeThreads(a, b) => match (pick(&threads, a), pick(&threads, b)) {
                (Some(a), Some(b)) => rbb::ops::merge_threads(app, a, b, None).await,
                _ => Ok(()),
            },
            Op::Split(i) => match pick(&threads, i) {
                Some(x) => {
                    let replies = ids(&t, &format!("SELECT p.pid FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.tid = {x} AND p.pid <> t.firstpost ORDER BY p.pid")).await;
                    if replies.is_empty() {
                        Ok(())
                    } else {
                        rbb::ops::split_posts(app, &replies[..1], "Split", 3)
                            .await
                            .map(|_| ())
                    }
                }
                None => Ok(()),
            },
            Op::MergePosts(i) => match pick(&threads, i) {
                Some(x) => {
                    let ps = ids(
                        &t,
                        &format!("SELECT pid FROM posts WHERE tid = {x} ORDER BY dateline, pid"),
                    )
                    .await;
                    if ps.len() < 2 {
                        Ok(())
                    } else {
                        rbb::ops::merge_posts(app, &ps[ps.len() - 2..], "\n").await
                    }
                }
                None => Ok(()),
            },
            Op::Copy(i) => match pick(&threads, i) {
                Some(x) => rbb::ops::copy_thread(app, x, 4).await.map(|_| ()),
                None => Ok(()),
            },
        };
        // An operation may fail (merging posts of different threads, …); either way nothing
        // may be left half-done.
        let problems = rbb::ops::check_counters(&t.db.pool).await.unwrap();
        prop_assert!(problems.is_empty(), "after {op:?} ({r:?}): {problems:?}");
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 8, max_shrink_iters: 20, failure_persistence: None, .. ProptestConfig::default() })]

    #[test]
    fn moderation_keeps_counters_consistent(ops in prop::collection::vec(op(), 1..15)) {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(run(ops))?;
    }
}
