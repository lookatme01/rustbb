use crate::{
    app::App,
    automod::Policy,
    ctx::{CsrfForm, Ctx, de},
    error::{AppError, AppResult},
    util::now,
};
use axum::{
    Router,
    response::Response,
    routing::{get, post},
};
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/automod", get(index).post(save))
        .route("/automod/undo", post(undo))
        .route("/automod/rollback", post(rollback))
        .route("/automod/restore-policy", post(restore_policy))
}
async fn index(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "content");
    let (revision, value): (i64, serde_json::Value) =
        sqlx::query_as("SELECT revision, policy FROM automod_config WHERE id = 1")
            .fetch_one(&ctx.app.db)
            .await?;
    let policy: Policy = serde_json::from_value(value).map_err(anyhow::Error::from)?;
    let list: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT to_jsonb(a) FROM (SELECT * FROM automod_actions ORDER BY id DESC LIMIT 100) a",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let history: Vec<(serde_json::Value,)> = sqlx::query_as("SELECT to_jsonb(h) FROM (SELECT * FROM automod_config_history ORDER BY id DESC LIMIT 20) h").fetch_all(&ctx.app.db).await?;
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM automod_queue")
        .fetch_one(&ctx.app.db)
        .await?;
    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM automod_actions WHERE status = 'quarantined'")
            .fetch_one(&ctx.app.db)
            .await?;
    crate::admin::page(&ctx,"admin/automod.html","content","Automated moderation",minijinja::context! {policy=>policy, revision=>revision, phrases=>policy.phrases.join("\n"), list=>list.into_iter().map(|x|x.0).collect::<Vec<_>>(), history=>history.into_iter().map(|x|x.0).collect::<Vec<_>>(), queued=>queued, pending=>pending}).await
}
#[derive(Deserialize)]
struct PolicyForm {
    #[serde(deserialize_with = "de::i64")]
    revision: i64,
    mode: String,
    #[serde(deserialize_with = "de::i64")]
    max_links: i64,
    #[serde(deserialize_with = "de::i32")]
    new_user_posts: i32,
    #[serde(deserialize_with = "de::i64")]
    duplicate_limit: i64,
    #[serde(default)]
    phrases: String,
}
async fn write_policy(ctx: &Ctx, revision: i64, policy: Policy) -> AppResult<()> {
    policy.validate()?;
    let value = serde_json::to_value(&policy).map_err(anyhow::Error::from)?;
    let mut tx = ctx.app.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(424244)")
        .execute(&mut *tx)
        .await?;
    let (current, before): (i64, serde_json::Value) =
        sqlx::query_as("SELECT revision, policy FROM automod_config WHERE id = 1 FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    if current != revision {
        return Err(AppError::user(
            "The policy changed in another session. Refresh before saving.",
        ));
    }
    sqlx::query("UPDATE automod_config SET revision = revision + 1, policy = $1 WHERE id = 1")
        .bind(&value)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO automod_config_history (uid,dateline,before_policy,after_policy) VALUES ($1,$2,$3,$4)").bind(ctx.uid()).bind(now()).bind(&before).bind(&value).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO adminlog (uid,ipaddress,dateline,module,action,data) VALUES ($1,$2,$3,'content','Automod policy changed',$4)").bind(ctx.uid()).bind(crate::util::IpText::from(&ctx.ip)).bind(now()).bind(serde_json::json!({"before":before,"after":value})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn save(ctx: Ctx, CsrfForm(f): CsrfForm<PolicyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "content");
    if f.max_links < 0 {
        return Err(AppError::user("Link threshold cannot be negative."));
    }
    let policy = Policy {
        mode: f.mode,
        max_links: f.max_links as usize,
        new_user_posts: f.new_user_posts,
        duplicate_limit: f.duplicate_limit,
        phrases: f
            .phrases
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    write_policy(&ctx, f.revision, policy).await?;
    Ok(ctx.redirect("/admin/automod", "The moderation policy has been saved."))
}
#[derive(Deserialize)]
struct ActionForm {
    #[serde(deserialize_with = "de::i64")]
    id: i64,
}
async fn undo(ctx: Ctx, CsrfForm(f): CsrfForm<ActionForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "content");
    let ok = crate::automod::undo(&ctx.app, f.id, ctx.uid()).await?;
    Ok(ctx.redirect("/admin/automod",if ok {"The action has been undone. This post is now exempt from automation."} else {"Unable to undo: content was changed, removed, or already restored. Review the moderator log."}))
}
#[derive(Deserialize)]
struct RevisionForm {
    #[serde(deserialize_with = "de::i64")]
    revision: i64,
}
async fn rollback(ctx: Ctx, CsrfForm(f): CsrfForm<RevisionForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "content");
    let value: serde_json::Value =
        sqlx::query_scalar("SELECT policy FROM automod_config WHERE id = 1")
            .fetch_one(&ctx.app.db)
            .await?;
    let mut policy: Policy = serde_json::from_value(value).map_err(anyhow::Error::from)?;
    policy.mode = "off".into();
    write_policy(&ctx, f.revision, policy).await?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM automod_actions WHERE status = 'quarantined' ORDER BY id DESC",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let mut restored = 0;
    let mut conflicts = 0;
    for id in ids {
        if crate::automod::undo(&ctx.app, id, ctx.uid()).await? {
            restored += 1;
        } else {
            conflicts += 1;
        }
    }
    Ok(ctx.redirect("/admin/automod",&format!("Automation disabled. Restored {restored} actions; {conflicts} conflicts need manual review. History has been preserved.")))
}
#[derive(Deserialize)]
struct RestoreForm {
    #[serde(deserialize_with = "de::i64")]
    id: i64,
    #[serde(deserialize_with = "de::i64")]
    revision: i64,
}
async fn restore_policy(ctx: Ctx, CsrfForm(f): CsrfForm<RestoreForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "content");
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT before_policy FROM automod_config_history WHERE id = $1")
            .bind(f.id)
            .fetch_optional(&ctx.app.db)
            .await?;
    let policy =
        serde_json::from_value(value.ok_or_else(|| AppError::not_found("policy revision"))?)
            .map_err(anyhow::Error::from)?;
    write_policy(&ctx, f.revision, policy).await?;
    Ok(ctx.redirect(
        "/admin/automod",
        "The previous policy has been restored and recorded as a new revision.",
    ))
}
