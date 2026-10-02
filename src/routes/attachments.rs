//! Attachment upload (AJAX, with thumbnails and quotas) and permission-checked downloads.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
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
    let mut file: Option<(String, Vec<u8>)> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::user(format!("Upload failed: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                let name = field.file_name().unwrap_or("file").to_string();
                let data = field
                    .bytes()
                    .await
                    .map_err(|_| AppError::user("Upload failed."))?;
                file = Some((name, data.to_vec()));
            }
            "posthash" => posthash = field.text().await.unwrap_or_default(),
            "pid" => pid = field.text().await.unwrap_or_default().parse().unwrap_or(0),
            "fid" => fid = field.text().await.unwrap_or_default().parse().unwrap_or(0),
            "my_post_key" => key = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }
    ctx.check_csrf(&key)?;
    let (filename, data) = file.ok_or_else(|| AppError::user("No file was uploaded."))?;
    let filename: String = filename
        .replace(['/', '\\', '\0'], "_")
        .chars()
        .take(120)
        .collect();
    if data.is_empty() {
        return Ok(json_err("The uploaded file is empty."));
    }
    // Editing an existing post: must be allowed to edit it.
    if pid > 0 {
        let row: Option<(i32, i32)> = sqlx::query_as("SELECT uid, fid FROM posts WHERE pid = $1")
            .bind(pid)
            .fetch_optional(&ctx.app.db)
            .await?;
        let (puid, pfid) = row.ok_or_else(|| AppError::not_found("post"))?;
        if puid != me.uid && !ctx.mod_perms(pfid).map(|m| m.caneditposts).unwrap_or(false) {
            return Ok(json_err("You cannot add attachments to this post."));
        }
        fid = pfid;
        posthash.clear();
    } else if posthash.len() < 16 || !posthash.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(json_err("Invalid upload session. Please reload the page."));
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
    if at.maxsize > 0 && data.len() as i64 > at.maxsize as i64 * 1024 {
        return Ok(json_err(&format!(
            "The file is too large. The maximum size for .{ext} files is {}.",
            util::format_bytes(at.maxsize as i64 * 1024)
        )));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE (posthash = $1 AND posthash <> '' AND uid = $3) OR (pid = $2 AND pid > 0)")
        .bind(&posthash)
        .bind(pid)
        .bind(me.uid)
        .fetch_one(&ctx.app.db)
        .await?;
    let max = s.int("maxattachments");
    if max > 0 && count >= max && !ctx.is_any_mod() {
        return Ok(json_err(&format!(
            "You can attach at most {max} files to a post."
        )));
    }
    if ctx.perms.attachquota > 0 {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(filesize), 0)::bigint FROM attachments WHERE uid = $1",
        )
        .bind(me.uid)
        .fetch_one(&ctx.app.db)
        .await?;
        if used + data.len() as i64 > ctx.perms.attachquota as i64 * 1024 {
            return Ok(json_err(
                "Uploading this file would exceed your attachment quota. Delete some attachments in the User CP first.",
            ));
        }
    }
    let is_image = at.mimetype.starts_with("image/");
    // Images must decode; thumbnails are generated for them.
    let (tw, th) = (
        s.int("attachthumbw").max(32) as u32,
        s.int("attachthumbh").max(32) as u32,
    );
    let data_for_thumb = if is_image { Some(data.clone()) } else { None };
    let thumb: Option<Vec<u8>> = match data_for_thumb {
        Some(d) => {
            let r = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, String> {
                let img = crate::util::decode_image(&d).map_err(|_| {
                    "The image appears to be corrupt, too large, or is not a supported format."
                        .to_string()
                })?;
                if img.width() <= tw && img.height() <= th {
                    return Ok(None);
                }
                let t = img.thumbnail(tw, th);
                let mut out = std::io::Cursor::new(Vec::new());
                t.write_to(&mut out, image::ImageFormat::Png)
                    .map_err(|e| e.to_string())?;
                Ok(Some(out.into_inner()))
            })
            .await
            .map_err(|e| AppError::Other(e.into()))?;
            match r {
                Ok(t) => t,
                Err(e) => return Ok(json_err(&e)),
            }
        }
        None => None,
    };
    let month = chrono::Utc::now().format("%Y%m").to_string();
    let dir = format!("{}/attachments/{month}", ctx.app.cfg.upload_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::Other(e.into()))?;
    let stem = format!("{}_{}", me.uid, util::random_token(24));
    let attachname = format!("attachments/{month}/{stem}.attach");
    tokio::fs::write(format!("{}/{attachname}", ctx.app.cfg.upload_dir), &data)
        .await
        .map_err(|e| AppError::Other(e.into()))?;
    let thumbname = match thumb {
        Some(t) => {
            let n = format!("attachments/{month}/{stem}_thumb.png");
            tokio::fs::write(format!("{}/{n}", ctx.app.cfg.upload_dir), &t)
                .await
                .map_err(|e| AppError::Other(e.into()))?;
            n
        }
        None if is_image => attachname.clone(),
        None => String::new(),
    };
    let visible = !(fid > 0 && ctx.forum_perms(fid).modattachments && !ctx.is_mod(fid))
        && !ctx.perms.modattachments
        || ctx.is_any_mod();
    let aid: i32 = sqlx::query_scalar(
        "INSERT INTO attachments (pid, posthash, uid, filename, filetype, filesize, attachname, dateuploaded, visible, thumbnail) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING aid",
    )
    .bind(pid)
    .bind(&posthash)
    .bind(me.uid)
    .bind(&filename)
    .bind(&at.mimetype)
    .bind(data.len() as i64)
    .bind(&attachname)
    .bind(now())
    .bind(visible)
    .bind(if thumbname == attachname { String::new() } else { thumbname })
    .fetch_one(&ctx.app.db)
    .await?;
    if pid > 0 {
        sqlx::query("UPDATE posts SET parser_rev = -1 WHERE pid = $1")
            .bind(pid)
            .execute(&ctx.app.db)
            .await?;
    }
    Ok(Json(serde_json::json!({"aid": aid, "filename": filename, "size": util::format_bytes(data.len() as i64), "visible": visible, "is_image": is_image})).into_response())
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
        let fp = ctx.forum_perms(fid);
        if !thumb && (!fp.candlattachments || !ctx.perms.candlattachments) {
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
    if path.contains("..") {
        return Err(AppError::not_found("attachment"));
    }
    let full = format!("{}/{path}", ctx.app.cfg.upload_dir);
    let file = tokio::fs::File::open(&full)
        .await
        .map_err(|_| AppError::not_found("attachment file"))?;
    let len = file
        .metadata()
        .await
        .map(|m| m.len())
        .unwrap_or(size as u64);
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
    let stream = tokio_util_stream(file);
    let mut resp = Response::new(Body::from_stream(stream));
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

fn tokio_util_stream(
    file: tokio::fs::File,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> {
    use tokio::io::AsyncReadExt;
    async_stream::try_stream! {
        let mut f = file;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 { break; }
            yield bytes::Bytes::copy_from_slice(&buf[..n]);
        }
    }
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
    sqlx::query("DELETE FROM attachments WHERE aid = $1")
        .bind(aid)
        .execute(&ctx.app.db)
        .await?;
    let _ = tokio::fs::remove_file(format!("{}/{a}", ctx.app.cfg.upload_dir)).await;
    if !t.is_empty() {
        let _ = tokio::fs::remove_file(format!("{}/{t}", ctx.app.cfg.upload_dir)).await;
    }
    if pid > 0 {
        sqlx::query("UPDATE posts SET parser_rev = -1 WHERE pid = $1")
            .bind(pid)
            .execute(&ctx.app.db)
            .await?;
    }
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
