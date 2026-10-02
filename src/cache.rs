//! In-memory caches of rarely-changing board configuration.
//!
//! Everything here is loaded at startup and reloaded when any node publishes a
//! `NOTIFY rbb_cache, '<part>'`, so multiple app servers stay coherent without Redis.

use crate::models::*;
use crate::parser::{ParserData, Smilie};
use crate::perms::{ForumPerms, GroupPerms, ModPerms};
use crate::settings::Settings;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct Cache {
    pub settings: Arc<Settings>,
    pub groups: Arc<HashMap<i32, UserGroup>>,
    /// Forums in display order (depth-first tree order).
    pub forums: Arc<Vec<Forum>>,
    pub forum_idx: Arc<HashMap<i32, usize>>,
    pub forum_depth: Arc<HashMap<i32, usize>>,
    pub forum_perms: Arc<HashMap<(i32, i32), ForumPerms>>,
    pub moderators: Arc<Vec<Moderator>>,
    pub parser: Arc<ParserData>,
    pub smilies: Arc<Vec<SmilieRow>>,
    pub icons: Arc<Vec<Icon>>,
    pub prefixes: Arc<Vec<Prefix>>,
    pub themes: Arc<Vec<Theme>>,
    pub templates: Arc<HashMap<(i32, String), String>>,
    pub attachtypes: Arc<Vec<AttachType>>,
    pub profilefields: Arc<Vec<ProfileField>>,
    pub usertitles: Arc<Vec<UserTitle>>,
    pub reportreasons: Arc<Vec<ReportReason>>,
    pub announcements: Arc<Vec<Announcement>>,
    pub calendars: Arc<Vec<Calendar>>,
    /// The built-in System account and its group (0 until `system::ensure` has run).
    pub system_uid: i32,
    pub system_gid: i32,
    /// Bumped whenever parsing inputs change (smilies, word filters, MyCodes, forum parse options).
    pub parser_rev: i32,
    /// Public settings as a template value (`bb` in templates), built once per settings change
    /// instead of on every page view.
    pub bb: minijinja::Value,
    /// Per theme: stylesheet version hash (for cache-busting its CSS URL).
    pub theme_versions: Arc<HashMap<i32, String>>,
    /// Per theme, resolved through parent themes: (brand colour, has any custom CSS).
    pub theme_look: Arc<HashMap<i32, (String, bool)>>,
}

pub const PARTS: &[&str] = &[
    "settings",
    "groups",
    "forums",
    "forumperms",
    "moderators",
    "parser",
    "icons",
    "prefixes",
    "themes",
    "templates",
    "attachtypes",
    "profilefields",
    "usertitles",
    "reportreasons",
    "announcements",
    "calendars",
];

impl Cache {
    pub async fn load_all(db: &PgPool) -> anyhow::Result<Cache> {
        let mut c = Cache::default();
        for p in PARTS {
            c.reload(db, p).await?;
        }
        Ok(c)
    }

    pub async fn reload(&mut self, db: &PgPool, part: &str) -> anyhow::Result<()> {
        match part {
            "settings" => {
                let rows: Vec<(String, String)> =
                    sqlx::query_as("SELECT name, value FROM settings")
                        .fetch_all(db)
                        .await?;
                let s = Settings::from_rows(rows);
                self.parser_rev = s.int("parser_rev") as i32;
                let mut bb = s.public_map();
                bb.remove("tos");
                bb.remove("privacypolicy");
                self.bb = minijinja::Value::from_serialize(&bb);
                self.settings = Arc::new(s);
            }
            "groups" => {
                let rows: Vec<UserGroup> =
                    sqlx::query_as("SELECT * FROM usergroups ORDER BY disporder, gid")
                        .fetch_all(db)
                        .await?;
                self.system_gid = rows
                    .iter()
                    .find(|g| g.is_system)
                    .map(|g| g.gid)
                    .unwrap_or(0);
                self.groups = Arc::new(rows.into_iter().map(|g| (g.gid, g)).collect());
                self.system_uid = sqlx::query_scalar("SELECT uid FROM users WHERE is_system")
                    .fetch_optional(db)
                    .await?
                    .unwrap_or(0);
            }
            "forums" => {
                let rows: Vec<Forum> = sqlx::query_as(
                    "SELECT fid, name, description, linkto, type, pid, parentlist, disporder, active, open, allowhtml, allowmycode, allowsmilies, allowimgcode, allowvideocode, allowpicons, allowtratings, usepostcounts, usethreadcounts, requireprefix, password, showinjump, style, overridestyle, rulestype, rulestitle, rules, defaultdatecut, defaultsortby, defaultsortorder FROM forums ORDER BY disporder, fid",
                )
                .fetch_all(db)
                .await?;
                // order depth-first
                let mut children: HashMap<i32, Vec<Forum>> = HashMap::new();
                for f in rows {
                    children.entry(f.pid).or_default().push(f);
                }
                let mut ordered = Vec::new();
                let mut depth = HashMap::new();
                fn walk(
                    pid: i32,
                    d: usize,
                    children: &mut HashMap<i32, Vec<Forum>>,
                    out: &mut Vec<Forum>,
                    depth: &mut HashMap<i32, usize>,
                ) {
                    if let Some(kids) = children.remove(&pid) {
                        for k in kids {
                            let fid = k.fid;
                            depth.insert(fid, d);
                            out.push(k);
                            walk(fid, d + 1, children, out, depth);
                        }
                    }
                }
                walk(0, 0, &mut children, &mut ordered, &mut depth);
                self.forum_idx = Arc::new(
                    ordered
                        .iter()
                        .enumerate()
                        .map(|(i, f)| (f.fid, i))
                        .collect(),
                );
                self.forums = Arc::new(ordered);
                self.forum_depth = Arc::new(depth);
            }
            "forumperms" => {
                let rows: Vec<(i32, i32, sqlx::types::Json<ForumPerms>)> =
                    sqlx::query_as("SELECT fid, gid, perms FROM forumpermissions")
                        .fetch_all(db)
                        .await?;
                self.forum_perms =
                    Arc::new(rows.into_iter().map(|(f, g, p)| ((f, g), p.0)).collect());
            }
            "moderators" => {
                self.moderators = Arc::new(
                    sqlx::query_as("SELECT * FROM moderators")
                        .fetch_all(db)
                        .await?,
                );
            }
            "parser" => {
                let smilies: Vec<SmilieRow> =
                    sqlx::query_as("SELECT * FROM smilies ORDER BY disporder, sid")
                        .fetch_all(db)
                        .await?;
                let bad: Vec<(String, bool, String)> =
                    sqlx::query_as("SELECT badword, regex, replacement FROM badwords")
                        .fetch_all(db)
                        .await?;
                let custom: Vec<(String, String)> = sqlx::query_as(
                    "SELECT regex, replacement FROM mycode WHERE active ORDER BY parseorder, cid",
                )
                .fetch_all(db)
                .await?;
                let sm: Vec<Smilie> = smilies
                    .iter()
                    .flat_map(|s| {
                        s.find
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .map(|f| Smilie {
                                find: f.trim().to_string(),
                                image: s.image.clone(),
                                name: s.name.clone(),
                            })
                    })
                    .collect();
                self.parser = Arc::new(ParserData::new(sm, bad, custom));
                self.smilies = Arc::new(smilies);
            }
            "icons" => {
                self.icons = Arc::new(
                    sqlx::query_as("SELECT * FROM icons ORDER BY name")
                        .fetch_all(db)
                        .await?,
                )
            }
            "prefixes" => {
                self.prefixes = Arc::new(
                    sqlx::query_as("SELECT * FROM threadprefixes ORDER BY prefix")
                        .fetch_all(db)
                        .await?,
                )
            }
            "themes" => {
                let themes: Vec<Theme> = sqlx::query_as("SELECT * FROM themes ORDER BY tid")
                    .fetch_all(db)
                    .await?;
                // The version covers the stylesheet and the properties (brand colour, banner…).
                self.theme_versions = Arc::new(
                    themes
                        .iter()
                        .map(|t| {
                            let src = format!("{}\u{0}{}", t.stylesheet, t.properties.0);
                            (t.tid, crate::util::sha256_hex(&src)[..10].to_string())
                        })
                        .collect(),
                );
                let by_id: HashMap<i32, &Theme> = themes.iter().map(|t| (t.tid, t)).collect();
                self.theme_look = Arc::new(
                    themes
                        .iter()
                        .map(|t| {
                            let (mut brand, mut css, mut cur, mut n) =
                                (String::new(), false, Some(t), 0);
                            while let Some(x) = cur {
                                if brand.is_empty()
                                    && let Some(b) =
                                        x.properties.0.get("brand").and_then(|v| v.as_str())
                                    && crate::admin::themes::valid_brand(b)
                                {
                                    brand = b.to_string();
                                }
                                css |= !x.stylesheet.trim().is_empty();
                                n += 1;
                                cur = if x.pid > 0 && n < 16 {
                                    by_id.get(&x.pid).copied()
                                } else {
                                    None
                                };
                            }
                            (t.tid, (brand, css))
                        })
                        .collect(),
                );
                self.themes = Arc::new(themes);
            }
            "templates" => {
                let rows: Vec<(i32, String, String)> =
                    sqlx::query_as("SELECT theme, title, template FROM templates")
                        .fetch_all(db)
                        .await?;
                self.templates = Arc::new(rows.into_iter().map(|(t, n, s)| ((t, n), s)).collect());
            }
            "attachtypes" => {
                self.attachtypes = Arc::new(
                    sqlx::query_as("SELECT * FROM attachtypes ORDER BY extension")
                        .fetch_all(db)
                        .await?,
                )
            }
            "profilefields" => {
                self.profilefields = Arc::new(
                    sqlx::query_as("SELECT * FROM profilefields ORDER BY disporder, fid")
                        .fetch_all(db)
                        .await?,
                )
            }
            "usertitles" => {
                self.usertitles = Arc::new(
                    sqlx::query_as("SELECT * FROM usertitles ORDER BY posts DESC")
                        .fetch_all(db)
                        .await?,
                )
            }
            "reportreasons" => {
                self.reportreasons = Arc::new(
                    sqlx::query_as("SELECT * FROM reportreasons ORDER BY disporder, rid")
                        .fetch_all(db)
                        .await?,
                )
            }
            "announcements" => {
                self.announcements = Arc::new(
                    sqlx::query_as("SELECT * FROM announcements ORDER BY startdate DESC")
                        .fetch_all(db)
                        .await?,
                )
            }
            "calendars" => {
                self.calendars = Arc::new(
                    sqlx::query_as("SELECT * FROM calendars ORDER BY disporder, cid")
                        .fetch_all(db)
                        .await?,
                )
            }
            other => anyhow::bail!("unknown cache part {other}"),
        }
        Ok(())
    }

    pub fn forum(&self, fid: i32) -> Option<&Forum> {
        self.forum_idx.get(&fid).map(|&i| &self.forums[i])
    }

    pub fn children(&self, pid: i32) -> impl Iterator<Item = &Forum> {
        self.forums.iter().filter(move |f| f.pid == pid)
    }

    /// All descendants of a forum (not including itself).
    pub fn descendants(&self, fid: i32) -> Vec<i32> {
        self.forums
            .iter()
            .filter(|f| f.fid != fid && f.parentlist.contains(&fid))
            .map(|f| f.fid)
            .collect()
    }

    /// Whether `uid` is the built-in System account.
    pub fn is_system(&self, uid: i32) -> bool {
        uid > 0 && uid == self.system_uid
    }

    pub fn group(&self, gid: i32) -> Option<&UserGroup> {
        self.groups.get(&gid)
    }

    pub fn group_perms(&self, groups: &[i32]) -> GroupPerms {
        let mut it = groups.iter().filter_map(|g| self.groups.get(g));
        let Some(first) = it.next() else {
            return GroupPerms::guest();
        };
        let mut p = first.perms.0.clone();
        for g in it {
            p.merge_groups(&g.perms.0);
        }
        p
    }

    /// Effective forum permissions for a set of groups. Per-forum overrides are inherited from the
    /// nearest ancestor that defines one; groups without an override use their global permissions.
    pub fn forum_perms(&self, groups: &[i32], fid: i32) -> ForumPerms {
        let Some(forum) = self.forum(fid) else {
            return ForumPerms::none();
        };
        let mut result: Option<ForumPerms> = None;
        for gid in groups {
            let mut found = None;
            for f in forum.parentlist.iter().rev() {
                if let Some(p) = self.forum_perms.get(&(*f, *gid)) {
                    found = Some(p.clone());
                    break;
                }
            }
            let p = found.unwrap_or_else(|| {
                self.groups
                    .get(gid)
                    .map(|g| ForumPerms::from_group(&g.perms.0))
                    .unwrap_or_else(ForumPerms::none)
            });
            match &mut result {
                None => result = Some(p),
                Some(r) => r.merge_groups(&p),
            }
        }
        let mut r = result.unwrap_or_else(ForumPerms::none);
        // A forum is only viewable if all of its parents are.
        if forum.parentlist.len() > 1 {
            for f in &forum.parentlist[..forum.parentlist.len() - 1] {
                if !self.forum_perms_single(groups, *f) {
                    r.canview = false;
                }
            }
        }
        r
    }

    fn forum_perms_single(&self, groups: &[i32], fid: i32) -> bool {
        let Some(forum) = self.forum(fid) else {
            return false;
        };
        groups.iter().any(|gid| {
            for f in forum.parentlist.iter().rev() {
                if let Some(p) = self.forum_perms.get(&(*f, *gid)) {
                    return p.canview;
                }
            }
            self.groups
                .get(gid)
                .map(|g| g.perms.0.canview)
                .unwrap_or(false)
        })
    }

    /// Moderator permissions for a user in a forum (inherited from parent forums).
    pub fn mod_perms(
        &self,
        uid: i32,
        groups: &[i32],
        gperms: &GroupPerms,
        fid: i32,
    ) -> Option<ModPerms> {
        if gperms.issupermod || gperms.cancp {
            return Some(ModPerms::all());
        }
        if uid == 0 {
            return None;
        }
        let forum = self.forum(fid)?;
        let mut result: Option<ModPerms> = None;
        for m in self.moderators.iter() {
            if !forum.parentlist.contains(&m.fid) {
                continue;
            }
            let applies = if m.isgroup {
                groups.contains(&m.id)
            } else {
                m.id == uid
            };
            if applies {
                match &mut result {
                    None => result = Some(m.perms.0.clone()),
                    Some(r) => r.merge(&m.perms.0),
                }
            }
        }
        result
    }

    /// True if the user moderates any forum at all.
    pub fn is_any_moderator(&self, uid: i32, groups: &[i32], gperms: &GroupPerms) -> bool {
        gperms.issupermod
            || gperms.cancp
            || (uid > 0
                && self.moderators.iter().any(|m| {
                    if m.isgroup {
                        groups.contains(&m.id)
                    } else {
                        m.id == uid
                    }
                }))
    }

    pub fn moderated_forums(
        &self,
        uid: i32,
        groups: &[i32],
        gperms: &GroupPerms,
    ) -> Option<Vec<i32>> {
        if gperms.issupermod || gperms.cancp {
            return None; // all
        }
        let mut v: Vec<i32> = Vec::new();
        for m in self.moderators.iter() {
            let applies = if m.isgroup {
                groups.contains(&m.id)
            } else {
                m.id == uid
            };
            if applies {
                v.push(m.fid);
                v.extend(self.descendants(m.fid));
            }
        }
        v.sort();
        v.dedup();
        Some(v)
    }

    pub fn default_theme(&self) -> i32 {
        self.themes
            .iter()
            .find(|t| t.def)
            .or(self.themes.first())
            .map(|t| t.tid)
            .unwrap_or(1)
    }

    pub fn theme(&self, tid: i32) -> Option<&Theme> {
        self.themes.iter().find(|t| t.tid == tid)
    }

    pub fn usertitle_for(&self, posts: i32) -> Option<&UserTitle> {
        self.usertitles.iter().find(|t| posts >= t.posts)
    }

    pub fn icon(&self, iid: i32) -> Option<&Icon> {
        self.icons.iter().find(|i| i.iid == iid)
    }

    pub fn prefix(&self, pid: i32) -> Option<&Prefix> {
        self.prefixes.iter().find(|p| p.pid == pid)
    }

    pub fn prefixes_for(&self, fid: i32, groups: &[i32]) -> Vec<Prefix> {
        let parents = self
            .forum(fid)
            .map(|f| f.parentlist.clone())
            .unwrap_or_default();
        self.prefixes
            .iter()
            .filter(|p| p.forums.is_empty() || p.forums.iter().any(|f| parents.contains(f)))
            .filter(|p| p.groups.is_empty() || p.groups.iter().any(|g| groups.contains(g)))
            .cloned()
            .collect()
    }

    /// Username formatted with the display group's name style (HTML, trusted from ACP).
    pub fn format_name(&self, username: &str, usergroup: i32, displaygroup: i32) -> String {
        let gid = if displaygroup > 0 {
            displaygroup
        } else {
            usergroup
        };
        let name = crate::util::escape_html(username);
        match self.groups.get(&gid) {
            Some(g) if g.namestyle.contains("{username}") => {
                g.namestyle.replace("{username}", &name)
            }
            _ => name,
        }
    }

    pub fn reaction_types(&self) -> Vec<(String, String)> {
        self.settings
            .get("reactiontypes")
            .split(',')
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .filter(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .collect()
    }
}
