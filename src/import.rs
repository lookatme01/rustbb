//! Import a MyBB 1.8 board directly from its MySQL database.
//!
//! IDs are preserved, so old links (`showthread.php?tid=N`) keep resolving through the
//! legacy redirects, and MyBB password hashes are kept in `mybb$salt$hash` form: they
//! verify on first login and are upgraded to argon2id transparently.
//!
//! Each table is streamed from MySQL and written to PostgreSQL in batches through
//! `jsonb_to_recordset`, which coerces values with each column's input function, so
//! MyBB's 0/1 flags become booleans and `{1,2}` strings become arrays.

use crate::perms::{FORUM_PERM_META, GROUP_PERM_META, MOD_PERM_META, PermMeta};
use anyhow::{Context, Result, bail};
use futures::TryStreamExt;
use serde_json::{Map, Value};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, PgPool, Row, TypeInfo};
use std::collections::HashMap;

const BATCH: usize = 2000;

type Obj = Map<String, Value>;

pub struct Importer {
    my: MySqlPool,
    pg: PgPool,
    prefix: String,
    /// Column name → SQL type for each target table (generated columns excluded).
    columns: HashMap<String, Vec<(String, String)>>,
}

fn row_to_obj(r: &MySqlRow) -> Obj {
    let mut m = Obj::new();
    for (i, c) in r.columns().iter().enumerate() {
        let ty = c.type_info().name().to_ascii_uppercase();
        let v = if ty.contains("INT") {
            r.try_get::<Option<i64>, _>(i)
                .ok()
                .flatten()
                .or_else(|| {
                    r.try_get::<Option<u64>, _>(i)
                        .ok()
                        .flatten()
                        .map(|v| v as i64)
                })
                .map(Value::from)
                .unwrap_or(Value::Null)
        } else if ty.contains("DECIMAL") || ty.contains("FLOAT") || ty.contains("DOUBLE") {
            r.try_get::<Option<f64>, _>(i)
                .ok()
                .flatten()
                .map(Value::from)
                .unwrap_or(Value::Null)
        } else {
            match r.try_get::<Option<String>, _>(i) {
                Ok(v) => v.map(Value::from).unwrap_or(Value::Null),
                Err(_) => r
                    .try_get::<Option<Vec<u8>>, _>(i)
                    .ok()
                    .flatten()
                    .map(|b| Value::from(String::from_utf8_lossy(&b).into_owned()))
                    .unwrap_or(Value::Null),
            }
        };
        m.insert(c.name().to_string(), v);
    }
    m
}

/// "1,2,,3" → "{1,2,3}" (a Postgres int[] literal).
fn int_list(v: &Value) -> Value {
    let s = v
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string());
    let ids: Vec<String> = s
        .split(',')
        .filter_map(|x| x.trim().parse::<i64>().ok())
        .filter(|x| *x > 0)
        .map(|x| x.to_string())
        .collect();
    Value::from(format!("{{{}}}", ids.join(",")))
}

fn text_array(items: &[&str]) -> Value {
    Value::from(items.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn int(o: &Obj, k: &str) -> i64 {
    match o.get(k) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(o: &Obj, k: &str) -> String {
    match o.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// Moves the MyBB permission columns of a row into a JSON object keyed like rbb's
/// permission structs, layered over `base` so rbb-only permissions keep sensible values.
fn take_perms(o: &mut Obj, meta: &[PermMeta], base: Value) -> Value {
    let mut p = match base {
        Value::Object(m) => m,
        _ => Obj::new(),
    };
    for m in meta {
        if let Some(v) = o.remove(m.name) {
            let n = match &v {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                Value::String(s) => s.trim().parse().unwrap_or(0),
                _ => continue,
            };
            p.insert(
                m.name.to_string(),
                if m.is_bool {
                    Value::from(n != 0)
                } else {
                    Value::from(n)
                },
            );
        }
    }
    Value::Object(p)
}

impl Importer {
    pub async fn connect(mysql_url: &str, pg: PgPool, prefix: &str) -> Result<Self> {
        let my = MySqlPoolOptions::new()
            .max_connections(2)
            .connect(mysql_url)
            .await
            .context("connecting to the MyBB MySQL database")?;
        Ok(Self {
            my,
            pg,
            prefix: prefix.to_string(),
            columns: HashMap::new(),
        })
    }

    fn t(&self, name: &str) -> String {
        format!("{}{}", self.prefix, name)
    }

    async fn mysql_columns(&self, table: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT CAST(COLUMN_NAME AS CHAR) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
        )
        .bind(self.t(table))
        .fetch_all(&self.my)
        .await?)
    }

    async fn has_table(&self, table: &str) -> Result<bool> {
        Ok(!self.mysql_columns(table).await?.is_empty())
    }

    /// `CAST(col AS SIGNED) AS col` for each permission column MyBB actually has.
    async fn perm_select(&self, table: &str, meta: &[PermMeta]) -> Result<String> {
        let have = self.mysql_columns(table).await?;
        Ok(meta
            .iter()
            .filter(|m| have.iter().any(|h| h == m.name))
            .map(|m| format!(", CAST(`{0}` AS SIGNED) AS `{0}`", m.name))
            .collect())
    }

    async fn pg_columns(&mut self, table: &str) -> Result<Vec<(String, String)>> {
        if let Some(c) = self.columns.get(table) {
            return Ok(c.clone());
        }
        let cols: Vec<(String, String)> = sqlx::query_as(
            "SELECT attname::text, format_type(atttypid, atttypmod) FROM pg_attribute
             WHERE attrelid = $1::regclass AND attnum > 0 AND NOT attisdropped AND attgenerated = '' ORDER BY attnum",
        )
        .bind(table)
        .fetch_all(&self.pg)
        .await?;
        self.columns.insert(table.to_string(), cols.clone());
        Ok(cols)
    }

    async fn write(&mut self, table: &str, rows: &mut Vec<Obj>, conflict: &str) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let cols = self.pg_columns(table).await?;
        let used: Vec<&(String, String)> = cols
            .iter()
            .filter(|(c, _)| rows[0].contains_key(c))
            .collect();
        let names = used
            .iter()
            .map(|(c, _)| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let defs = used
            .iter()
            .map(|(c, t)| format!("\"{c}\" {t}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {table} ({names}) SELECT {names} FROM jsonb_to_recordset($1) AS x({defs}) {conflict}"
        );
        let data = Value::Array(rows.drain(..).map(Value::Object).collect());
        sqlx::query(&sql)
            .bind(sqlx::types::Json(data))
            .execute(&self.pg)
            .await
            .with_context(|| format!("writing {table}"))?;
        Ok(())
    }

    /// Streams `select` from MySQL, applies `fix` to each row and bulk-inserts into `table`.
    async fn copy<F>(
        &mut self,
        label: &str,
        select: &str,
        table: &str,
        conflict: &str,
        mut fix: F,
    ) -> Result<u64>
    where
        F: FnMut(&mut Obj) -> bool,
    {
        let my = self.my.clone();
        let mut stream = sqlx::query(select).fetch(&my);
        let mut batch = Vec::with_capacity(BATCH);
        let mut n = 0u64;
        while let Some(row) = stream
            .try_next()
            .await
            .with_context(|| format!("reading {label}"))?
        {
            let mut o = row_to_obj(&row);
            if !fix(&mut o) {
                continue;
            }
            batch.push(o);
            n += 1;
            if batch.len() >= BATCH {
                self.write(table, &mut batch, conflict).await?;
                if n % 100_000 == 0 {
                    println!("  {label}: {n}");
                }
            }
        }
        self.write(table, &mut batch, conflict).await?;
        println!("  {label}: {n}");
        Ok(n)
    }

    pub async fn run(&mut self, uploads_prefix: &str) -> Result<()> {
        let version: Option<String> = sqlx::query_scalar(&format!(
            "SELECT CAST(value AS CHAR) FROM {} WHERE name = 'bbname'",
            self.t("settings")
        ))
        .fetch_optional(&self.my)
        .await
        .ok()
        .flatten();
        if version.is_none() {
            bail!("no MyBB settings table found with prefix '{}'", self.prefix);
        }

        println!("clearing existing board content…");
        sqlx::query(
            "TRUNCATE users, forums, threads, posts, polls, pollvotes, privatemessages, attachments, reputation,
                      threadsubscriptions, forumsubscriptions, forumpermissions, moderators, threadprefixes, sessions, logins
             RESTART IDENTITY CASCADE",
        )
        .execute(&self.pg)
        .await?;

        // Settings worth carrying over.
        let settings: Vec<(String, String)> = sqlx::query_as(&format!(
            "SELECT CAST(name AS CHAR), CAST(value AS CHAR) FROM {} WHERE name IN ('bbname','bburl','homename','homeurl','adminemail','contactemail')",
            self.t("settings")
        ))
        .fetch_all(&self.my)
        .await?;
        for (k, v) in settings {
            sqlx::query("UPDATE settings SET value = $2 WHERE name = $1")
                .bind(k)
                .bind(v)
                .execute(&self.pg)
                .await?;
        }

        // Usergroups: update the built-in ones in place, add custom ones on top of the
        // Registered group's permissions.
        let existing: HashMap<i32, Value> =
            sqlx::query_as::<_, (i32, Value)>("SELECT gid, perms FROM usergroups")
                .fetch_all(&self.pg)
                .await?
                .into_iter()
                .collect();
        let registered = existing.get(&2).cloned().unwrap_or(Value::Null);
        let psel = self.perm_select("usergroups", GROUP_PERM_META).await?;
        let rows = sqlx::query(&format!(
            "SELECT CAST(gid AS SIGNED) gid, CAST(type AS SIGNED) type, CAST(title AS CHAR) title, CAST(description AS CHAR) description,
                    CAST(namestyle AS CHAR) namestyle, CAST(usertitle AS CHAR) usertitle, CAST(stars AS SIGNED) stars,
                    CAST(disporder AS SIGNED) disporder, CAST(isbannedgroup AS SIGNED) isbannedgroup{psel}
             FROM {} ORDER BY gid",
            self.t("usergroups")
        ))
        .fetch_all(&self.my)
        .await?;
        // Move the System group (see `crate::system`) above every imported gid so no MyBB group
        // lands on it; it has no members at this point because users were truncated.
        let max_gid = rows
            .iter()
            .map(|r| int(&row_to_obj(r), "gid") as i32)
            .max()
            .unwrap_or(0);
        sqlx::query("UPDATE usergroups SET gid = (SELECT GREATEST(MAX(gid), $1) + 1 FROM usergroups) WHERE is_system AND gid <= $1")
            .bind(max_gid)
            .execute(&self.pg)
            .await?;
        for r in &rows {
            let mut o = row_to_obj(r);
            let gid = int(&o, "gid") as i32;
            let perms = take_perms(
                &mut o,
                GROUP_PERM_META,
                existing
                    .get(&gid)
                    .cloned()
                    .unwrap_or_else(|| registered.clone()),
            );
            sqlx::query(
                "INSERT INTO usergroups (gid, type, title, description, namestyle, usertitle, stars, starimage, disporder, isbannedgroup, perms)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, '/static/images/star.svg', $8, $9, $10)
                 ON CONFLICT (gid) DO UPDATE SET title = EXCLUDED.title, description = EXCLUDED.description, namestyle = EXCLUDED.namestyle,
                     usertitle = EXCLUDED.usertitle, stars = EXCLUDED.stars, disporder = EXCLUDED.disporder, perms = EXCLUDED.perms",
            )
            .bind(gid)
            .bind(if int(&o, "type") == 1 { 1i16 } else { 2i16 })
            .bind(text(&o, "title"))
            .bind(text(&o, "description"))
            .bind(text(&o, "namestyle"))
            .bind(text(&o, "usertitle"))
            .bind(int(&o, "stars") as i32)
            .bind(int(&o, "disporder") as i32)
            .bind(int(&o, "isbannedgroup") != 0)
            .bind(perms)
            .execute(&self.pg)
            .await?;
        }
        println!("  usergroups: {}", rows.len());

        println!("importing users…");
        let sel = format!(
            "SELECT CAST(uid AS SIGNED) uid, CAST(username AS CHAR) username, CAST(password AS CHAR) password, CAST(salt AS CHAR) salt,
                    CAST(email AS CHAR) email, CAST(usergroup AS SIGNED) usergroup, CAST(additionalgroups AS CHAR) additionalgroups,
                    CAST(displaygroup AS SIGNED) displaygroup, CAST(usertitle AS CHAR) usertitle, CAST(regdate AS SIGNED) regdate,
                    CAST(lastactive AS SIGNED) lastactive, CAST(lastvisit AS SIGNED) lastvisit, CAST(lastpost AS SIGNED) lastpost,
                    CAST(website AS CHAR) website, CAST(avatar AS CHAR) avatar, CAST(avatardimensions AS CHAR) avatardimensions,
                    CAST(avatartype AS CHAR) avatartype, CAST(signature AS CHAR) signature, CAST(birthday AS CHAR) birthday,
                    CAST(birthdayprivacy AS CHAR) birthdayprivacy, CAST(postnum AS SIGNED) postnum, CAST(threadnum AS SIGNED) threadnum,
                    CAST(reputation AS SIGNED) reputation, CAST(warningpoints AS SIGNED) warningpoints,
                    COALESCE(INET6_NTOA(regip), '') regip, COALESCE(INET6_NTOA(lastip), '') lastip,
                    CAST(invisible AS SIGNED) invisible, CAST(hideemail AS SIGNED) hideemail, CAST(allownotices AS SIGNED) allownotices,
                    CAST(receivepms AS SIGNED) receivepms, CAST(pmnotify AS SIGNED) pmnotify, CAST(showsigs AS SIGNED) showsigs,
                    CAST(showavatars AS SIGNED) showavatars, CAST(showquickreply AS SIGNED) showquickreply,
                    CAST(showredirect AS SIGNED) showredirect, CAST(tpp AS SIGNED) tpp, CAST(ppp AS SIGNED) ppp,
                    CAST(referrer AS SIGNED) referrer, CAST(referrals AS SIGNED) referrals, CAST(usernotes AS CHAR) usernotes,
                    CAST(buddylist AS CHAR) buddylist, CAST(ignorelist AS CHAR) ignorelist,
                    CAST(timeonline AS SIGNED) timeonline, CAST(coppauser AS SIGNED) coppauser
             FROM {} ORDER BY uid",
            self.t("users")
        );
        self.copy("users", &sel, "users", "ON CONFLICT DO NOTHING", |o| {
            let salt = text(o, "salt");
            let hash = text(o, "password");
            o.remove("salt");
            o.insert(
                "password".into(),
                Value::from(if hash.is_empty() {
                    String::new()
                } else {
                    format!("mybb${salt}${hash}")
                }),
            );
            for k in ["additionalgroups", "buddylist", "ignorelist"] {
                let v = o.get(k).cloned().unwrap_or(Value::Null);
                o.insert(k.into(), int_list(&v));
            }
            // Local uploads move under /uploads/avatars; remote URLs stay as they are.
            let av = text(o, "avatar");
            let av = av.split('?').next().unwrap_or("").to_string();
            let av = if let Some(rest) = av
                .strip_prefix("./uploads/avatars/")
                .or_else(|| av.strip_prefix("uploads/avatars/"))
            {
                format!("/uploads/avatars/{rest}")
            } else if av.starts_with("images/") || av.starts_with("./images/") {
                String::new()
            } else {
                av
            };
            o.insert("avatar".into(), Value::from(av));
            if int(o, "displaygroup") == 0 {
                o.insert("displaygroup".into(), Value::from(0));
            }
            for k in ["birthdayprivacy"] {
                if text(o, k).is_empty() {
                    o.insert(k.into(), Value::from("all"));
                }
            }
            true
        })
        .await?;
        // MyBB's free-text moderator notes become each member's first note (see migration 0010).
        sqlx::query(
            "INSERT INTO moderator_notes (uid, author, note, created)
             SELECT u.uid, 0, u.usernotes, EXTRACT(EPOCH FROM now())::bigint FROM users u
             WHERE btrim(u.usernotes) <> '' AND NOT EXISTS (SELECT 1 FROM moderator_notes n WHERE n.uid = u.uid AND n.author = 0)",
        )
        .execute(&self.pg)
        .await
        .context("importing moderator notes")?;

        println!("importing forums…");
        let sel = format!(
            "SELECT CAST(fid AS SIGNED) fid, CAST(name AS CHAR) name, CAST(description AS CHAR) description, CAST(linkto AS CHAR) linkto,
                    CAST(type AS CHAR) type, CAST(pid AS SIGNED) pid, CAST(parentlist AS CHAR) parentlist, CAST(disporder AS SIGNED) disporder,
                    CAST(active AS SIGNED) active, CAST(open AS SIGNED) open, CAST(allowhtml AS SIGNED) allowhtml,
                    CAST(allowmycode AS SIGNED) allowmycode, CAST(allowsmilies AS SIGNED) allowsmilies, CAST(allowimgcode AS SIGNED) allowimgcode,
                    CAST(allowvideocode AS SIGNED) allowvideocode, CAST(allowpicons AS SIGNED) allowpicons,
                    CAST(allowtratings AS SIGNED) allowtratings, CAST(usepostcounts AS SIGNED) usepostcounts,
                    CAST(usethreadcounts AS SIGNED) usethreadcounts, CAST(requireprefix AS SIGNED) requireprefix,
                    CAST(password AS CHAR) password, CAST(showinjump AS SIGNED) showinjump, CAST(rulestype AS SIGNED) rulestype,
                    CAST(rulestitle AS CHAR) rulestitle, CAST(rules AS CHAR) rules, CAST(defaultdatecut AS SIGNED) defaultdatecut,
                    CAST(defaultsortby AS CHAR) defaultsortby, CAST(defaultsortorder AS CHAR) defaultsortorder
             FROM {} ORDER BY pid, fid",
            self.t("forums")
        );
        self.copy("forums", &sel, "forums", "", |o| {
            let v = o.get("parentlist").cloned().unwrap_or(Value::Null);
            o.insert("parentlist".into(), int_list(&v));
            true
        })
        .await?;

        let fsel = self
            .perm_select("forumpermissions", FORUM_PERM_META)
            .await?;
        let base = serde_json::to_value(crate::perms::ForumPerms::default())?;
        let sel = format!(
            "SELECT CAST(fid AS SIGNED) fid, CAST(gid AS SIGNED) gid{fsel} FROM {}",
            self.t("forumpermissions")
        );
        self.copy(
            "forum permissions",
            &sel,
            "forumpermissions",
            "ON CONFLICT DO NOTHING",
            |o| {
                let p = take_perms(o, FORUM_PERM_META, base.clone());
                o.insert("perms".into(), p);
                true
            },
        )
        .await?;

        let msel = self.perm_select("moderators", MOD_PERM_META).await?;
        let base = serde_json::to_value(crate::perms::ModPerms::default())?;
        let sel = format!(
            "SELECT CAST(fid AS SIGNED) fid, CAST(id AS SIGNED) id, CAST(isgroup AS SIGNED) isgroup{msel} FROM {}",
            self.t("moderators")
        );
        self.copy(
            "moderators",
            &sel,
            "moderators",
            "ON CONFLICT DO NOTHING",
            |o| {
                let p = take_perms(o, MOD_PERM_META, base.clone());
                o.insert("perms".into(), p);
                true
            },
        )
        .await?;

        let sel = format!(
            "SELECT CAST(pid AS SIGNED) pid, CAST(prefix AS CHAR) prefix, CAST(displaystyle AS CHAR) displaystyle,
                    CAST(forums AS CHAR) forums, CAST(`groups` AS CHAR) `groups` FROM {}",
            self.t("threadprefixes")
        );
        self.copy("thread prefixes", &sel, "threadprefixes", "", |o| {
            for k in ["forums", "groups"] {
                let v = o.get(k).cloned().unwrap_or(Value::Null);
                o.insert(
                    k.into(),
                    if v.as_str() == Some("-1") {
                        Value::from("{}")
                    } else {
                        int_list(&v)
                    },
                );
            }
            true
        })
        .await?;

        println!("importing threads…");
        let sel = format!(
            "SELECT CAST(tid AS SIGNED) tid, CAST(fid AS SIGNED) fid, CAST(subject AS CHAR) subject, CAST(prefix AS SIGNED) prefix,
                    CAST(poll AS SIGNED) poll, CAST(uid AS SIGNED) uid, CAST(username AS CHAR) username, CAST(dateline AS SIGNED) dateline,
                    CAST(firstpost AS SIGNED) firstpost, CAST(lastpost AS SIGNED) lastpost, CAST(lastposter AS CHAR) lastposter,
                    CAST(lastposteruid AS SIGNED) lastposteruid, CAST(views AS SIGNED) views, CAST(replies AS SIGNED) replies,
                    CAST(closed AS CHAR) closed, CAST(sticky AS SIGNED) sticky, CAST(numratings AS SIGNED) numratings,
                    CAST(totalratings AS SIGNED) totalratings, CAST(notes AS CHAR) notes, CAST(visible AS SIGNED) visible,
                    CAST(unapprovedposts AS SIGNED) unapprovedposts, CAST(deletedposts AS SIGNED) deletedposts,
                    CAST(attachmentcount AS SIGNED) attachmentcount, CAST(deletetime AS SIGNED) deletetime
             FROM {} ORDER BY tid",
            self.t("threads")
        );
        self.copy("threads", &sel, "threads", "", |o| {
            if text(o, "closed") == "0" {
                o.insert("closed".into(), Value::from(""));
            }
            true
        })
        .await?;

        println!("importing posts…");
        let sel = format!(
            "SELECT CAST(pid AS SIGNED) pid, CAST(tid AS SIGNED) tid, CAST(replyto AS SIGNED) replyto, CAST(fid AS SIGNED) fid,
                    CAST(subject AS CHAR) subject, CAST(uid AS SIGNED) uid, CAST(username AS CHAR) username,
                    CAST(dateline AS SIGNED) dateline, CAST(message AS CHAR) message, COALESCE(INET6_NTOA(ipaddress), '') ipaddress,
                    CAST(includesig AS SIGNED) includesig, CAST(smilieoff AS SIGNED) smilieoff, CAST(edituid AS SIGNED) edituid,
                    CAST(edittime AS SIGNED) edittime, CAST(editreason AS CHAR) editreason, CAST(visible AS SIGNED) visible
             FROM {} ORDER BY pid",
            self.t("posts")
        );
        self.copy("posts", &sel, "posts", "", |_| true).await?;

        let sel = format!(
            "SELECT CAST(pid AS SIGNED) pid, CAST(tid AS SIGNED) tid, CAST(question AS CHAR) question, CAST(dateline AS SIGNED) dateline,
                    CAST(options AS CHAR) options, CAST(votes AS CHAR) votes, CAST(numvotes AS SIGNED) numvotes,
                    CAST(timeout AS SIGNED) timeout, CAST(closed AS SIGNED) closed, CAST(multiple AS SIGNED) multiple,
                    CAST(public AS SIGNED) public, CAST(maxoptions AS SIGNED) maxoptions
             FROM {} ORDER BY pid",
            self.t("polls")
        );
        self.copy("polls", &sel, "polls", "ON CONFLICT DO NOTHING", |o| {
            let opts = text(o, "options");
            let opts: Vec<&str> = opts.split("||~|~||").collect();
            let votes = text(o, "votes");
            let mut votes: Vec<i64> = votes
                .split("||~|~||")
                .map(|v| v.trim().parse().unwrap_or(0))
                .collect();
            votes.resize(opts.len(), 0);
            o.insert("options".into(), text_array(&opts));
            o.insert("votes".into(), Value::from(votes));
            // MyBB stores the poll length in days relative to the poll's creation.
            let (timeout, dateline) = (int(o, "timeout"), int(o, "dateline"));
            o.insert(
                "timeout".into(),
                Value::from(if timeout > 0 && timeout < 100_000 {
                    dateline + timeout * 86400
                } else {
                    timeout
                }),
            );
            true
        })
        .await?;
        let sel = format!(
            "SELECT CAST(vid AS SIGNED) vid, CAST(pid AS SIGNED) pid, CAST(uid AS SIGNED) uid, CAST(voteoption AS SIGNED) voteoption,
                    CAST(dateline AS SIGNED) dateline FROM {} ORDER BY vid",
            self.t("pollvotes")
        );
        self.copy(
            "poll votes",
            &sel,
            "pollvotes",
            "ON CONFLICT DO NOTHING",
            |_| true,
        )
        .await?;

        println!("importing private messages…");
        let sel = format!(
            "SELECT CAST(pmid AS SIGNED) pmid, CAST(uid AS SIGNED) uid, CAST(toid AS SIGNED) toid, CAST(fromid AS SIGNED) fromid,
                    CAST(folder AS SIGNED) folder, CAST(subject AS CHAR) subject, CAST(message AS CHAR) message,
                    CAST(dateline AS SIGNED) dateline, CAST(deletetime AS SIGNED) deletetime, CAST(status AS SIGNED) status,
                    CAST(statustime AS SIGNED) statustime, CAST(includesig AS SIGNED) includesig, CAST(smilieoff AS SIGNED) smilieoff,
                    CAST(receipt AS SIGNED) receipt, CAST(readtime AS SIGNED) readtime, COALESCE(INET6_NTOA(ipaddress), '') ipaddress
             FROM {} ORDER BY pmid",
            self.t("privatemessages")
        );
        self.copy(
            "private messages",
            &sel,
            "privatemessages",
            "ON CONFLICT DO NOTHING",
            |o| {
                o.insert(
                    "recipients".into(),
                    serde_json::json!({ "to": [int(o, "toid")] }),
                );
                true
            },
        )
        .await?;

        let sel = format!(
            "SELECT CAST(aid AS SIGNED) aid, CAST(pid AS SIGNED) pid, CAST(posthash AS CHAR) posthash, CAST(uid AS SIGNED) uid,
                    CAST(filename AS CHAR) filename, CAST(filetype AS CHAR) filetype, CAST(filesize AS SIGNED) filesize,
                    CAST(attachname AS CHAR) attachname, CAST(downloads AS SIGNED) downloads, CAST(dateuploaded AS SIGNED) dateuploaded,
                    CAST(visible AS SIGNED) visible, CAST(thumbnail AS CHAR) thumbnail
             FROM {} ORDER BY aid",
            self.t("attachments")
        );
        let up = uploads_prefix.trim_matches('/').to_string();
        self.copy(
            "attachments",
            &sel,
            "attachments",
            "ON CONFLICT DO NOTHING",
            |o| {
                let name = format!("{up}/{}", text(o, "attachname"));
                let thumb = text(o, "thumbnail");
                let thumb = if thumb.is_empty() {
                    String::new()
                } else if thumb == "SMALL" {
                    name.clone()
                } else {
                    format!("{up}/{thumb}")
                };
                o.insert("attachname".into(), Value::from(name));
                o.insert("thumbnail".into(), Value::from(thumb));
                true
            },
        )
        .await?;

        for (label, table, target, sel) in [
            (
                "thread subscriptions",
                "threadsubscriptions",
                "threadsubscriptions",
                "SELECT CAST(sid AS SIGNED) sid, CAST(uid AS SIGNED) uid, CAST(tid AS SIGNED) tid, CAST(notification AS SIGNED) notification, CAST(dateline AS SIGNED) dateline FROM {}",
            ),
            (
                "forum subscriptions",
                "forumsubscriptions",
                "forumsubscriptions",
                "SELECT CAST(fsid AS SIGNED) fsid, CAST(fid AS SIGNED) fid, CAST(uid AS SIGNED) uid FROM {}",
            ),
            (
                "reputation",
                "reputation",
                "reputation",
                "SELECT CAST(rid AS SIGNED) rid, CAST(uid AS SIGNED) uid, CAST(adduid AS SIGNED) adduid, CAST(pid AS SIGNED) pid, CAST(reputation AS SIGNED) reputation, CAST(dateline AS SIGNED) dateline, CAST(comments AS CHAR) comments FROM {}",
            ),
        ] {
            if self.has_table(table).await? {
                let sel = sel.replace("{}", &self.t(table));
                self.copy(label, &sel, target, "ON CONFLICT DO NOTHING", |_| true)
                    .await?;
            }
        }

        println!("cleaning up orphans and resetting sequences…");
        for sql in [
            "DELETE FROM threads t WHERE NOT EXISTS (SELECT 1 FROM forums f WHERE f.fid = t.fid)",
            "UPDATE forums SET pid = 0 WHERE pid <> 0 AND pid NOT IN (SELECT fid FROM forums)",
            "DELETE FROM posts p WHERE NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = p.tid)",
            "DELETE FROM threads t WHERE NOT EXISTS (SELECT 1 FROM posts p WHERE p.pid = t.firstpost) AND t.closed NOT LIKE 'moved|%'",
            "UPDATE posts p SET fid = t.fid FROM threads t WHERE t.tid = p.tid AND p.fid <> t.fid",
            "DELETE FROM polls p WHERE NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = p.tid)",
            "DELETE FROM pollvotes v WHERE NOT EXISTS (SELECT 1 FROM polls p WHERE p.pid = v.pid)",
            "DELETE FROM attachments a WHERE a.pid <> 0 AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.pid = a.pid)",
            "DELETE FROM threadsubscriptions s WHERE NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = s.tid) OR NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = s.uid)",
            "DELETE FROM forumsubscriptions s WHERE NOT EXISTS (SELECT 1 FROM forums f WHERE f.fid = s.fid) OR NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = s.uid)",
            "DELETE FROM forumpermissions p WHERE NOT EXISTS (SELECT 1 FROM forums f WHERE f.fid = p.fid)",
            "DELETE FROM moderators m WHERE NOT EXISTS (SELECT 1 FROM forums f WHERE f.fid = m.fid)",
            "DELETE FROM privatemessages m WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = m.uid)",
            "DELETE FROM reputation r WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = r.uid)",
            "UPDATE users SET usergroup = 2 WHERE usergroup NOT IN (SELECT gid FROM usergroups)",
            "UPDATE users SET displaygroup = 0 WHERE displaygroup NOT IN (SELECT gid FROM usergroups)",
        ] {
            sqlx::query(sql).execute(&self.pg).await?;
        }
        let serials: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname::text, a.attname::text FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid
             WHERE c.relkind = 'r' AND c.relnamespace = 'public'::regnamespace AND pg_get_serial_sequence(c.relname::text, a.attname::text) IS NOT NULL",
        )
        .fetch_all(&self.pg)
        .await?;
        for (t, c) in serials {
            sqlx::query(&format!("SELECT setval(pg_get_serial_sequence('{t}', '{c}'), GREATEST(COALESCE((SELECT max(\"{c}\") FROM \"{t}\"), 0), 1))"))
                .execute(&self.pg)
                .await?;
        }
        Ok(())
    }
}
