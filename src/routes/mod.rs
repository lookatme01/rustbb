//! HTTP routes.

pub mod api;
pub mod archive;
pub mod attachments;
pub mod calendar;
pub mod captcha;
pub mod forumdisplay;
pub mod index;
pub mod live;
pub mod member;
pub mod memberlist;
pub mod misc;
pub mod modcp;
pub mod modnotes;
pub mod moderation;
pub mod online;
pub mod pgp;
pub mod polls;
pub mod portal;
pub mod posting;
pub mod private;
pub mod report;
pub mod reputation;
pub mod search;
pub mod showthread;
pub mod stats;
pub mod syndication;
pub mod usercp;
pub mod warnings;

use crate::app::App;

/// Local path of the Referer header (for "go back" redirects), or a fallback.
pub fn misc_back(ctx: &crate::ctx::Ctx, fallback: &str) -> String {
    ctx.headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| url::Url::parse(r).ok())
        .map(|u| {
            format!(
                "{}{}",
                u.path(),
                u.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        })
        .filter(|p| p.starts_with('/') && !p.starts_with("//"))
        .unwrap_or_else(|| fallback.to_string())
}
use axum::response::Redirect;
use axum::Router;
use axum::routing::{get, post};

pub fn router() -> Router<App> {
    Router::new()
        .route("/", get(index::index))
        .route("/index.php", get(index::index))
        .route("/forum/{fid}", get(forumdisplay::forumdisplay))
        .route("/forum/{fid}/markread", post(misc::mark_forum_read))
        .route("/forum/{fid}/password", post(misc::forum_password))
        .route("/forum/{fid}/subscribe", post(misc::subscribe_forum))
        .route("/markread", post(misc::mark_all_read))
        .route("/thread/{tid}", get(showthread::showthread))
        .route("/thread/{tid}/lastpost", get(showthread::lastpost))
        .route("/thread/{tid}/newpost", get(showthread::newpost))
        .route("/thread/{tid}/print", get(showthread::printthread))
        .route("/thread/{tid}/whoposted", get(showthread::whoposted))
        .route("/thread/{tid}/send", get(showthread::sendthread_form).post(showthread::sendthread_submit))
        .route("/thread/{tid}/subscribe", post(misc::subscribe_thread))
        .route("/thread/{tid}/rate", post(misc::rate_thread))
        .route("/thread/{tid}/poll/vote", post(polls::vote))
        .route("/thread/{tid}/poll/undo", post(polls::undo_vote))
        .route("/thread/{tid}/poll/results", get(polls::results))
        .route(
            "/thread/{tid}/poll/edit",
            get(polls::edit_form).post(polls::edit_save),
        )
        .route(
            "/thread/{tid}/poll/new",
            get(polls::new_form).post(polls::new_save),
        )
        .route("/thread/{tid}/since/{pid}", get(showthread::posts_since))
        .route("/post/{pid}", get(showthread::goto_post))
        .route("/post/{pid}/history", get(showthread::edit_history))
        .route("/post/{pid}/react", post(misc::react))
        .route("/post/{pid}/reactions", get(misc::reactions_list))
        .route("/post/{pid}/quote", get(posting::quote_json))
        .route(
            "/newthread/{fid}",
            get(posting::newthread_form).post(posting::newthread_submit),
        )
        .route(
            "/newreply/{tid}",
            get(posting::newreply_form).post(posting::newreply_submit),
        )
        .route(
            "/editpost/{pid}",
            get(posting::editpost_form).post(posting::editpost_submit),
        )
        .route("/deletepost/{pid}", post(posting::deletepost))
        .route("/restorepost/{pid}", post(posting::restorepost))
        .route("/preview", post(posting::preview_json))
        .route("/drafts/save", post(posting::save_draft))
        .route("/announcement/{aid}", get(misc::announcement))
        // members
        .route(
            "/member/login",
            get(member::login_form).post(member::login_submit),
        )
        .route("/member/login/2fa", post(member::login_2fa))
        .route("/member/logout", post(member::logout))
        .route(
            "/member/register",
            get(member::register_form).post(member::register_submit),
        )
        .route("/member/activate", get(member::activate))
        .route(
            "/member/resendactivation",
            get(member::resend_form).post(member::resend_submit),
        )
        .route(
            "/member/lostpw",
            get(member::lostpw_form).post(member::lostpw_submit),
        )
        .route(
            "/member/resetpw",
            get(member::resetpw_form).post(member::resetpw_submit),
        )
        .route("/member/checkname", get(member::check_username))
        .route("/user/{uid}", get(member::profile))
        .route("/user/name/{name}", get(member::profile_by_name))
        .route(
            "/user/{uid}/email",
            get(member::email_form).post(member::email_submit),
        )
        .route("/user/{uid}/threads", get(search::user_threads))
        .route("/user/{uid}/posts", get(search::user_posts))
        .route("/user/{uid}/referrals", get(member::referrals))
        .route("/members", get(memberlist::memberlist))
        .route("/team", get(memberlist::showteam))
        // user cp
        .merge(usercp::router())
        .merge(private::router())
        .merge(pgp::router())
        .merge(modcp::router())
        .merge(modnotes::router())
        .merge(moderation::router())
        .route("/search", get(search::search_form).post(search::do_search))
        .route("/search/results/{sid}", get(search::results))
        .route("/search/new", get(search::new_posts))
        .route("/search/today", get(search::today_posts))
        .route("/search/unread", get(search::unread_threads))
        .route("/search/unanswered", get(search::unanswered))
        .route("/search/quick", get(search::quick))
        .route("/online", get(online::online))
        .route("/online/today", get(online::online_today))
        .route("/stats", get(stats::stats))
        .route("/portal", get(portal::portal))
        .route("/calendar", get(calendar::calendar))
        .route("/calendar/{cid}", get(calendar::calendar_cid))
        .route("/calendar/{cid}/day/{date}", get(calendar::day))
        .route("/calendar/{cid}/week/{date}", get(calendar::week))
        .route("/calendar/event/{eid}", get(calendar::event))
        .route(
            "/calendar/{cid}/addevent",
            get(calendar::add_form).post(calendar::add_submit),
        )
        .route(
            "/calendar/event/{eid}/edit",
            get(calendar::edit_form).post(calendar::edit_submit),
        )
        .route("/calendar/event/{eid}/delete", post(calendar::delete))
        .route("/calendar/event/{eid}/approve", post(calendar::approve))
        .route("/reputation/{uid}", get(reputation::list))
        .route(
            "/reputation/{uid}/add",
            get(reputation::add_form).post(reputation::add_submit),
        )
        .route("/reputation/delete/{rid}", post(reputation::delete))
        .route("/warnings/{uid}", get(warnings::list))
        .route(
            "/warnings/{uid}/warn",
            get(warnings::warn_form).post(warnings::warn_submit),
        )
        .route("/warning/{wid}", get(warnings::view))
        .route("/warning/{wid}/revoke", post(warnings::revoke))
        .route("/report", get(report::form).post(report::submit))
        .route("/attachment/{aid}", get(attachments::download))
        .route("/attachment/upload", post(attachments::upload))
        .route("/attachment/{aid}/remove", post(attachments::remove))
        .route("/attachment/{aid}/approve", post(attachments::approve))
        .route("/captcha/{hash}", get(captcha::image))
        .route("/captcha/refresh", get(captcha::refresh))
        .route("/help", get(misc::help))
        .route("/help/{hid}", get(misc::help_doc))
        .route("/rules", get(misc::rules))
        .route("/privacy", get(misc::privacy))
        .route(
            "/contact",
            get(misc::contact_form).post(misc::contact_submit),
        )
        .route("/smilies", get(misc::smilies))
        .route("/mycode", get(misc::mycode_help))
        .route("/buddypopup", get(misc::buddy_popup))
        .route("/syndication", get(syndication::feed))
        .route("/syndication.xml", get(syndication::feed))
        .route("/sitemap.xml", get(syndication::sitemap))
        .route("/robots.txt", get(syndication::robots))
        .route("/archive", get(archive::index))
        .route("/archive/forum/{fid}", get(archive::forum))
        .route("/archive/thread/{tid}", get(archive::thread))
        .route("/live", get(live::stream))
        .route("/theme/{tid}", post(misc::set_theme))
        .route("/lang/{code}", post(misc::set_lang))
        .route("/colormode", post(misc::set_colormode))
        // MyBB legacy URL compatibility (migrated boards keep their inbound links).
        .route("/showthread.php", get(misc::legacy_showthread))
        .route("/forumdisplay.php", get(misc::legacy_forumdisplay))
        .route("/member.php", get(misc::legacy_member))
        .route("/sendthread.php", get(misc::legacy_showthread))
        .route("/newreply.php", get(misc::legacy_showthread))
        .route("/newthread.php", get(misc::legacy_forumdisplay))
        .route("/archive/index.php", get(|| async { Redirect::permanent("/archive") }))
        .route("/private.php", get(|| async { Redirect::permanent("/pm") }))
        .route("/usercp.php", get(|| async { Redirect::permanent("/usercp") }))
        .route("/memberlist.php", get(|| async { Redirect::permanent("/members") }))
        .route("/showteam.php", get(|| async { Redirect::permanent("/team") }))
        .route("/search.php", get(|| async { Redirect::permanent("/search") }))
        .route("/calendar.php", get(|| async { Redirect::permanent("/calendar") }))
        .route("/portal.php", get(|| async { Redirect::permanent("/portal") }))
        .route("/online.php", get(|| async { Redirect::permanent("/online") }))
        .route("/stats.php", get(|| async { Redirect::permanent("/stats") }))
        .route("/syndication.php", get(|axum::extract::RawQuery(q): axum::extract::RawQuery| async move {
            Redirect::permanent(&match q { Some(q) => format!("/syndication?{q}"), None => "/syndication".into() })
        }))
        .nest("/api/v1", api::router())
        .nest("/admin", crate::admin::router())
}
