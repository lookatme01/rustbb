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
