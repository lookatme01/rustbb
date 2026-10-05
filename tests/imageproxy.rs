//! The image proxy end to end: posts and avatars point at the proxy, the built-in proxy serves
//! signed images and refuses everything else.

mod common;

use axum::http::header;
use common::TestApp;
use std::sync::atomic::Ordering;

const PNG: &[u8] =
    b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";

/// A tiny image host on 127.0.0.1. Returns its base URL.
async fn image_host() -> String {
    use axum::routing::get;
    let app = axum::Router::new()
        .route(
            "/a.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], PNG) }),
        )
        .route(
            "/evil.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    "<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>",
                )
            }),
        )
        .route(
            "/big.png",
            get(|| async {
                let mut v = PNG.to_vec();
                v.resize(200 * 1024, 0);
                ([(header::CONTENT_TYPE, "image/png")], v)
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

async fn settings(t: &TestApp, pairs: &[(&str, &str)]) {
    for (k, v) in pairs {
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
            .bind(k)
            .bind(v)
            .execute(&t.db.pool)
            .await
            .unwrap();
    }
    t.app.invalidate(&["settings"]).await.unwrap();
    t.app.bump_parser_rev().await.unwrap();
}

async fn thread_with_image(t: &TestApp, src: &str) -> i32 {
    let c = t.login_as(1).await;
    let r = c
        .post_form(
            "/newthread/3",
            &[
                ("subject", "Pictures"),
                ("message", &format!("Look: [img]{src}[/img]")),
            ],
        )
        .await;
    assert!(
        r.status.is_redirection(),
        "{} {}",
        r.status,
        r.body.chars().take(400).collect::<String>()
    );
    sqlx::query_scalar("SELECT MAX(tid) FROM threads")
        .fetch_one(&t.db.pool)
        .await
        .unwrap()
}

/// The proxied `src` of the first remote image in a page.
fn proxied_src(body: &str) -> String {
    let i = body.find("src=\"/imgproxy/").expect("image is proxied") + 5;
    body[i..i + body[i..].find('"').unwrap()].to_string()
}

#[tokio::test]
async fn builtin_proxy_serves_signed_images_only() {
    rbb::imageproxy::ALLOW_LOOPBACK_FOR_TESTS.store(true, Ordering::Relaxed);
    let t = test_app!();
    let host = image_host().await;
    settings(
        &t,
        &[("imageproxy", "builtin"), ("imageproxy_maxsize", "64")],
    )
    .await;
    let tid = thread_with_image(&t, &format!("{host}/a.png")).await;
    let guest = t.client();
    let page = guest.get(&format!("/thread/{tid}")).await;
    assert_eq!(page.status, 200);
    assert!(
        !page.body.contains(&format!("src=\"{host}/a.png")),
        "image not rewritten"
    );
    let src = proxied_src(&page.body);

    let r = guest.get(&src).await;
    assert_eq!(r.status, 200, "{src}");
    assert_eq!(r.headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(
        r.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );
    assert!(r.body.contains("PNG"));

    // A forged or altered signature is refused.
    let parts: Vec<&str> = src.split('/').collect();
    let forged = format!("/imgproxy/{}/{}", "00".repeat(20), parts[3]);
    assert_eq!(guest.get(&forged).await.status, 403);
    let other = hex::encode(format!("{host}/b.png"));
    assert_eq!(
        guest
            .get(&format!("/imgproxy/{}/{other}", parts[2]))
            .await
            .status,
        403
    );

    // Signed, but not an image (an SVG with script, mislabelled as PNG): refused.
    let tid = thread_with_image(&t, &format!("{host}/evil.svg")).await;
    let page = guest.get(&format!("/thread/{tid}")).await;
    assert_eq!(guest.get(&proxied_src(&page.body)).await.status, 422);

    // Larger than the configured limit: refused.
    let tid = thread_with_image(&t, &format!("{host}/big.png")).await;
    let page = guest.get(&format!("/thread/{tid}")).await;
    assert_eq!(guest.get(&proxied_src(&page.body)).await.status, 422);
}

#[tokio::test]
async fn external_proxy_rewrites_posts_and_avatars_and_builtin_route_is_off() {
    let t = test_app!();
    settings(
        &t,
        &[
            ("imageproxy", "external"),
            ("imageproxy_url", "https://camo.example.net/"),
            ("imageproxy_key", "shared-secret"),
        ],
    )
    .await;
    sqlx::query("UPDATE users SET avatar = 'https://avatars.example.com/me.png', avatartype = 'remote' WHERE uid = 1")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.avatar_cache.invalidate_all();
    let tid = thread_with_image(&t, "https://images.example.com/cat.jpg").await;
    let page = t.client().get(&format!("/thread/{tid}")).await;
    assert_eq!(page.status, 200);
    let cat = hex::encode("https://images.example.com/cat.jpg");
    assert!(
        page.body.contains(&format!("{cat}\"")),
        "post image not proxied"
    );
    assert!(
        page.body.contains("src=\"https://camo.example.net/"),
        "external base not used"
    );
    assert!(
        !page.body.contains("src=\"https://images.example.com/"),
        "original image URL leaked"
    );
    assert!(
        !page.body.contains("src=\"https://avatars.example.com/"),
        "avatar not proxied"
    );
    assert!(
        !page.body.contains("shared-secret"),
        "proxy key leaked into the page"
    );

    // The built-in route only answers in built-in mode.
    let r = t
        .client()
        .get(&format!("/imgproxy/{}/{cat}", "00".repeat(20)))
        .await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn admins_are_told_the_builtin_proxy_reveals_the_server() {
    let t = test_app!();
    let admin = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(rbb::util::now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let r = admin.get("/admin/settings/images").await;
    assert_eq!(r.status, 200);
    assert!(
        r.body.contains("DDoS-protection CDN"),
        "no settings guidance"
    );
    let r = admin.get("/admin").await;
    assert_eq!(r.status, 200);
    assert!(
        r.body.contains("An image proxy on a separate host"),
        "no tip while off"
    );
    settings(&t, &[("imageproxy", "builtin")]).await;
    let r = admin.get("/admin").await;
    assert!(
        r.body.contains("reveals this server's IP address"),
        "no dashboard warning"
    );
    settings(&t, &[("imageproxy", "external")]).await;
    let r = admin.get("/admin").await;
    assert!(
        r.body.contains("selected but not configured"),
        "no warning for a half-configured proxy"
    );
}
