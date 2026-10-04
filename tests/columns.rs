//! Every explicit column list matches its table and decodes into its struct.

mod common;

use rbb::models::*;

macro_rules! check {
    ($t:expr, $ty:ty, $cols:expr, $table:expr) => {{
        let rows: Vec<$ty> = sqlx::query_as(&format!("SELECT {} FROM {}", $cols, $table))
            .fetch_all(&$t.db.pool)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", $table));
        rows.len()
    }};
}

#[tokio::test]
async fn column_lists_match_their_structs() {
    let t = test_app!();
    assert!(check!(t, User, USER_COLUMNS, "users") > 0);
    assert!(check!(t, UserGroup, GROUP_COLUMNS, "usergroups") > 0);
    check!(t, Thread, THREAD_COLUMNS, "threads");
    assert!(check!(t, Theme, THEME_COLUMNS, "themes") > 0);
    check!(t, Icon, ICON_COLUMNS, "icons");
    check!(t, Prefix, PREFIX_COLUMNS, "threadprefixes");
    check!(t, Moderator, MODERATOR_COLUMNS, "moderators");
    check!(t, AttachType, ATTACHTYPE_COLUMNS, "attachtypes");
    check!(t, ProfileField, PROFILEFIELD_COLUMNS, "profilefields");
    check!(t, UserTitle, USERTITLE_COLUMNS, "usertitles");
    check!(t, SmilieRow, SMILIE_COLUMNS, "smilies");
    check!(t, Announcement, ANNOUNCEMENT_COLUMNS, "announcements");
    check!(t, ReportReason, REPORTREASON_COLUMNS, "reportreasons");
    check!(t, Calendar, CALENDAR_COLUMNS, "calendars");
    assert!(check!(t, Badge, BADGE_COLUMNS, "badges") > 0);
    check!(t, Post, POST_COLUMNS, "posts");
    // Joined, qualified variants.
    let n: Vec<User> = sqlx::query_as(&format!("SELECT {} FROM users u", USER_COLUMNS_U.as_str()))
        .fetch_all(&t.db.pool)
        .await
        .unwrap();
    assert!(!n.is_empty());
}
