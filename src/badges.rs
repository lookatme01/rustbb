//! Badges: achievements earned automatically from activity (hourly task) or awarded by staff.
//! Definitions live in the configuration cache; awards in `user_badges`.

use crate::admin::promotions::{REQS, op_sql};
use crate::app::{App, LiveEvent};
use crate::cache::Cache;
use crate::models::Badge;
use crate::util::now;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;

/// Built-in icons (drawn by the `badge_icon` template macro). Must match the CHECK in 0024.
pub const ICONS: &[(&str, &str)] = &[
    ("award", "Ribbon"),
    ("calendar", "Calendar"),
    ("chat", "Speech bubble"),
    ("star", "Star"),
    ("flame", "Flame"),
    ("heart", "Heart"),
    ("users", "People"),
    ("shield", "Shield"),
    ("trophy", "Trophy"),
    ("sparkle", "Sparkle"),
    ("bolt", "Lightning"),
    ("leaf", "Leaf"),
];

/// Colours (`badge-<color>` CSS classes). Must match the CHECK in 0024.
pub const COLORS: &[(&str, &str)] = &[
    ("bronze", "Bronze"),
    ("silver", "Silver"),
    ("gold", "Gold"),
    ("green", "Green"),
    ("blue", "Blue"),
    ("purple", "Purple"),
    ("red", "Red"),
];

/// Activity a badge can require: the promotion requirements, minus warning points (a badge is
/// never earned for being warned).
pub fn requirements() -> impl Iterator<Item = &'static (&'static str, &'static str, &'static str)> {
    REQS.iter().filter(|(k, _, _)| *k != "warnings")
}

/// "Days registered at least 365 and Post count at least 100", or "" for hand-awarded badges.
pub fn describe(b: &Badge) -> String {
    let words = |op: &str| match op {
        ">" => "more than",
        "<" => "less than",
        "<=" => "at most",
        "=" => "exactly",
        "!=" => "not",
        _ => "at least",
    };
    requirements()
        .filter_map(|(k, label, _)| {
            let r = b.requirements.get(*k)?;
            Some(format!(
                "{label} {} {}",
                words(r[0].as_str().unwrap_or(">=")),
                r[1].as_i64().unwrap_or(0)
            ))
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The SQL condition on `users` for an automatic badge, or None if it's awarded by hand.
/// Operators are whitelisted and values are integers, so the result is safe to splice.
fn condition(b: &Badge) -> Option<String> {
    let conds: Vec<String> = requirements()
        .filter_map(|(k, _, expr)| {
            let r = b.requirements.get(*k)?;
            Some(format!(
                "{expr} {} {}",
                op_sql(r[0].as_str().unwrap_or(">=")),
                r[1].as_i64().unwrap_or(0)
            ))
        })
        .collect();
    (!conds.is_empty()).then(|| conds.join(" AND "))
}

/// A badge as shown on posts and profiles.
#[derive(Serialize, Clone, Debug)]
pub struct Shown {
    pub bid: i32,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub color: String,
    pub dateline: i64,
}

/// Each member's enabled badges, in display order. One query for any number of members.
pub async fn of_members(
    db: &PgPool,
    cache: &Cache,
    uids: &[i32],
) -> sqlx::Result<HashMap<i32, Vec<Shown>>> {
    let mut out: HashMap<i32, Vec<Shown>> = HashMap::new();
    if uids.is_empty() || !cache.badges.iter().any(|b| b.enabled) {
        return Ok(out);
    }
    let rows: Vec<(i32, i32, i64)> =
        sqlx::query_as("SELECT uid, bid, dateline FROM user_badges WHERE uid = ANY($1)")
            .bind(uids)
            .fetch_all(db)
            .await?;
    for (uid, bid, dateline) in rows {
        if let Some(b) = cache.badge(bid).filter(|b| b.enabled) {
            out.entry(uid).or_default().push(Shown {
                bid,
                name: b.name.clone(),
                description: b.description.clone(),
                icon: b.icon.clone(),
                color: b.color.clone(),
                dateline,
            });
        }
    }
    let order: HashMap<i32, usize> = cache
        .badges
        .iter()
        .enumerate()
        .map(|(i, b)| (b.bid, i))
        .collect();
    for list in out.values_mut() {
        list.sort_by_key(|s| order.get(&s.bid).copied().unwrap_or(usize::MAX));
    }
    Ok(out)
}

/// Alerts above this many per badge per run are still recorded, but not pushed live.
const LIVE_ALERT_LIMIT: usize = 100;

/// Insert awards and their alerts in one statement. Returns who newly got the badge.
async fn award_where(
    app: &App,
    b: &Badge,
    who: &str,
    manual: Option<(i32, &str)>,
) -> sqlx::Result<Vec<i32>> {
    let cache = app.cache();
    let alerts = cache.settings.bool("enablealerts");
    let sql = format!(
        "WITH new AS (
             INSERT INTO user_badges (uid, bid, dateline, manual, awarded_by, reason)
             SELECT uid, $1, $2, $5, $6, $7 FROM users
             WHERE {who} AND usergroup NOT IN (1, 5, 7) AND NOT is_system
             ON CONFLICT DO NOTHING RETURNING uid
         ), alerted AS (
             INSERT INTO alerts (uid, from_uid, kind, object_id, extra, dateline)
             SELECT uid, $3, 'badge', $1, $4, $2 FROM new WHERE $8 RETURNING uid
         ), counted AS (
             UPDATE users SET unreadalerts = unreadalerts + 1 WHERE uid IN (SELECT uid FROM alerted)
         )
         SELECT uid FROM new"
    );
    let uids: Vec<i32> = sqlx::query_scalar(&sql)
        .bind(b.bid)
        .bind(now())
        .bind(cache.system_uid)
        .bind(serde_json::json!({ "badge": b.name }))
        .bind(manual.is_some())
        .bind(manual.map(|(by, _)| by).filter(|by| *by > 0))
        .bind(manual.map(|(_, r)| r).unwrap_or(""))
        .bind(alerts)
        .fetch_all(&app.db)
        .await?;
    if alerts && uids.len() <= LIVE_ALERT_LIMIT {
        for uid in &uids {
            app.publish_all(LiveEvent {
                kind: "alert",
                tid: 0,
                uid: *uid,
                data: serde_json::json!({"kind": "badge"}),
            })
            .await;
        }
    }
    Ok(uids)
}

/// Task: award every enabled automatic badge to the members who now qualify.
pub async fn run(app: &App) -> anyhow::Result<String> {
    let badges = app.cache().badges.clone();
    let mut total = 0;
    for b in badges.iter().filter(|b| b.enabled) {
        if let Some(cond) = condition(b) {
            total += award_where(app, b, &cond, None).await?.len();
        }
    }
    Ok(format!("awarded {total} badges"))
}

/// Award a badge by hand. False if the member already has it or can't hold badges.
pub async fn award(app: &App, uid: i32, b: &Badge, by: i32, reason: &str) -> sqlx::Result<bool> {
    let who = format!("uid = {uid}");
    Ok(!award_where(app, b, &who, Some((by, reason)))
        .await?
        .is_empty())
}

/// Take a badge away. An automatic badge is earned again on the next run if the member still
/// qualifies, so revoking those is mostly useful together with disabling the badge.
pub async fn revoke(db: &PgPool, uid: i32, bid: i32) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("DELETE FROM user_badges WHERE uid = $1 AND bid = $2")
            .bind(uid)
            .bind(bid)
            .execute(db)
            .await?
            .rows_affected()
            > 0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn badge(reqs: serde_json::Value) -> Badge {
        Badge {
            bid: 1,
            name: "x".into(),
            description: String::new(),
            icon: "award".into(),
            color: "gold".into(),
            requirements: reqs,
            enabled: true,
            disporder: 0,
        }
    }

    #[test]
    fn conditions_use_whitelisted_operators_and_integers() {
        let b = badge(
            serde_json::json!({"posts": ["; DROP TABLE users", 5], "registered_days": [">", 365]}),
        );
        let c = condition(&b).unwrap();
        assert!(c.contains("postnum >= 5"), "{c}");
        assert!(c.contains("> 365"), "{c}");
        assert!(!c.contains("DROP"));
    }

    #[test]
    fn warnings_are_never_a_requirement() {
        let b = badge(serde_json::json!({"warnings": [">=", 10]}));
        assert!(condition(&b).is_none());
        assert_eq!(describe(&b), "");
    }

    #[test]
    fn hand_awarded_badges_have_no_condition() {
        assert!(condition(&badge(serde_json::json!({}))).is_none());
    }

    #[test]
    fn descriptions_read_naturally() {
        let b = badge(serde_json::json!({"registered_days": [">=", 365]}));
        assert_eq!(describe(&b), "Days registered at least 365");
    }
}
