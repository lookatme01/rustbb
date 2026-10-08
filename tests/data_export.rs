//! The User CP data export: a zip split into account, posts and private messages, carrying only
//! the member's own IP addresses.

mod common;

use std::collections::HashMap;
use std::io::Read;

fn unzip(bytes: &[u8]) -> HashMap<String, String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a zip file");
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            (f.name().to_string(), s)
        })
        .collect()
}

fn json(files: &HashMap<String, String>, name: &str) -> serde_json::Value {
    serde_json::from_str(files.get(name).unwrap_or_else(|| panic!("{name} missing"))).unwrap()
}

#[tokio::test]
async fn export_is_a_zip_split_by_kind_with_only_the_members_own_addresses() {
    let t = test_app!();
    let alice = t.create_user("alice", "Passw0rd-alice").await;
    let bob = t.create_user("bob", "Passw0rd-bob").await;
    t.create_user("carol", "Passw0rd-carol").await;
    let fid: i32 = sqlx::query_scalar("SELECT fid FROM forums WHERE name = 'General Discussion'")
        .fetch_one(&t.db.pool)
        .await
        .unwrap();

    let a = t.login_as(alice).await;
    let r = a
        .send_json(
            "POST",
            &format!("/api/v1/forums/{fid}/threads"),
            serde_json::json!({"subject": "Alice's thread", "message": "Opening post"}),
            &[("x-csrf-token", &a.csrf())],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = a
        .post_form(
            "/pm/send",
            &[
                ("to", "bob"),
                ("bcc", "carol"),
                ("subject", "From Alice"),
                ("message", "Hi Bob"),
                ("savecopy", "1"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);

    let b = t.login_as(bob).await;
    let r = b
        .post_form(
            "/pm/send",
            &[
                ("to", "alice"),
                ("bcc", "carol"),
                ("subject", "From Bob"),
                ("message", "Hi Alice"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.body);

    // The in-process client has no peer address, so give the rows real ones.
    sqlx::query("UPDATE posts SET ipaddress = '203.0.113.10' WHERE uid = $1")
        .bind(alice)
        .execute(&t.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE privatemessages SET ipaddress = CASE WHEN fromid = $1 THEN '203.0.113.10'::inet ELSE '192.0.2.99'::inet END")
        .bind(alice).execute(&t.db.pool).await.unwrap();
    sqlx::query("UPDATE logins SET ip = '203.0.113.10' WHERE uid = $1")
        .bind(alice)
        .execute(&t.db.pool)
        .await
        .unwrap();

    // A sign-in from another address, and a staff action that carries the staff member's address.
    sqlx::query("INSERT INTO user_audit (uid, dateline, action, ipaddress, useragent) VALUES ($1, 100, 'login', '198.51.100.20', 'Mozilla/5.0 (Windows NT 10.0) Firefox/130.0')")
        .bind(alice).execute(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO user_audit (uid, dateline, action, ipaddress, useragent, actor_uid) VALUES ($1, 200, 'warned', '198.51.100.77', 'staff browser', 1)")
        .bind(alice).execute(&t.db.pool).await.unwrap();

    let r = a.get("/usercp/export").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["content-type"], "application/zip");
    assert!(
        r.headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains(".zip")
    );
    let files = unzip(&r.bytes);
    for name in [
        "README.txt",
        "user/profile.json",
        "user/ip_addresses.json",
        "user/sign_ins.json",
        "user/account_activity.json",
        "user/devices.json",
        "posts/posts.json",
        "private_messages/inbox.json",
        "private_messages/sent_items.json",
        "private_messages/drafts.json",
        "private_messages/trash_can.json",
    ] {
        assert!(
            files.contains_key(name),
            "{name} missing from {:?}",
            files.keys()
        );
    }

    assert_eq!(json(&files, "user/profile.json")["username"], "alice");
    let posts = json(&files, "posts/posts.json");
    assert_eq!(posts[0]["thread"], "Alice's thread");
    assert_eq!(posts[0]["message"], "Opening post");
    assert_eq!(posts[0]["ip_address"], "203.0.113.10");

    // Received: the sender's address and blind copies stay private.
    let inbox = json(&files, "private_messages/inbox.json");
    let received = &inbox["messages"][0];
    assert_eq!(received["from"], "bob");
    assert_eq!(received["to"], serde_json::json!(["alice"]));
    assert!(
        received.get("ip_address").is_none() && received.get("bcc").is_none(),
        "{received}"
    );
    // Sent: your own copy shows everything.
    let sent = &json(&files, "private_messages/sent_items.json")["messages"][0];
    assert_eq!(sent["subject"], "From Alice");
    assert_eq!(sent["bcc"], serde_json::json!(["carol"]));
    assert_eq!(sent["ip_address"], "203.0.113.10");

    let sign_ins = json(&files, "user/sign_ins.json");
    assert!(
        sign_ins
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "login"
                && e["ip_address"] == "198.51.100.20"
                && e["device"] == "Firefox on Windows"),
        "{sign_ins}"
    );
    let activity = json(&files, "user/account_activity.json");
    let warned = activity
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "warned")
        .expect("staff action");
    assert_eq!(warned["by_staff"], "admin");
    assert!(warned.get("ip_address").is_none(), "{warned}");

    let ips = json(&files, "user/ip_addresses.json");
    let addrs: Vec<&str> = ips
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["address"].as_str().unwrap())
        .collect();
    assert!(
        addrs.contains(&"203.0.113.10") && addrs.contains(&"198.51.100.20"),
        "{ips}"
    );
    assert!(
        !addrs.contains(&"192.0.2.99"),
        "the sender's address leaked: {ips}"
    );
    assert!(
        !addrs.contains(&"198.51.100.77"),
        "the staff address leaked: {ips}"
    );

    let exported: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND action = 'data_exported'",
    )
    .bind(alice)
    .fetch_one(&t.db.pool)
    .await
    .unwrap();
    assert_eq!(exported, 1);
}

#[tokio::test]
async fn activity_page_hides_the_staff_members_address_and_browser() {
    let t = test_app!();
    let alice = t.create_user("alice", "Passw0rd-alice").await;
    sqlx::query("INSERT INTO user_audit (uid, dateline, action, ipaddress, useragent) VALUES ($1, 100, 'login', '198.51.100.20', 'Mozilla/5.0 (Windows NT 10.0) Firefox/130.0')")
        .bind(alice).execute(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO user_audit (uid, dateline, action, ipaddress, useragent, actor_uid) VALUES ($1, 200, 'warned', '198.51.100.77', 'Mozilla/5.0 (Macintosh) Safari/605.1', 1)")
        .bind(alice).execute(&t.db.pool).await.unwrap();

    let a = t.login_as(alice).await;
    let r = a.get("/usercp/activity").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.body.contains("198.51.100.20"),
        "own sign-in address missing"
    );
    assert!(
        !r.body.contains("198.51.100.77") && !r.body.contains("Macintosh"),
        "the staff member's address or browser leaked"
    );

    // Staff still see the address on the Admin CP view of the same log.
    let admin = t.login_as(1).await;
    sqlx::query("UPDATE logins SET acp_verified = $1 WHERE uid = 1")
        .bind(rbb::util::now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let r = admin.get(&format!("/admin/users/{alice}/activity")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("198.51.100.77"), "{}", r.body);
}
