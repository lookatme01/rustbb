//! Server-Sent Events stream: new replies in the thread being read, and the viewer's own
//! alerts / private messages. Events arrive from this node's broadcast channel, which other
//! nodes feed through Postgres NOTIFY.

use crate::ctx::Ctx;
use crate::error::AppResult;
use axum::extract::Query;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Deserialize, Default)]
pub struct LiveQuery {
    #[serde(default)]
    pub tid: i32,
}

pub async fn stream(ctx: Ctx, Query(q): Query<LiveQuery>) -> AppResult<Response> {
    let tid = if q.tid > 0 {
        crate::routes::showthread::check_thread(&ctx, q.tid).await?;
        q.tid
    } else {
        0
    };
    let uid = ctx.uid();
    if tid == 0 && uid == 0 {
        return Ok(axum::http::StatusCode::NO_CONTENT.into_response());
    }
    // Cap open streams (in total, per address, per member); the slot is held until the stream
    // ends, however it ends.
    let guard = match ctx.app.streams.acquire(&ctx.ip, uid) {
        Ok(g) => g,
        Err(why) => {
            let which = match why {
                crate::infra::streams::Refused::Total => "total",
                crate::infra::streams::Refused::Address => "address",
                crate::infra::streams::Refused::Member => "member",
            };
            crate::infra::metrics::counter_with("rbb_live_refused_total", &[("limit", which)], 1);
            return Ok((
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "30")],
                "Too many open live-update connections.",
            )
                .into_response());
        }
    };
    // Subscribe only to this thread's and this member's topics: an event wakes just the
    // streams it concerns.
    type Events = std::pin::Pin<Box<dyn futures::Stream<Item = crate::app::LiveEvent> + Send>>;
    let topic = |rx: tokio::sync::broadcast::Receiver<crate::app::LiveEvent>| -> Events {
        Box::pin(BroadcastStream::new(rx).filter_map(|e| e.ok()))
    };
    let thread_events: Events = if tid > 0 {
        topic(ctx.app.live.thread(tid))
    } else {
        Box::pin(futures::stream::empty())
    };
    let user_events: Events = if uid > 0 {
        topic(ctx.app.live.user(uid))
    } else {
        Box::pin(futures::stream::empty())
    };
    let s = futures::stream::select(thread_events, user_events).map(move |ev| {
        let _slot = &guard;
        Ok::<Event, std::convert::Infallible>(
            Event::default().event(ev.kind).data(ev.data.to_string()),
        )
    });
    // End streams after an hour, or at once when the server shuts down; the browser reconnects
    // automatically.
    let s = futures::StreamExt::take_until(s, tokio::time::sleep(Duration::from_secs(3600)));
    let mut stopping = ctx.app.shutdown.subscribe();
    let s = futures::StreamExt::take_until(s, async move {
        let _ = stopping.wait_for(|s| *s).await;
    });
    Ok(Sse::new(s)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(25))
                .text("ping"),
        )
        .into_response())
}
