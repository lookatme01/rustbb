mod common;

#[tokio::test]
async fn member_pagination_keeps_the_website_filter() {
    let t = test_app!();
    let website = "https://example.test/?foo=1&bar=2";
    sqlx::query("INSERT INTO users (username, password, email, usergroup, regdate, website) SELECT 'web' || lpad(i::text, 3, '0'), 'disabled', 'web' || i || '@example.test', 2, 1, $1 FROM generate_series(0, 5) i")
        .bind(website).execute(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO settings (name, value) VALUES ('membersperpage', '5') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value")
        .execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["settings"]).await.unwrap();
    let c = t.client();
    let query =
        serde_urlencoded::to_string([("website", website), ("sort", "username"), ("order", "asc")])
            .unwrap();
    let r = c.get(&format!("/members?{query}")).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("web004"));
    assert!(!r.body.contains("web005"));
    let re = regex::Regex::new(r#"href="([^"]*page=2[^"]*)""#).unwrap();
    let link = re.captures(&r.body).unwrap()[1]
        .replace("&amp;", "&")
        .replace("&#x2f;", "/");
    let url = url::Url::parse(&format!("http://localhost{link}")).unwrap();
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "website" && v == website)
    );
    let r = c.get(&link).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("web005"));
    assert!(!r.body.contains("web004"));
    assert!(
        r.body.contains(r#"name="website""#),
        "the search form lost the filter"
    );
}

#[tokio::test]
async fn team_groups_keep_their_order_memberships_and_individual_limits() {
    let t = test_app!();
    sqlx::query("INSERT INTO usergroups (gid, title, disporder, perms) VALUES (20, 'First staff section', 10, '{\"showforumteam\":true}'), (21, 'Second staff section', 11, '{\"showforumteam\":true}')")
        .execute(&t.db.pool).await.unwrap();
    sqlx::query("INSERT INTO users (username, password, email, usergroup, regdate, additionalgroups) SELECT 'team' || lpad(i::text, 3, '0'), 'disabled', 'team' || i || '@example.test', 20, 1, CASE WHEN i = 0 THEN '{20,21}'::int[] ELSE '{}'::int[] END FROM generate_series(0, 204) i")
        .execute(&t.db.pool).await.unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let r = t.client().get("/team").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let first = r.body.find("First staff section").unwrap();
    let second = r.body.find("Second staff section").unwrap();
    assert!(first < second);
    let one = &r.body[first..second];
    assert_eq!(
        one.matches("team000").count(),
        1,
        "primary and additional membership duplicated a user"
    );
    assert!(one.contains("team199"));
    assert!(!one.contains("team200"));
    assert!(
        r.body[second..].contains("team000"),
        "additional group membership was omitted"
    );
}

#[tokio::test]
async fn filtering_the_member_list_by_a_hidden_group_does_not_list_its_members() {
    let t = test_app!();
    sqlx::query("INSERT INTO usergroups (gid, title, disporder, perms) VALUES (30, 'Quiet group', 40, '{\"showmemberlist\":false}')")
        .execute(&t.db.pool)
        .await
        .unwrap();
    // A visible-group member who also belongs to the hidden group.
    sqlx::query("INSERT INTO users (username, password, email, usergroup, regdate, additionalgroups) VALUES ('secretmember', 'disabled', 'secret@example.test', 2, 1, '{30}')")
        .execute(&t.db.pool)
        .await
        .unwrap();
    t.app.invalidate(&["groups"]).await.unwrap();
    let all = t.client().get("/members").await;
    assert!(all.body.contains("secretmember"), "{}", all.status);
    let r = t.client().get("/members?gid=30").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains("secretmember"));
}

#[tokio::test]
async fn an_invisible_member_is_counted_once_in_the_online_total() {
    let t = test_app!();
    let uid = t.create_user("ghost", "Passw0rd-ghost").await;
    sqlx::query("UPDATE users SET invisible = TRUE WHERE uid = $1")
        .bind(uid)
        .execute(&t.db.pool)
        .await
        .unwrap();
    // The board already has them in the sessions table, as it would after any earlier visit.
    sqlx::query("INSERT INTO sessions (sid, uid, time) VALUES ('ghost-session', $1, $2)")
        .bind(uid)
        .bind(rbb::util::now())
        .execute(&t.db.pool)
        .await
        .unwrap();
    let c = t.login_as(uid).await;
    let page = c.get("/").await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("1 here now"), "counted once: {}", {
        let i = page.body.find("here now").unwrap_or(0);
        page.body[i.saturating_sub(60)..(i + 20).min(page.body.len())].to_string()
    });
}
