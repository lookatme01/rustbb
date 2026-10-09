//! Attachment upload (AJAX, with thumbnails and quotas) and permission-checked downloads.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::{POST_COLUMNS, Post};
use crate::util::{self, now};
use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

fn json_err(msg: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

async fn upload_limit_error(
    ctx: &Ctx,
    pid: i32,
    posthash: &str,
    size: i64,
    conn: &mut sqlx::PgConnection,
) -> AppResult<Option<Response>> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE (posthash = $1 AND posthash <> '' AND uid = $3) OR (pid = $2 AND pid > 0)")
        .bind(posthash)
        .bind(pid)
        .bind(ctx.uid())
        .fetch_one(&mut *conn)
        .await?;
    let max = ctx.settings().int("maxattachments");
    if max > 0 && count >= max && !ctx.can(crate::domain::staff::Cap::PostingExempt) {
        return Ok(Some(json_err(&format!(
            "You can attach at most {max} files to a post."
        ))));
    }
    if ctx.perms.attachquota > 0 {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(filesize), 0)::bigint FROM attachments WHERE uid = $1",
        )
        .bind(ctx.uid())
        .fetch_one(&mut *conn)
        .await?;
        if used + size > ctx.perms.attachquota as i64 * 1024 {
            return Ok(Some(json_err(
                "Uploading this file would exceed your attachment quota. Delete some attachments in the User CP first.",
            )));
        }
    }
    Ok(None)
}

/// The largest attachment any enabled type allows, capped by `RBB_MAX_UPLOAD_MB`.
fn upload_ceiling(ctx: &Ctx) -> u64 {
    let cap = ctx.app.cfg.max_upload_mb * 1024 * 1024;
    let types = &ctx.cache.attachtypes;
    if types.iter().any(|a| a.enabled && a.maxsize <= 0) {
        return cap;
    }
    types
        .iter()
        .filter(|a| a.enabled)
        .map(|a| a.maxsize as u64 * 1024)
        .max()
        .unwrap_or(0)
        .min(cap)
        .max(1)
}

pub async fn upload(ctx: Ctx, mut mp: Multipart) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    let s = ctx.settings().clone();
    if !s.bool("enableattachments") || !ctx.perms.canpostattachments {
        return Ok(json_err("You are not allowed to upload attachments."));
    }
    let mut posthash = String::new();
    let mut pid = 0i32;
    let mut fid = 0i32;
    let mut key = String::new();
    // The file streams to a temporary file, cut off at the largest size any enabled type allows
    // (the limit for its own type is checked once the form's other fields are known).
    let limit = if ctx.perms.attachquota > 0 {
        upload_ceiling(&ctx).min(ctx.perms.attachquota as u64 * 1024)
    } else {
        upload_ceiling(&ctx)
    };
    let mut file: Option<crate::infra::uploads::Spooled> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::user(format!("Upload failed: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                match crate::infra::uploads::spool(&ctx.app.cfg.upload_dir, field, limit).await {
                    Ok(f) => file = Some(f),
                    Err(crate::infra::uploads::SpoolError::Io(e)) => {
                        return Err(AppError::Other(e.into()));
                    }
                    Err(e) => return Ok(json_err(&e.to_string())),
                }
            }
            "posthash" => posthash = field.text().await.unwrap_or_default(),
            "pid" => pid = field.text().await.unwrap_or_default().parse().unwrap_or(0),
            "fid" => fid = field.text().await.unwrap_or_default().parse().unwrap_or(0),
            "my_post_key" => key = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }
    ctx.check_csrf(&key)?;
    let upload = file.ok_or_else(|| AppError::user("No file was uploaded."))?;
    let filename = upload.file_name.clone();
    let size = upload.size as i64;
    // Editing an existing post: must be allowed to edit it.
    if pid > 0 {
        let post: Post =
            sqlx::query_as(&format!("SELECT {POST_COLUMNS} FROM posts WHERE pid = $1"))
                .bind(pid)
                .fetch_optional(&ctx.app.db)
                .await?
                .ok_or_else(|| AppError::not_found("post"))?;
        let (thread, _, fp) = super::showthread::check_thread(&ctx, post.tid).await?;
        let mp = ctx.mod_perms(post.fid);
        if !super::showthread::edit_allowed(&ctx, &post, &thread, &fp, &mp) {
            return Ok(json_err("You cannot add attachments to this post."));
        }
        fid = post.fid;
        posthash.clear();
    } else if posthash.len() < 16 || !posthash.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(json_err("Invalid upload session. Please reload the page."));
    }
    if fid > 0 {
        ctx.check_forum(fid)?;
    }
    if fid > 0 && !ctx.forum_perms(fid).canpostattachments {
        return Ok(json_err("You cannot post attachments in this forum."));
    }
    let ext = filename
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let cache = ctx.cache.clone();
    let Some(at) = cache
        .attachtypes
        .iter()
        .find(|a| a.enabled && a.extension.eq_ignore_ascii_case(&ext))
    else {
        return Ok(json_err(&format!("Files of type .{ext} are not allowed.")));
    };
    if !at.groups.is_empty() && !at.groups.iter().any(|g| ctx.groups.contains(g)) {
        return Ok(json_err(&format!(
            "You are not allowed to upload .{ext} files."
        )));
    }
    if fid > 0 && !at.forums.is_empty() {
        let parents = cache
            .forum(fid)
            .map(|f| f.parentlist.clone())
            .unwrap_or_default();
        if !at.forums.iter().any(|f| parents.contains(f)) {
            return Ok(json_err(&format!(
                "Files of type .{ext} are not allowed in this forum."
            )));
        }
    }
    if at.maxsize > 0 && size > at.maxsize as i64 * 1024 {
        return Ok(json_err(&format!(
            "The file is too large. The maximum size for .{ext} files is {}.",
            util::format_bytes(at.maxsize as i64 * 1024)
        )));
    }
    // Reject exhausted quotas before image decoding without holding a connection
    // through CPU work. The authoritative check is repeated under the locks below.
    let preflight = {
        let mut conn = ctx.app.db.acquire().await?;
        upload_limit_error(&ctx, pid, &posthash, size, &mut conn).await?
    };
    if let Some(error) = preflight {
        return Ok(error);
    }
    let is_image = at.mimetype.starts_with("image/");
    // Images must decode (within limits); thumbnails are made for large ones.
    let (tw, th) = (
        s.int("attachthumbw").max(32) as u32,
        s.int("attachthumbh").max(32) as u32,
    );
    let thumb: Option<Vec<u8>> = if is_image {
        let r = crate::infra::uploads::with_image(upload.path().to_path_buf(), move |img| {
            if img.width() <= tw && img.height() <= th {
                return Ok(None);
            }
            crate::infra::uploads::png(&img.thumbnail(tw, th)).map(Some)
        })
        .await;
        match r {
            Ok(t) => t,
            Err(e) => return Ok(json_err(&e)),
        }
    } else {
        None
    };
    // Serialize quota checks through insertion on every node. Separate namespaces
    // distinguish account quotas from the attachment count shared by a post.
    // Always lock the account before the post, and bound how long a rival upload waits.
    let mut tx = ctx.app.db.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *tx)
        .await?;
    let locks = [(0x52424241_i32, me.uid), (0x52424250_i32, pid)];
    for (namespace, id) in locks.into_iter().take(if pid > 0 { 2 } else { 1 }) {
        if let Err(e) = sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
        {
            if e.as_database_error()
                .is_some_and(|e| e.code().as_deref() == Some("55P03"))
            {
                return Ok(json_err(
                    "Another attachment upload is in progress. Please try again.",
                ));
            }
            return Err(e.into());
        }
    }
    if let Some(error) = upload_limit_error(&ctx, pid, &posthash, size, &mut tx).await? {
        return Ok(error);
    }
    let month = chrono::Utc::now().format("%Y%m").to_string();
    let stem = format!("{}_{}", me.uid, util::random_token(24));
    let attachname = format!("attachments/{month}/{stem}.attach");
    ctx.app
        .storage
        .put_file(&attachname, upload.path())
        .await
        .map_err(AppError::Other)?;
    let thumbname = match thumb {
        Some(t) => {
            let n = format!("attachments/{month}/{stem}_thumb.png");
            ctx.app
                .storage
                .put_bytes(&n, t.into())
                .await
                .map_err(AppError::Other)?;
            n
        }
        None if is_image => attachname.clone(),
        None => String::new(),
    };
    // Moderators of this forum (or of every forum) skip attachment moderation; moderating some
    // other forum is no exemption.
    let visible = ctx.is_mod(fid)
        || !((fid > 0 && ctx.forum_perms(fid).modattachments) || ctx.perms.modattachments);
    let aid: i32 = sqlx::query_scalar(
        "INSERT INTO attachments (pid, posthash, uid, filename, filetype, filesize, attachname, dateuploaded, visible, thumbnail) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING aid",
    )
    .bind(pid)
    .bind(&posthash)
    .bind(me.uid)
    .bind(&filename)
    .bind(&at.mimetype)
    .bind(size)
    .bind(&attachname)
    .bind(now())
    .bind(visible)
    .bind(if thumbname == attachname { String::new() } else { thumbname })
    .fetch_one(&mut *tx)
    .await?;
    if pid > 0 {
        sqlx::query("UPDATE posts SET parser_rev = -1 WHERE pid = $1")
            .bind(pid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({"aid": aid, "filename": filename, "size": util::format_bytes(size), "visible": visible, "is_image": is_image})).into_response())
}

#[derive(Deserialize, Default)]
pub struct DlQuery {
    #[serde(default)]
    pub thumb: Option<String>,
}

pub async fn download(
    ctx: Ctx,
    Path(aid): Path<i32>,
    Query(q): Query<DlQuery>,
) -> AppResult<Response> {
    let row: Option<(i32, i32, String, String, i64, String, String, bool)> =
        sqlx::query_as("SELECT pid, uid, filename, filetype, filesize, attachname, thumbnail, visible FROM attachments WHERE aid = $1").bind(aid).fetch_optional(&ctx.app.db).await?;
    let (pid, uid, filename, filetype, size, attachname, thumbnail, visible) =
        row.ok_or_else(|| AppError::not_found("attachment"))?;
    let thumb = q.thumb.is_some();
    if pid == 0 {
        if uid != ctx.uid() || uid == 0 {
            return Err(AppError::no_perm());
        }
    } else {
        let (tid, fid, post_uid, post_visible): (i32, i32, i32, i16) =
            sqlx::query_as("SELECT tid, fid, uid, visible FROM posts WHERE pid = $1")
                .bind(pid)
                .fetch_optional(&ctx.app.db)
                .await?
                .ok_or_else(|| AppError::not_found("attachment"))?;
        crate::routes::showthread::check_thread(&ctx, tid).await?;
        // The post itself must be visible to the viewer: attachments of soft-deleted or
        // unapproved posts are not public just because their thread is.
        let own_post = post_uid == ctx.uid() && ctx.uid() > 0;
        if !ctx.visible_states(fid).contains(&post_visible) && !(post_visible == 0 && own_post)
            || (post_visible == -1 && !ctx.is_mod(fid))
        {
            return Err(AppError::not_found("attachment"));
        }
        // A thumbnail request falls back to the original file when there is no separate
        // thumbnail, so only a real thumbnail is exempt from the download permission.
        let serves_original = !thumb || thumbnail.is_empty();
        let fp = ctx.forum_perms(fid);
        if serves_original && (!fp.candlattachments || !ctx.perms.candlattachments) {
            return Err(AppError::NoPermission(
                "You do not have permission to download attachments.".into(),
            ));
        }
        if !visible && uid != ctx.uid() && !ctx.is_mod(fid) {
            return Err(AppError::not_found("attachment"));
        }
    }
    let path = if thumb && !thumbnail.is_empty() {
        thumbnail
    } else {
        attachname
    };
    let Some(stored) = ctx
        .app
        .storage
        .get(&path)
        .await
        .map_err(|_| AppError::not_found("attachment file"))?
    else {
        return Err(AppError::not_found("attachment file"));
    };
    let len = stored.size;
    let _ = size;
    if !thumb && ctx.method == "GET" {
        let db = ctx.app.db.clone();
        tokio::spawn(async move {
            let _ = sqlx::query("UPDATE attachments SET downloads = downloads + 1 WHERE aid = $1")
                .bind(aid)
                .execute(&db)
                .await;
        });
    }
    let is_image = filetype.starts_with("image/") && filetype != "image/svg+xml";
    let ctype = if thumb && path.ends_with(".png") {
        "image/png".to_string()
    } else if is_image
        || filetype == "application/pdf"
        || filetype.starts_with("video/")
        || filetype.starts_with("audio/")
    {
        filetype
    } else {
        "application/octet-stream".to_string()
    };
    let disp_name =
        percent_encoding::utf8_percent_encode(&filename, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    let disposition = if is_image || ctype == "application/pdf" {
        format!("inline; filename*=UTF-8''{disp_name}")
    } else {
        format!("attachment; filename*=UTF-8''{disp_name}")
    };
    let mut resp = Response::new(Body::from_stream(stored.stream));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&ctype)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment")),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'; sandbox"));
    Ok(resp)
}

#[derive(Deserialize, Default)]
pub struct Empty {
    #[serde(default, deserialize_with = "de::string")]
    pub _x: String,
}

pub async fn remove(
    ctx: Ctx,
    Path(aid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    let me = ctx.require_login()?;
    let row: Option<(i32, i32, String, String)> =
        sqlx::query_as("SELECT pid, uid, attachname, thumbnail FROM attachments WHERE aid = $1")
            .bind(aid)
            .fetch_optional(&ctx.app.db)
            .await?;
    let (pid, uid, a, t) = row.ok_or_else(|| AppError::not_found("attachment"))?;
    let allowed = uid == me.uid
        || if pid > 0 {
            let fid: i32 = sqlx::query_scalar("SELECT fid FROM posts WHERE pid = $1")
                .bind(pid)
                .fetch_one(&ctx.app.db)
                .await?;
            ctx.mod_perms(fid).map(|m| m.caneditposts).unwrap_or(false)
        } else {
            false
        };
    if !allowed {
        return Err(AppError::no_perm());
    }
    // The row goes now; the files once that has committed.
    let mut uow = crate::usecase::Uow::begin(&ctx.app).await?;
    sqlx::query("DELETE FROM attachments WHERE aid = $1")
        .bind(aid)
        .execute(uow.conn())
        .await?;
    if pid > 0 {
        sqlx::query("UPDATE posts SET parser_rev = -1 WHERE pid = $1")
            .bind(pid)
            .execute(uow.conn())
            .await?;
    }
    uow.job(crate::infra::outbox::Job::DeleteFiles {
        paths: [a, t].into_iter().filter(|p| !p.is_empty()).collect(),
    });
    uow.commit(&ctx.app).await?;
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

pub async fn approve(
    ctx: Ctx,
    Path(aid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    let pid: i32 = sqlx::query_scalar("SELECT pid FROM attachments WHERE aid = $1")
        .bind(aid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("attachment"))?;
    let fid: i32 = sqlx::query_scalar("SELECT fid FROM posts WHERE pid = $1")
        .bind(pid)
        .fetch_one(&ctx.app.db)
        .await?;
    if !ctx
        .mod_perms(fid)
        .map(|m| m.canapproveunapproveattachs)
        .unwrap_or(false)
    {
        return Err(AppError::no_perm());
    }
    sqlx::query("UPDATE attachments SET visible = TRUE WHERE aid = $1")
        .bind(aid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(&format!("/post/{pid}"), "The attachment has been approved."))
}
