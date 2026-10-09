//! Upload limits, streaming to storage, and live-stream caps.

mod common;

use axum::body::Body;
use axum::http::{Request, header};
use tower::ServiceExt;

fn multipart(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let b = "rbbtestboundary7f3a";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((name, data)) = file {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

fn tmp_files(t: &common::TestApp) -> usize {
    std::fs::read_dir(format!("{}/tmp", t.app.cfg.upload_dir))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// Hold INSERTs until both requests are blocked in PostgreSQL. Without quota
/// serialization both have already observed the old count when the gate opens.
async fn race_uploads(
    t: &common::TestApp,
    c: &common::Client,
    sessions: &[(&str, i32)],
    data: &[u8],
) -> Vec<common::Response> {
    let mut gate = t.db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE attachments IN SHARE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let csrf = c.csrf();
    let requests = sessions.iter().map(|(hash, pid)| {
        let pid = pid.to_string();
        let (ct, body) = multipart(
            &[("my_post_key", &csrf), ("posthash", hash), ("pid", &pid)],
            Some(("race.txt", data)),
        );
        c.request(
            Request::post("/attachment/upload")
                .header(header::CONTENT_TYPE, ct)
                .header(
                    header::COOKIE,
                    format!("rbb_auth={}", c.cookie("rbb_auth").unwrap()),
                )
                .body(Body::from(body))
                .unwrap(),
        )
    });
    let release = async {
        let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND state = 'active'")
                    .fetch_one(&t.db.pool).await.unwrap();
                if waiting >= 2 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await;
        gate.rollback().await.unwrap();
        assert!(
            ready.is_ok(),
            "both uploads did not reach the database gate"
        );
    };
    let (responses, ()) = tokio::join!(futures::future::join_all(requests), release);
    responses
}

#[tokio::test]
async fn failed_storage_cleans_up_temporary_files_and_rolls_back_quota() {
    let t = test_app!();
    sqlx::query(
        "UPDATE usergroups SET perms = perms || '{\"attachquota\":1}'::jsonb WHERE gid = 2",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let uid = t.create_user("storagefailure", "Passw0rd-storage").await;
    let c = t.login_as(uid).await;
    let path = format!("{}/attachments", t.app.cfg.upload_dir);
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, b"block the storage directory").unwrap();
    let csrf = c.csrf();
    let (ct, body) = multipart(
        &[("my_post_key", &csrf), ("posthash", "0123456789abcdef")],
        Some(("failure.txt", &vec![b'x'; 700])),
    );
    let request = || {
        Request::post("/attachment/upload")
            .header(header::CONTENT_TYPE, ct.clone())
            .header(
                header::COOKIE,
                format!("rbb_auth={}", c.cookie("rbb_auth").unwrap()),
            )
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let r = c.request(request()).await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert_eq!(tmp_files(&t), 0, "failed storage leaked the upload");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let r = c.request(request()).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(tmp_files(&t), 0);
}

#[tokio::test]
async fn busy_uploads_time_out_and_recover_after_the_lock_is_released() {
    let t = test_app!();
    let uid = t.create_user("busyupload", "Passw0rd-busy").await;
    let c = t.login_as(uid).await;
    let mut gate = t.db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(0x52424241_i32)
        .bind(uid)
        .execute(&mut *gate)
        .await
        .unwrap();
    let csrf = c.csrf();
    let (ct, body) = multipart(
        &[("my_post_key", &csrf), ("posthash", "0123456789abcdef")],
        Some(("busy.txt", b"attachment")),
    );
    let request = || {
        Request::post("/attachment/upload")
            .header(header::CONTENT_TYPE, ct.clone())
            .header(
                header::COOKIE,
                format!("rbb_auth={}", c.cookie("rbb_auth").unwrap()),
            )
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(5), c.request(request())).await;
    gate.rollback().await.unwrap();
    let r = response.expect("busy upload did not obey its lock timeout");
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(r.body.contains("upload is in progress"));
    assert_eq!(tmp_files(&t), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let r = c.request(request()).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn uploads_larger_than_total_quota_are_rejected_during_streaming() {
    let t = test_app!();
    sqlx::query(
        "UPDATE usergroups SET perms = perms || '{\"attachquota\":1}'::jsonb WHERE gid = 2",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let uid = t.create_user("smallquota", "Passw0rd-quota").await;
    let c = t.login_as(uid).await;
    let csrf = c.csrf();
    let (ct, body) = multipart(
        &[("my_post_key", &csrf), ("posthash", "0123456789abcdef")],
        Some(("large.txt", &vec![b'x'; 2048])),
    );
    let mut gate = t.db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE attachments IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        c.request(
            Request::post("/attachment/upload")
                .header(header::CONTENT_TYPE, ct)
                .header(
                    header::COOKIE,
                    format!("rbb_auth={}", c.cookie("rbb_auth").unwrap()),
                )
                .body(Body::from(body))
                .unwrap(),
        ),
    )
    .await;
    gate.rollback().await.unwrap();
    let r = response.expect("oversized upload reached the database quota check");
    assert_eq!(r.status, 422);
    assert!(r.body.contains("too large"), "{}", r.body);
    assert_eq!(tmp_files(&t), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE uid = $1")
        .bind(uid)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn concurrent_uploads_cannot_exceed_account_quota() {
    let t = test_app!();
    sqlx::query(
        "UPDATE usergroups SET perms = perms || '{\"attachquota\":1}'::jsonb WHERE gid = 2",
    )
    .execute(&t.db.pool)
    .await
    .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let uid = t.create_user("quotaowner", "Passw0rd-quota").await;
    let c = t.login_as(uid).await;
    let responses = race_uploads(
        &t,
        &c,
        &[("0123456789abcdef", 0), ("fedcba9876543210", 0)],
        &vec![b'x'; 700],
    )
    .await;
    assert_eq!(
        responses.iter().filter(|r| r.status == 200).count(),
        1,
        "{:?}",
        responses
            .iter()
            .map(|r| (&r.status, &r.body))
            .collect::<Vec<_>>()
    );
    assert_eq!(responses.iter().filter(|r| r.status == 422).count(), 1);
    let (count, used): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(filesize), 0)::bigint FROM attachments WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!((count, used), (1, 700));
    assert_eq!(tmp_files(&t), 0);
}

#[tokio::test]
async fn concurrent_uploads_cannot_exceed_post_attachment_limit() {
    let t = test_app!();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('maxattachments', '1') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let uid = t.create_user("countowner", "Passw0rd-count").await;
    let c = t.login_as(uid).await;
    for pid in [0, {
        let tid: i32 = sqlx::query_scalar("INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES (3, 'Attachment race', $1, $2, $2) RETURNING tid")
            .bind(uid).bind(rbb::util::now()).fetch_one(&t.db.pool).await.unwrap();
        sqlx::query_scalar("INSERT INTO posts (tid, fid, uid, message, dateline) VALUES ($1, 3, $2, 'Post', $3) RETURNING pid")
            .bind(tid).bind(uid).bind(rbb::util::now()).fetch_one(&t.db.pool).await.unwrap()
    }] {
        let hash = if pid == 0 { "0123456789abcdef" } else { "" };
        let responses = race_uploads(&t, &c, &[(hash, pid), (hash, pid)], b"attachment").await;
        assert_eq!(responses.iter().filter(|r| r.status == 200).count(), 1);
        assert_eq!(responses.iter().filter(|r| r.status == 422).count(), 1);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE uid = $1 AND pid = $2")
                .bind(uid)
                .bind(pid)
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
        assert_eq!(tmp_files(&t), 0);
    }
}

#[tokio::test]
async fn existing_post_uploads_enforce_edit_and_forum_access_rules() {
    let t = test_app!();
    let uid = t.create_user("attachmentowner", "Passw0rd-owner").await;
    let tid: i32 = sqlx::query_scalar("INSERT INTO threads (fid, subject, uid, dateline, lastpost) VALUES (3, 'Upload permissions', $1, 1, 1) RETURNING tid")
        .bind(uid).fetch_one(&t.db.pool).await.unwrap();
    let pid: i32 = sqlx::query_scalar("INSERT INTO posts (tid, fid, uid, message, dateline) VALUES ($1, 3, $2, 'Original message', 1) RETURNING pid")
        .bind(tid).bind(uid).fetch_one(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('enableattachments', '1'), ('edittimelimit', '1') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let owner = t.login_as(uid).await;
    for state in [
        "expired",
        "closed",
        "deleted",
        "locked",
        "edit_denied",
        "allowed",
    ] {
        sqlx::query("UPDATE threads SET closed = $2 WHERE tid = $1")
            .bind(tid)
            .bind(if state == "closed" { "1" } else { "" })
            .execute(&t.db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE posts SET dateline = $2, visible = $3 WHERE pid = $1")
            .bind(pid)
            .bind(if state == "expired" {
                1
            } else {
                rbb::util::now()
            })
            .bind(if state == "deleted" { -1i16 } else { 1 })
            .execute(&t.db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE forums SET password = $1 WHERE fid = 3")
            .bind(if state == "locked" { "locked" } else { "" })
            .execute(&t.db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES (3, 2, $1) ON CONFLICT (fid, gid) DO UPDATE SET perms = EXCLUDED.perms")
            .bind(serde_json::json!({"caneditposts": state != "edit_denied"})).execute(&t.db.pool).await.unwrap();
        t.app.invalidate(&["forums", "forumperms"]).await.unwrap();
        let before = tmp_files(&t);
        let csrf = owner.csrf();
        let pid_text = pid.to_string();
        let (ct, body) = multipart(
            &[("my_post_key", &csrf), ("pid", &pid_text)],
            Some(("notes.txt", b"New attachment")),
        );
        let r = owner
            .request(
                Request::post("/attachment/upload")
                    .header(header::CONTENT_TYPE, ct)
                    .header(
                        header::COOKIE,
                        format!("rbb_auth={}", owner.cookie("rbb_auth").unwrap()),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
        if state == "allowed" {
            assert_eq!(r.status, 200, "{}", r.body);
        } else {
            assert!(
                r.status.is_client_error(),
                "{state} permitted an upload: {} {}",
                r.status,
                r.body
            );
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE pid = $1")
                .bind(pid)
                .fetch_one(&t.db.pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{state}");
        }
        assert_eq!(tmp_files(&t), before, "{state} left a partial upload");
    }
    // Staff retain their explicit edit override on closed, old posts.
    sqlx::query("UPDATE threads SET closed = '1' WHERE tid = $1")
        .bind(tid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE posts SET dateline = 1 WHERE pid = $1")
        .bind(pid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    let admin = t.login_as(1).await;
    let csrf = admin.csrf();
    let pid_text = pid.to_string();
    let (ct, body) = multipart(
        &[("my_post_key", &csrf), ("pid", &pid_text)],
        Some(("staff.txt", b"Staff attachment")),
    );
    let r = admin
        .request(
            Request::post("/attachment/upload")
                .header(header::CONTENT_TYPE, ct)
                .header(
                    header::COOKIE,
                    format!("rbb_auth={}", admin.cookie("rbb_auth").unwrap()),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);

    // A new-post upload must also respect the target forum's password.
    sqlx::query("UPDATE forums SET password = 'locked' WHERE fid = 3")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["forums"]).await.unwrap();
    let csrf = owner.csrf();
    let (ct, body) = multipart(
        &[
            ("my_post_key", &csrf),
            ("fid", "3"),
            ("posthash", "0123456789abcdef"),
        ],
        Some(("new.txt", b"New-post attachment")),
    );
    let before = tmp_files(&t);
    let r = owner
        .request(
            Request::post("/attachment/upload")
                .header(header::CONTENT_TYPE, ct)
                .header(
                    header::COOKIE,
                    format!("rbb_auth={}", owner.cookie("rbb_auth").unwrap()),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE pid = 0")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
    assert_eq!(orphans, 0);
    assert_eq!(tmp_files(&t), before);
}

#[tokio::test]
async fn ordinary_routes_refuse_large_bodies() {
    let t = test_app!();
    let uid = t.create_user("bigposter", "Passw0rd-bigposter").await;
    let c = t.login_as(uid).await;
    let huge = "x".repeat(3 * 1024 * 1024);
    let r = c.post_form("/newreply/1", &[("message", &huge)]).await;
    assert_eq!(r.status, 413, "{}", r.status);
}

#[tokio::test]
async fn oversized_avatar_is_cut_off_while_streaming() {
    let t = test_app!();
    let uid = t.create_user("avatarer", "Passw0rd-avatarer").await;
    let c = t.login_as(uid).await;
    let before = tmp_files(&t);
    let data = vec![0u8; 2 * 1024 * 1024]; // far over the default avatar limit
    let csrf = c.csrf();
    let (ct, body) = multipart(
        &[("my_post_key", &csrf), ("action", "upload")],
        Some(("a.png", &data)),
    );
    let r = c
        .request(
            Request::post("/usercp/avatar")
                .header(header::CONTENT_TYPE, ct)
                .header(
                    header::COOKIE,
                    format!("rbb_auth={}", c.cookie("rbb_auth").unwrap()),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert!(
        r.body.contains("too large"),
        "{} {}",
        r.status,
        &r.body[..r.body.len().min(300)]
    );
    assert_eq!(tmp_files(&t), before, "the partial upload was removed");
}

#[tokio::test]
async fn attachments_round_trip_through_storage() {
    let t = test_app!();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('enableattachments', '1') ON CONFLICT (name) DO UPDATE SET value = '1'")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let c = t.login_as(1).await;
    let content = b"plain text attachment content\n".repeat(1000);
    let csrf = c.csrf();
    let (ct, body) = multipart(
        &[
            ("my_post_key", &csrf),
            ("posthash", "0123456789abcdefXYZ"),
            ("fid", "3"),
        ],
        Some(("notes.txt", &content)),
    );
    let r = c
        .request(
            Request::post("/attachment/upload")
                .header(header::CONTENT_TYPE, ct)
                .header(
                    header::COOKIE,
                    format!("rbb_auth={}", c.cookie("rbb_auth").unwrap()),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let aid = v["aid"].as_i64().unwrap();
    let (size, key): (i64, String) =
        sqlx::query_as("SELECT filesize, attachname FROM attachments WHERE aid = $1")
            .bind(aid as i32)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
    assert_eq!(size as usize, content.len());
    assert!(t.app.storage.get(&key).await.unwrap().is_some());
    // The uploader can download it before it is attached to a post.
    let d = c.get(&format!("/attachment/{aid}")).await;
    assert_eq!(d.status, 200);
    assert_eq!(d.body.as_bytes(), &content[..]);
}

#[tokio::test]
async fn thumbnail_requests_do_not_bypass_the_download_permission() {
    let t = test_app!();
    sqlx::query("UPDATE usergroups SET perms = jsonb_set(perms, '{candlattachments}', 'false') WHERE gid = 1")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["groups", "forumperms"]).await.unwrap();
    let pid: i32 = sqlx::query_scalar(
        "SELECT p.pid FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.visible = 1 AND t.visible = 1 ORDER BY p.pid LIMIT 1",
    )
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let key = "attachments/test/notes.attach";
    t.app
        .storage
        .put_bytes(key, bytes::Bytes::from_static(b"not for guests"))
        .await
        .unwrap();
    // A non-image attachment has no separate thumbnail.
    let aid: i32 = sqlx::query_scalar(
        "INSERT INTO attachments (pid, uid, filename, filetype, filesize, attachname, visible, thumbnail, dateuploaded) VALUES ($1, 1, 'notes.txt', 'text/plain', 14, $2, TRUE, '', 1) RETURNING aid",
    )
    .bind(pid)
    .bind(key)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    let guest = t.client();
    assert_eq!(guest.get(&format!("/attachment/{aid}")).await.status, 403);
    let r = guest.get(&format!("/attachment/{aid}?thumb=1")).await;
    assert_eq!(
        r.status, 403,
        "the thumbnail fallback served the original file"
    );
}

#[tokio::test]
async fn live_streams_are_capped_per_address() {
    let t = test_app!();
    let uid = t.create_user("streamer", "Passw0rd-streamer").await;
    let c = t.login_as(uid).await;
    let cookie = format!("rbb_auth={}", c.cookie("rbb_auth").unwrap());
    let mut open = vec![];
    let mut refused = 0;
    for _ in 0..12 {
        let resp = t
            .router
            .clone()
            .oneshot(
                Request::get("/live")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if resp.status() == 429 {
            refused += 1;
        } else {
            assert_eq!(resp.status(), 200);
            open.push(resp); // keep the stream open
        }
    }
    // Per member: 8 by default.
    assert_eq!(open.len(), 8);
    assert_eq!(refused, 4);
    drop(open);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        t.app.streams.open(),
        0,
        "slots are released when streams end"
    );
}
