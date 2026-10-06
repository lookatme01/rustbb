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
