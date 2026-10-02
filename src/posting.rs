//! Creating and editing posts/threads (the MyBB "PostDataHandler").
//!
//! Each operation is one unit of work: the post, thread and forum rows, counters, read markers,
//! subscriptions, attachments, edit history and moderator log commit together. Notifications,
//! plugin hooks and live updates are queued in the same transaction and happen after commit.
//! Writes to an existing thread lock it first (then its posts), which serializes replies, merges,
//! edits and moderation of the same thread.

use crate::app::LiveEvent;
use crate::ctx::CtxInner;
use crate::error::{AppError, AppResult};
use crate::infra::outbox::Job;
use crate::parser;
use crate::usecase::Uow;
use crate::util::now;

pub struct PostInput {
    pub subject: String,
    pub message: String,
    pub icon: i32,
    pub includesig: bool,
    pub smilieoff: bool,
    pub posthash: String,
    pub replyto: i32,
    /// Publish as the System account. Callers must check `canpostassystem` first.
    pub as_system: bool,
}

pub struct ThreadExtra {
    pub prefix: i32,
    pub sticky: bool,
    pub closed: bool,
    pub poll: Option<PollInput>,
}

pub struct PollInput {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
    pub public: bool,
    pub timeout_days: i64,
    pub maxoptions: i32,
}

/// Validate subject/message against board settings.
pub fn validate(ctx: &CtxInner, input: &PostInput, need_subject: bool) -> AppResult<()> {
    let s = ctx.settings();
    let subject = input.subject.trim();
    if need_subject && subject.is_empty() {
        return Err(AppError::user(
            "You did not enter a subject. Please enter one.",
        ));
    }
    let sublen = s.int("subjectlength").max(10) as usize;
    if subject.chars().count() > sublen {
        return Err(AppError::user(format!(
            "The subject is too long. Please enter a subject shorter than {sublen} characters."
        )));
    }
    let msg = input.message.trim();
    let len = if s.bool("mycodemessagelength") {
        msg.chars().count()
    } else {
        parser::to_plaintext(msg).chars().count()
    };
    let min = s.int("minmessagelength").max(1) as usize;
    let max = s.int("maxmessagelength") as usize;
    let is_mod_or_admin = ctx.can(crate::domain::staff::Cap::PostingExempt);
    if len < min && !is_mod_or_admin {
        return Err(AppError::user(format!(
            "The message is too short. Please enter a message longer than {min} characters."
        )));
    }
    if msg.is_empty() {
        return Err(AppError::user(
            "You did not enter a message. Please enter one.",
        ));
    }
    if max > 0 && len > max && !is_mod_or_admin {
        return Err(AppError::user(format!(
            "The message is too long. Please enter a message shorter than {max} characters."
        )));
    }
    let maximg = s.int("maxpostimages") as usize;
    if maximg > 0 && parser::count_images(msg) > maximg && !is_mod_or_admin {
        return Err(AppError::user(format!(
            "You have posted too many images. Maximum per post: {maximg}."
        )));
    }
    let maxvid = s.int("maxpostvideos") as usize;
    if maxvid > 0 && parser::count_videos(msg) > maxvid && !is_mod_or_admin {
        return Err(AppError::user(format!(
            "You have posted too many videos. Maximum per post: {maxvid}."
        )));
    }
    Ok(())
}

/// Flood control + daily post limit + suspension checks.
pub async fn check_posting_allowed(ctx: &CtxInner) -> AppResult<()> {
    let s = ctx.settings();
    if let Some(u) = &ctx.user {
        if u.suspendposting && (u.suspensiontime == 0 || u.suspensiontime > now()) {
            return Err(AppError::user(
                "Your posting privileges are currently suspended.",
            ));
        }
        let exempt = ctx.can(crate::domain::staff::Cap::PostingExempt);
        if !exempt && s.bool("postfloodcheck") {
            let secs = s.int("postfloodsecs");
            let wait = u.lastpost + secs - now();
            if secs > 0 && wait > 0 {
                return Err(AppError::user(format!(
                    "You are trying to post too fast. Please wait {wait} more second{}.",
                    if wait == 1 { "" } else { "s" }
                )));
            }
        }
        if !exempt && ctx.perms.maxposts > 0 {
            let n: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE uid = $1 AND dateline > $2")
                    .bind(u.uid)
                    .bind(now() - 86400)
                    .fetch_one(&ctx.app.db)
                    .await?;
            if n >= ctx.perms.maxposts as i64 {
                return Err(AppError::user(format!(
                    "You have reached your maximum of {} posts per day. Please try again later.",
                    ctx.perms.maxposts
                )));
            }
        }
    } else {
        // Guests: per-IP flood control.
        let secs = s.int("postfloodsecs").max(10);
        if !ctx
            .app
            .throttle(&format!("guestpost:{}", ctx.ip), 1, secs)
            .await
        {
            return Err(AppError::user(
                "You are trying to post too fast. Please wait a moment.",
            ));
        }
    }
    Ok(())
}

fn needs_moderation(ctx: &CtxInner, fid: i32, thread: bool) -> bool {
    if ctx.is_mod(fid) {
        return false;
    }
    if let Some(u) = &ctx.user
        && u.moderateposts
        && (u.moderationtime == 0 || u.moderationtime > now())
    {
        return true;
    }
    let fp = ctx.forum_perms(fid);
    if thread { fp.modthreads } else { fp.modposts }
}

/// Create a new thread with its first post. Returns (tid, pid, visible).
pub async fn create_thread(
    ctx: &CtxInner,
    fid: i32,
    input: &PostInput,
    extra: &ThreadExtra,
) -> AppResult<(i32, i32, i16)> {
    let app = &ctx.app;
    let actor = ctx.uid();
    let (uid, username) = if input.as_system {
        system_author(ctx).await?
    } else if actor > 0 {
        (actor, ctx.username().to_string())
    } else {
        (0, input_guest_name(input))
    };
    // Content published as System carries no IP; the staff member's is kept in its authorship record.
    let ip = if input.as_system { "" } else { ctx.ip.as_str() };
    let visible: i16 = if needs_moderation(ctx, fid, true) {
        0
    } else {
        1
    };
    let t = now();
    let subject = input.subject.trim().to_string();
    let mut uow = Uow::begin(app).await?;
    let tx = uow.conn();
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, prefix, icon, uid, username, dateline, lastpost, lastposter, lastposteruid, visible, sticky, closed)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $6, $5, $8, $9, $10) RETURNING tid",
    )
    .bind(fid)
    .bind(&subject)
    .bind(extra.prefix)
    .bind(input.icon)
    .bind(uid)
    .bind(&username)
    .bind(t)
    .bind(visible)
    .bind(extra.sticky)
    .bind(if extra.closed { "1" } else { "" })
    .fetch_one(&mut *tx)
    .await?;
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, fid, subject, icon, uid, username, dateline, message, ipaddress, includesig, smilieoff, visible)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING pid",
    )
    .bind(tid)
    .bind(fid)
    .bind(&subject)
    .bind(input.icon)
    .bind(uid)
    .bind(&username)
    .bind(t)
    .bind(input.message.trim())
    .bind(crate::util::IpText(ip.to_string()))
    .bind(input.includesig)
    .bind(input.smilieoff)
    .bind(visible)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE threads SET firstpost = $2 WHERE tid = $1")
        .bind(tid)
        .bind(pid)
        .execute(&mut *tx)
        .await?;
    if let Some(p) = &extra.poll {
        let votes = vec![0i32; p.options.len()];
        let poll_id: i32 = sqlx::query_scalar(
            "INSERT INTO polls (tid, question, dateline, options, votes, timeout, multiple, public, maxoptions)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING pid",
        )
        .bind(tid)
        .bind(&p.question)
        .bind(t)
        .bind(&p.options)
        .bind(&votes)
        .bind(if p.timeout_days > 0 { t + p.timeout_days * 86400 } else { 0 })
        .bind(p.multiple)
        .bind(p.public)
        .bind(p.maxoptions)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE threads SET poll = $2 WHERE tid = $1")
            .bind(tid)
            .bind(poll_id)
            .execute(&mut *tx)
            .await?;
    }
    attach_uploads(tx, ctx, fid, &input.posthash, pid, actor, uid).await?;
    if input.as_system {
        record_system_authorship(tx, ctx, "thread", tid, &subject).await?;
    }
    let forum = ctx.cache.forum(fid);
    if visible == 1 {
        sqlx::query(
            "UPDATE forums SET threads = threads + 1, posts = posts + 1, lastpost = $2, lastposter = $3, lastposteruid = $4,
                lastposttid = $5, lastpostsubject = $6 WHERE fid = $1",
        )
        .bind(fid)
        .bind(t)
        .bind(&username)
        .bind(uid)
        .bind(tid)
        .bind(&subject)
        .execute(&mut *tx)
        .await?;
        if uid > 0 {
            let pc = forum.map(|f| f.usepostcounts).unwrap_or(true) as i32;
            let tc = forum.map(|f| f.usethreadcounts).unwrap_or(true) as i32;
            sqlx::query("UPDATE users SET postnum = postnum + $2, threadnum = threadnum + $3, lastpost = $4 WHERE uid = $1")
                .bind(uid)
                .bind(pc)
                .bind(tc)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
    } else {
        sqlx::query("UPDATE forums SET unapprovedthreads = unapprovedthreads + 1, unapprovedposts = unapprovedposts + 1 WHERE fid = $1")
            .bind(fid)
            .execute(&mut *tx)
            .await?;
        if uid > 0 {
            sqlx::query("UPDATE users SET lastpost = $2 WHERE uid = $1")
                .bind(uid)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
    }
    if actor > 0 {
        sqlx::query("INSERT INTO threadsread (tid, uid, dateline) VALUES ($1, $2, $3) ON CONFLICT (uid, tid) DO UPDATE SET dateline = $3")
            .bind(tid)
            .bind(actor)
            .bind(t)
            .execute(&mut *tx)
            .await?;
        auto_subscribe(tx, ctx, tid).await?;
    }
    if visible == 1 {
        uow.job_once(
            Job::PostNotifications {
                new_thread: true,
                fid,
                tid,
                pid,
                uid,
                username: username.clone(),
                subject: subject.clone(),
                message: input.message.clone(),
            },
            format!("notify:post:{pid}"),
        );
        uow.hook(
            "thread_created",
            serde_json::json!({"tid": tid, "pid": pid, "fid": fid, "uid": uid}),
        );
    }
    uow.page_tags(crate::pagecache::forum_tags(&ctx.cache, fid));
    uow.commit(app).await?;
    ctx.app.mod_counts.invalidate_all();
    Ok((tid, pid, visible))
}

/// The System account as author, for a signed-in member allowed to post as System.
async fn system_author(ctx: &CtxInner) -> AppResult<(i32, String)> {
    if ctx.uid() == 0 || !ctx.perms.canpostassystem {
        return Err(AppError::no_perm());
    }
    crate::system::identity(&ctx.app).await
}

async fn record_system_authorship(
    tx: &mut sqlx::PgConnection,
    ctx: &CtxInner,
    kind: &str,
    ref_id: i32,
    summary: &str,
) -> AppResult<()> {
    crate::system::record(
        tx,
        crate::system::Authorship {
            kind,
            ref_id,
            actor: ctx.uid(),
            actor_name: ctx.username(),
            ip: &ctx.ip,
            summary,
        },
    )
    .await
}

fn input_guest_name(input: &PostInput) -> String {
    let _ = input;
    "Guest".to_string()
}

/// Create a reply. Returns (pid, visible, merged_into_previous).
pub async fn create_reply(
    ctx: &CtxInner,
    tid: i32,
    fid: i32,
    input: &PostInput,
    guest_name: Option<&str>,
) -> AppResult<(i32, i16, bool)> {
    let mut uow = Uow::begin(&ctx.app).await?;
    let r = create_reply_in(&mut uow, ctx, tid, fid, input, guest_name).await?;
    uow.commit(&ctx.app).await?;
    ctx.app.mod_counts.invalidate_all();
    Ok(r)
}

/// `create_reply` inside a caller's unit of work.
pub async fn create_reply_in(
    uow: &mut Uow,
    ctx: &CtxInner,
    tid: i32,
    fid: i32,
    input: &PostInput,
    guest_name: Option<&str>,
) -> AppResult<(i32, i16, bool)> {
    let actor = ctx.uid();
    let (uid, username) = if input.as_system {
        system_author(ctx).await?
    } else if actor > 0 {
        (actor, ctx.username().to_string())
    } else {
        (
            0,
            guest_name
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("Guest")
                .chars()
                .take(30)
                .collect(),
        )
    };
    let ip = if input.as_system { "" } else { ctx.ip.as_str() };
    let visible: i16 = if needs_moderation(ctx, fid, false) {
        0
    } else {
        1
    };
    let t = now();

    // Serialize writes to this thread (replies, merges, edits, moderation) and keep counters right.
    let tvis: i16 = sqlx::query_scalar("SELECT visible FROM threads WHERE tid = $1 FOR UPDATE")
        .bind(tid)
        .fetch_optional(uow.conn())
        .await?
        .ok_or_else(|| AppError::not_found("thread"))?;

    // Automatic post merge (double posting) — MyBB's "postmergemins". Decided under the thread
    // lock, with the last post locked too, so two quick replies cannot both merge or both miss.
    let mergemins = ctx.settings().int("postmergemins");
    if uid > 0 && !input.as_system && mergemins > 0 && visible == 1 && input.posthash.is_empty() {
        let last: Option<(i32, i32, i64, String)> = sqlx::query_as(
            "SELECT pid, uid, dateline, message FROM posts WHERE tid = $1 AND visible = 1
             ORDER BY dateline DESC, pid DESC LIMIT 1 FOR UPDATE",
        )
        .bind(tid)
        .fetch_optional(uow.conn())
        .await?;
        if let Some((lpid, luid, ldate, lmsg)) = last
            && luid == uid
            && t - ldate < mergemins * 60
        {
            let merged = format!("{lmsg}\n[hr]\n{}", input.message.trim());
            if ctx.settings().bool("keepedithistory") {
                record_edit(uow.conn(), lpid, uid, t, None, &lmsg, "Automatic merge").await?;
            }
            sqlx::query(
                "UPDATE posts SET message = $2, parser_rev = -1, edittime = $3 WHERE pid = $1",
            )
            .bind(lpid)
            .bind(&merged)
            .bind(t)
            .execute(uow.conn())
            .await?;
            sqlx::query("UPDATE users SET lastpost = $2 WHERE uid = $1")
                .bind(uid)
                .bind(t)
                .execute(uow.conn())
                .await?;
            uow.live(LiveEvent {
                kind: "editpost",
                tid,
                uid: 0,
                data: serde_json::json!({"pid": lpid}),
            });
            uow.page_tags(crate::pagecache::post_tags(&ctx.cache, fid, tid));
            return Ok((lpid, 1, true));
        }
    }

    let tx = uow.conn();
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, replyto, fid, subject, icon, uid, username, dateline, message, ipaddress, includesig, smilieoff, visible)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING pid",
    )
    .bind(tid)
    .bind(input.replyto)
    .bind(fid)
    .bind(input.subject.trim())
    .bind(input.icon)
    .bind(uid)
    .bind(&username)
    .bind(t)
    .bind(input.message.trim())
    .bind(crate::util::IpText(ip.to_string()))
    .bind(input.includesig)
    .bind(input.smilieoff)
    .bind(visible)
    .fetch_one(&mut *tx)
    .await?;
    let nattach = attach_uploads(tx, ctx, fid, &input.posthash, pid, actor, uid).await?;
    if input.as_system {
        record_system_authorship(tx, ctx, "post", pid, input.subject.trim()).await?;
    }
    let subject: String = sqlx::query_scalar("SELECT subject FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(&mut *tx)
        .await?;
    if visible == 1 {
        sqlx::query(
            "UPDATE threads SET replies = replies + 1, lastpost = $2, lastposter = $3, lastposteruid = $4,
                attachmentcount = attachmentcount + $5 WHERE tid = $1",
        )
        .bind(tid)
        .bind(t)
        .bind(&username)
        .bind(uid)
        .bind(nattach as i32)
        .execute(&mut *tx)
        .await?;
        if tvis == 1 {
            sqlx::query(
                "UPDATE forums SET posts = posts + 1, lastpost = $2, lastposter = $3, lastposteruid = $4, lastposttid = $5, lastpostsubject = $6
                 WHERE fid = $1",
            )
            .bind(fid)
            .bind(t)
            .bind(&username)
            .bind(uid)
            .bind(tid)
            .bind(&subject)
            .execute(&mut *tx)
            .await?;
        } else {
            let col = if tvis == 0 {
                "unapprovedposts"
            } else {
                "deletedposts"
            };
            sqlx::query(&format!(
                "UPDATE forums SET {col} = {col} + 1 WHERE fid = $1"
            ))
            .bind(fid)
            .execute(&mut *tx)
            .await?;
        }
        if uid > 0 {
            let pc = ctx
                .cache
                .forum(fid)
                .map(|f| f.usepostcounts)
                .unwrap_or(true)
                && tvis == 1;
            sqlx::query("UPDATE users SET postnum = postnum + $2, lastpost = $3 WHERE uid = $1")
                .bind(uid)
                .bind(pc as i32)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
    } else {
        sqlx::query("UPDATE threads SET unapprovedposts = unapprovedposts + 1 WHERE tid = $1")
            .bind(tid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE forums SET unapprovedposts = unapprovedposts + 1 WHERE fid = $1")
            .bind(fid)
            .execute(&mut *tx)
            .await?;
        if uid > 0 {
            sqlx::query("UPDATE users SET lastpost = $2 WHERE uid = $1")
                .bind(uid)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
    }
    if actor > 0 {
        sqlx::query("INSERT INTO threadsread (tid, uid, dateline) VALUES ($1, $2, $3) ON CONFLICT (uid, tid) DO UPDATE SET dateline = $3")
            .bind(tid)
            .bind(actor)
            .bind(t)
            .execute(&mut *tx)
            .await?;
        auto_subscribe(tx, ctx, tid).await?;
    }
    if visible == 1 && tvis == 1 {
        uow.job_once(
            Job::PostNotifications {
                new_thread: false,
                fid,
                tid,
                pid,
                uid,
                username: username.clone(),
                subject,
                message: input.message.clone(),
            },
            format!("notify:post:{pid}"),
        );
        uow.live(LiveEvent {
            kind: "newpost",
            tid,
            uid: 0,
            data: serde_json::json!({"pid": pid, "username": username, "uid": uid}),
        });
        uow.hook(
            "post_created",
            serde_json::json!({"tid": tid, "pid": pid, "fid": fid, "uid": uid}),
        );
    }
    uow.page_tags(crate::pagecache::post_tags(&ctx.cache, fid, tid));
    Ok((pid, visible, false))
}

async fn auto_subscribe(tx: &mut sqlx::PgConnection, ctx: &CtxInner, tid: i32) -> AppResult<()> {
    let Some(u) = &ctx.user else { return Ok(()) };
    if u.subscriptionmethod == 0 {
        return Ok(());
    }
    let notification: i16 = match u.subscriptionmethod {
        2 => 1,
        3 => 2,
        _ => 0,
    };
    sqlx::query(
        "INSERT INTO threadsubscriptions (uid, tid, notification, dateline) VALUES ($1, $2, $3, $4) ON CONFLICT (uid, tid) DO NOTHING",
    )
    .bind(u.uid)
    .bind(tid)
    .bind(notification)
    .bind(now())
    .execute(tx)
    .await?;
    Ok(())
}

/// Keep the previous text of a post in its edit history.
async fn record_edit(
    tx: &mut sqlx::PgConnection,
    pid: i32,
    editor: i32,
    t: i64,
    old_subject: Option<&str>,
    old_message: &str,
    reason: &str,
) -> AppResult<()> {
    let subject = match old_subject {
        Some(s) => s.to_string(),
        None => {
            sqlx::query_scalar("SELECT subject FROM posts WHERE pid = $1")
                .bind(pid)
                .fetch_one(&mut *tx)
                .await?
        }
    };
    sqlx::query("INSERT INTO post_edits (pid, uid, dateline, subject, message, reason) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(pid)
        .bind(editor)
        .bind(t)
        .bind(subject)
        .bind(old_message)
        .bind(reason)
        .execute(tx)
        .await?;
    Ok(())
}

/// Claim attachments uploaded with a posthash for the newly created post.
///
/// Uploads don't know their final forum for certain (the client says which forum it is posting
/// to), so the forum's attachment permission and per-forum file type restrictions are enforced
/// here, when the files are bound to a post. Files that aren't allowed stay unclaimed and are
/// pruned with other orphaned uploads.
async fn attach_uploads(
    tx: &mut sqlx::PgConnection,
    ctx: &CtxInner,
    fid: i32,
    posthash: &str,
    pid: i32,
    uploader: i32,
    owner: i32,
) -> AppResult<u64> {
    if posthash.is_empty() || uploader == 0 {
        return Ok(0);
    }
    if !ctx.settings().bool("enableattachments")
        || !ctx.perms.canpostattachments
        || !ctx.forum_perms(fid).canpostattachments
    {
        return Ok(0);
    }
    let parents = ctx
        .cache
        .forum(fid)
        .map(|f| f.parentlist.clone())
        .unwrap_or_default();
    let files: Vec<(i32, String)> = sqlx::query_as(
        "SELECT aid, filename FROM attachments WHERE posthash = $1 AND uid = $2 AND pid = 0",
    )
    .bind(posthash)
    .bind(uploader)
    .fetch_all(&mut *tx)
    .await?;
    let ok: Vec<i32> = files
        .into_iter()
        .filter(|(_, name)| {
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            ctx.cache.attachtypes.iter().any(|a| {
                a.enabled
                    && a.extension.eq_ignore_ascii_case(&ext)
                    && (a.groups.is_empty() || a.groups.iter().any(|g| ctx.groups.contains(g)))
                    && (a.forums.is_empty() || a.forums.iter().any(|f| parents.contains(f)))
            })
        })
        .map(|(aid, _)| aid)
        .collect();
    let r = sqlx::query(
        "UPDATE attachments SET pid = $1, posthash = '', uid = $3 WHERE aid = ANY($2) AND pid = 0",
    )
    .bind(pid)
    .bind(&ok)
    .bind(owner)
    .execute(&mut *tx)
    .await?;
    Ok(r.rows_affected())
}

/// What an edit changes.
pub struct PostEdit<'a> {
    pub subject: &'a str,
    pub message: &'a str,
    pub reason: &'a str,
    pub icon: i32,
    pub includesig: bool,
    pub smilieoff: bool,
    /// Moderators may edit without the "edited" note.
    pub silent: bool,
    /// New thread prefix (first post only), already checked against the allowed prefixes.
    pub prefix: Option<i32>,
    /// Record the edit in the moderator log (a moderator editing someone else's post).
    pub log_as_moderator: bool,
}

/// Edit a post: the edit history entry, the post itself, the thread subject and prefix, the
/// forum's last-post subject, the move to moderation (if edits are moderated here) and the
/// moderator log commit together, with the thread and post locked. Returns true if the edit was
/// sent to moderation.
pub async fn edit_post(ctx: &CtxInner, pid: i32, e: &PostEdit<'_>) -> AppResult<bool> {
    let app = &ctx.app;
    let mut uow = Uow::begin(app).await?;
    if crate::ops::lock_posts(uow.conn(), &[pid]).await?.is_empty() {
        return Err(AppError::not_found("post"));
    }
    let (tid, fid, old_subject, old_message): (i32, i32, String, String) =
        sqlx::query_as("SELECT tid, fid, subject, message FROM posts WHERE pid = $1")
            .bind(pid)
            .fetch_one(uow.conn())
            .await?;
    let (subject, message) = (e.subject.trim(), e.message.trim());
    let t = now();
    if ctx.settings().bool("keepedithistory") && (old_message != message || old_subject != subject)
    {
        record_edit(
            uow.conn(),
            pid,
            ctx.uid(),
            t,
            Some(&old_subject),
            &old_message,
            e.reason,
        )
        .await?;
    }
    let (edituid, edittime) = if e.silent { (0, 0) } else { (ctx.uid(), t) };
    sqlx::query(
        "UPDATE posts SET subject = $2, message = $3, icon = $4, includesig = $5, smilieoff = $6, parser_rev = -1,
            edituid = CASE WHEN $10 THEN edituid ELSE $7 END,
            edittime = CASE WHEN $10 THEN edittime ELSE $8 END,
            editreason = CASE WHEN $10 THEN editreason ELSE $9 END
         WHERE pid = $1",
    )
    .bind(pid)
    .bind(subject)
    .bind(message)
    .bind(e.icon)
    .bind(e.includesig)
    .bind(e.smilieoff)
    .bind(edituid)
    .bind(edittime)
    .bind(e.reason)
    .bind(e.silent)
    .execute(uow.conn())
    .await?;
    let firstpost: i32 = sqlx::query_scalar("SELECT firstpost FROM threads WHERE tid = $1")
        .bind(tid)
        .fetch_one(uow.conn())
        .await?;
    if firstpost == pid {
        // Keep the thread subject (and forum last-post subjects) in sync with the first post.
        if !subject.is_empty() && subject != old_subject {
            sqlx::query("UPDATE threads SET subject = $2 WHERE tid = $1")
                .bind(tid)
                .bind(subject)
                .execute(uow.conn())
                .await?;
            sqlx::query("UPDATE forums SET lastpostsubject = $2 WHERE lastposttid = $1")
                .bind(tid)
                .bind(subject)
                .execute(uow.conn())
                .await?;
        }
        if let Some(prefix) = e.prefix {
            sqlx::query("UPDATE threads SET prefix = $2 WHERE tid = $1")
                .bind(tid)
                .bind(prefix)
                .execute(uow.conn())
                .await?;
        }
    }
    let moderate = !ctx.is_mod(fid) && ctx.forum_perms(fid).mod_edit_posts;
    if moderate {
        crate::ops::set_posts_visibility_in(&mut uow, &[pid], 0).await?;
    }
    if e.log_as_moderator {
        crate::ops::log_moderator_action_in(
            uow.conn(),
            ctx.uid(),
            &ctx.ip,
            fid,
            tid,
            pid,
            "Edited post",
            serde_json::json!({}),
        )
        .await?;
    }
    uow.live(LiveEvent {
        kind: "editpost",
        tid,
        uid: 0,
        data: serde_json::json!({"pid": pid}),
    });
    uow.page_tags(crate::pagecache::post_tags(&ctx.cache, fid, tid));
    uow.commit(app).await?;
    if moderate {
        app.mod_counts.invalidate_all();
    }
    Ok(moderate)
}
