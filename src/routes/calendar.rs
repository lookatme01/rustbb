//! Calendar: month/week/day views, events (single, ranged, repeating), birthdays, moderation.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::Calendar;
use crate::util::now;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Datelike, Duration, NaiveDate};
use serde::Deserialize;

#[derive(Clone, Debug)]
struct CalPerms {
    canview: bool,
    canadd: bool,
    bypass: bool,
    moderate: bool,
}

async fn cal_perms(ctx: &Ctx, cid: i32) -> AppResult<CalPerms> {
    let rows: Vec<(i32, serde_json::Value)> =
        sqlx::query_as("SELECT gid, perms FROM calendarpermissions WHERE cid = $1")
            .bind(cid)
            .fetch_all(&ctx.app.db)
            .await?;
    let mut p = CalPerms {
        canview: false,
        canadd: false,
        bypass: false,
        moderate: false,
    };
    for g in &ctx.groups {
        let gp = ctx
            .cache
            .group(*g)
            .map(|x| x.perms.0.clone())
            .unwrap_or_default();
        let (v, a, b, m) = match rows.iter().find(|r| r.0 == *g) {
            Some((_, j)) => (
                j["canviewcalendar"].as_bool().unwrap_or(gp.canviewcalendar),
                j["canaddevents"].as_bool().unwrap_or(gp.canaddevents),
                j["canbypasseventmod"]
                    .as_bool()
                    .unwrap_or(gp.canbypasseventmod),
                j["canmoderateevents"]
                    .as_bool()
                    .unwrap_or(gp.canmoderateevents),
            ),
            None => (
                gp.canviewcalendar,
                gp.canaddevents,
                gp.canbypasseventmod,
                gp.canmoderateevents,
            ),
        };
        p.canview |= v;
        p.canadd |= a;
        p.bypass |= b;
        p.moderate |= m;
    }
    if ctx.uid() == 0 {
        p.canadd = false;
    }
    Ok(p)
}

fn get_calendar(ctx: &Ctx, cid: i32) -> AppResult<Calendar> {
    if !ctx.settings().bool("enablecalendar") {
        return Err(AppError::user("The calendar is disabled."));
    }
    ctx.cache
        .calendars
        .iter()
        .find(|c| c.cid == cid)
        .cloned()
        .ok_or_else(|| AppError::not_found("calendar"))
}

#[derive(sqlx::FromRow, Clone, serde::Serialize)]
struct EventRow {
    eid: i32,
    cid: i32,
    uid: i32,
    name: String,
    description: String,
    visible: bool,
    private: bool,
    dateline: i64,
    starttime: i64,
    endtime: i64,
    timezone: String,
    ignoretimezone: bool,
    usingtime: bool,
    repeats: sqlx::types::Json<serde_json::Value>,
}

/// Local calendar date of a timestamp for this event (all-day events ignore time zones).
fn event_date(ctx: &Ctx, e: &EventRow, ts: i64) -> NaiveDate {
    if e.ignoretimezone || !e.usingtime {
        chrono::DateTime::from_timestamp(ts, 0)
            .unwrap_or_default()
            .date_naive()
    } else {
        crate::util::to_local(ts, ctx.tz).date_naive()
    }
}

/// Dates within [from, to] on which the event occurs.
fn occurrences(ctx: &Ctx, e: &EventRow, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    let start = event_date(ctx, e, e.starttime);
    let end = if e.endtime > e.starttime {
        event_date(ctx, e, e.endtime)
    } else {
        start
    };
    let span = (end - start).num_days().max(0);
    let kind = e.repeats.0["type"].as_str().unwrap_or("none");
    let interval = e.repeats.0["interval"].as_i64().unwrap_or(1).max(1);
    let until = e.repeats.0["until"].as_i64().filter(|u| *u > 0).map(|u| {
        chrono::DateTime::from_timestamp(u, 0)
            .unwrap_or_default()
            .date_naive()
    });
    let mut out = vec![];
    let push_span = |d: NaiveDate, out: &mut Vec<NaiveDate>| {
        for i in 0..=span {
            let x = d + Duration::days(i);
            if x >= from && x <= to {
                out.push(x);
            }
        }
    };
    match kind {
        "none" | "" => push_span(start, &mut out),
        _ => {
            let mut d = start;
            let mut guard = 0;
            while d <= to && guard < 5000 {
                if until.map(|u| d > u).unwrap_or(false) {
                    break;
                }
                if d + Duration::days(span) >= from {
                    push_span(d, &mut out);
                }
                d = match kind {
                    "daily" => d + Duration::days(interval),
                    "weekly" => d + Duration::weeks(interval),
                    "monthly" => d
                        .checked_add_months(chrono::Months::new(interval as u32))
                        .unwrap_or(to + Duration::days(1)),
                    "yearly" => d
                        .checked_add_months(chrono::Months::new(12 * interval as u32))
                        .unwrap_or(to + Duration::days(1)),
                    _ => to + Duration::days(1),
                };
                guard += 1;
            }
        }
    }
    out
}

async fn events_between(
    ctx: &Ctx,
    cid: i32,
    from: NaiveDate,
    to: NaiveDate,
    perms: &CalPerms,
) -> AppResult<Vec<(NaiveDate, EventRow)>> {
    let from_ts = from.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() - 86400;
    let to_ts = to.and_hms_opt(23, 59, 59).unwrap().and_utc().timestamp() + 86400;
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT eid, cid, uid, name, description, visible, private, dateline, starttime, endtime, timezone, ignoretimezone, usingtime, repeats FROM events
         WHERE cid = $1 AND starttime <= $3 AND (GREATEST(endtime, starttime) >= $2 OR COALESCE(repeats->>'type', 'none') <> 'none')
           AND (visible OR $4 OR uid = $5) AND (NOT private OR uid = $5)",
    )
    .bind(cid)
    .bind(from_ts)
    .bind(to_ts)
    .bind(perms.moderate)
    .bind(ctx.uid())
    .fetch_all(&ctx.app.db)
    .await?;
    let mut out = vec![];
    for e in rows {
        for d in occurrences(ctx, &e, from, to) {
            out.push((d, e.clone()));
        }
    }
    out.sort_by_key(|(d, e)| (*d, e.starttime));
    Ok(out)
}

async fn birthdays_between(
    ctx: &Ctx,
    from: NaiveDate,
    to: NaiveDate,
) -> AppResult<Vec<(NaiveDate, i32, String)>> {
    let mut keys = vec![];
    let mut d = from;
    while d <= to {
        keys.push(format!("{}-{}-", d.day(), d.month()));
        d += Duration::days(1);
    }
    let rows: Vec<(i32, String, i32, i32, String)> = sqlx::query_as(
        "SELECT uid, username, usergroup, displaygroup, birthday FROM users WHERE birthday <> '' AND birthdayprivacy <> 'none'
         AND (split_part(birthday, '-', 1) || '-' || split_part(birthday, '-', 2) || '-') = ANY($1) LIMIT 2000",
    )
    .bind(&keys)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut out = vec![];
    for (uid, name, g, dg, b) in rows {
        let parts: Vec<u32> = b.split('-').filter_map(|x| x.parse().ok()).collect();
        if parts.len() < 2 {
            continue;
        }
        let mut d = from;
        while d <= to {
            if d.day() == parts[0] && d.month() == parts[1] {
                out.push((d, uid, ctx.cache.format_name(&name, g, dg)));
            }
            d += Duration::days(1);
        }
    }
    Ok(out)
}

#[derive(Deserialize, Default)]
pub struct MonthQuery {
    pub year: Option<i32>,
    pub month: Option<u32>,
}

pub async fn calendar(ctx: Ctx, q: Query<MonthQuery>) -> AppResult<Response> {
    let cid = ctx
        .cache
        .calendars
        .first()
        .map(|c| c.cid)
        .ok_or_else(|| AppError::not_found("calendar"))?;
    calendar_cid(ctx, Path(cid), q).await
}

pub async fn calendar_cid(
    ctx: Ctx,
    Path(cid): Path<i32>,
    Query(q): Query<MonthQuery>,
) -> AppResult<Response> {
    let cal = get_calendar(&ctx, cid)?;
    let perms = cal_perms(&ctx, cid).await?;
    if !perms.canview || !ctx.perms.canviewcalendar {
        return Err(AppError::no_perm());
    }
    let today = crate::util::to_local(now(), ctx.tz).date_naive();
    let year = q.year.unwrap_or(today.year()).clamp(1901, 2100);
    let month = q.month.unwrap_or(today.month()).clamp(1, 12);
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let next = first.checked_add_months(chrono::Months::new(1)).unwrap();
    let last = next - Duration::days(1);
    // Grid starts on the calendar's start of week.
    let sow = cal.startofweek.clamp(0, 6) as i64;
    let offset = (first.weekday().num_days_from_sunday() as i64 - sow).rem_euclid(7);
    let grid_start = first - Duration::days(offset);
    let grid_end = grid_start + Duration::days(41);
    let events = events_between(&ctx, cid, grid_start, grid_end, &perms).await?;
    let birthdays = if cal.showbirthdays {
        birthdays_between(&ctx, grid_start, grid_end).await?
    } else {
        vec![]
    };
    let limit = cal.eventlimit.max(1) as usize;
    let mut weeks = vec![];
    for w in 0..6 {
        let mut days = vec![];
        for i in 0..7 {
            let d = grid_start + Duration::days(w * 7 + i);
            let evs: Vec<_> = events.iter().filter(|(ed, _)| *ed == d).map(|(_, e)| minijinja::context! { eid => e.eid, name => &e.name, visible => e.visible }).collect();
            let bdays = birthdays.iter().filter(|b| b.0 == d).count();
            days.push(minijinja::context! {
                day => d.day(), date => d.format("%Y-%m-%d").to_string(), other => d.month() != month, today => d == today,
                events => evs.iter().take(limit).cloned().collect::<Vec<_>>(), more => evs.len().saturating_sub(limit), birthdays => bdays,
            });
        }
        weeks.push(days);
        if grid_start + Duration::days((w + 1) * 7) > last {
            break;
        }
    }
    let prev = first - Duration::days(1);
    let dow: Vec<String> = (0..7)
        .map(|i| {
            [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
            ][((i + sow) % 7) as usize]
                .to_string()
        })
        .collect();
    ctx.render(
        "calendar.html",
        minijinja::context! {
            title => cal.name.clone(), cal => &cal, calendars => ctx.cache.calendars.to_vec(), weeks => weeks, dow => dow,
            month_name => first.format("%B %Y").to_string(), prev => (prev.year(), prev.month()), next => (next.year(), next.month()),
            can_add => perms.canadd, year => year, month => month,
        },
    )
    .await
}

fn parse_date(s: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| AppError::not_found("date"))
}

pub async fn day(ctx: Ctx, Path((cid, date)): Path<(i32, String)>) -> AppResult<Response> {
    let cal = get_calendar(&ctx, cid)?;
    let perms = cal_perms(&ctx, cid).await?;
    if !perms.canview {
        return Err(AppError::no_perm());
    }
    let d = parse_date(&date)?;
    let events = events_between(&ctx, cid, d, d, &perms).await?;
    let bdays = if cal.showbirthdays {
        birthdays_between(&ctx, d, d).await?
    } else {
        vec![]
    };
    let opts = parse_opts(&cal);
    let list: Vec<_> = events
        .iter()
        .map(|(_, e)| minijinja::context! { eid => e.eid, name => &e.name, html => crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, &e.description), usingtime => e.usingtime, starttime => e.starttime, endtime => e.endtime, visible => e.visible })
        .collect();
    ctx.render(
        "calendar_day.html",
        minijinja::context! { title => d.format("%A, %B %-d, %Y").to_string(), cal => &cal, events => list, birthdays => bdays.iter().map(|b| (b.1, b.2.clone())).collect::<Vec<_>>(), date => date, can_add => perms.canadd,
            prev => (d - Duration::days(1)).format("%Y-%m-%d").to_string(), next => (d + Duration::days(1)).format("%Y-%m-%d").to_string() },
    )
    .await
}

pub async fn week(ctx: Ctx, Path((cid, date)): Path<(i32, String)>) -> AppResult<Response> {
    let cal = get_calendar(&ctx, cid)?;
    let perms = cal_perms(&ctx, cid).await?;
    if !perms.canview {
        return Err(AppError::no_perm());
    }
    let d = parse_date(&date)?;
    let sow = cal.startofweek.clamp(0, 6) as i64;
    let start = d - Duration::days((d.weekday().num_days_from_sunday() as i64 - sow).rem_euclid(7));
    let end = start + Duration::days(6);
    let events = events_between(&ctx, cid, start, end, &perms).await?;
    let bdays = if cal.showbirthdays {
        birthdays_between(&ctx, start, end).await?
    } else {
        vec![]
    };
    let days: Vec<_> = (0..7)
        .map(|i| {
            let x = start + Duration::days(i);
            minijinja::context! {
                label => x.format("%A, %B %-d").to_string(), date => x.format("%Y-%m-%d").to_string(),
                events => events.iter().filter(|(ed, _)| *ed == x).map(|(_, e)| minijinja::context!{ eid => e.eid, name => &e.name, usingtime => e.usingtime, starttime => e.starttime }).collect::<Vec<_>>(),
                birthdays => bdays.iter().filter(|b| b.0 == x).map(|b| (b.1, b.2.clone())).collect::<Vec<_>>(),
            }
        })
        .collect();
    ctx.render(
        "calendar_week.html",
        minijinja::context! { title => format!("Week of {}", start.format("%B %-d, %Y")), cal => &cal, days => days,
            prev => (start - Duration::days(7)).format("%Y-%m-%d").to_string(), next => (start + Duration::days(7)).format("%Y-%m-%d").to_string() },
    )
    .await
}

fn parse_opts(cal: &Calendar) -> crate::parser::ParseOptions {
    crate::parser::ParseOptions {
        allow_html: cal.allowhtml,
        allow_mycode: cal.allowmycode,
        allow_smilies: cal.allowsmilies,
        allow_imgcode: cal.allowimgcode,
        allow_videocode: cal.allowvideocode,
        ..Default::default()
    }
}

async fn load_event(ctx: &Ctx, eid: i32) -> AppResult<EventRow> {
    sqlx::query_as("SELECT eid, cid, uid, name, description, visible, private, dateline, starttime, endtime, timezone, ignoretimezone, usingtime, repeats FROM events WHERE eid = $1")
        .bind(eid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("event"))
}

pub async fn event(ctx: Ctx, Path(eid): Path<i32>) -> AppResult<Response> {
    let e = load_event(&ctx, eid).await?;
    let cal = get_calendar(&ctx, e.cid)?;
    let perms = cal_perms(&ctx, e.cid).await?;
    if !perms.canview
        || (e.private && e.uid != ctx.uid())
        || (!e.visible && !perms.moderate && e.uid != ctx.uid())
    {
        return Err(AppError::not_found("event"));
    }
    let author = crate::render::load_authors(&ctx, &[e.uid])
        .await?
        .remove(&e.uid);
    let html = crate::render::parse_with(
        &ctx.cache,
        &ctx.app.plugins,
        &parse_opts(&cal),
        &e.description,
    );
    let is_owner = e.uid == ctx.uid() && ctx.uid() > 0;
    ctx.render(
        "calendar_event.html",
        minijinja::context! { title => &e.name, cal => &cal, e => &e, html => html, author => author, can_edit => is_owner || perms.moderate, can_moderate => perms.moderate,
            date => event_date(&ctx, &e, e.starttime).format("%Y-%m-%d").to_string(), enddate => if e.endtime > 0 { Some(event_date(&ctx, &e, e.endtime).format("%Y-%m-%d").to_string()) } else { None } },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AddQuery {
    #[serde(default)]
    pub date: String,
}

pub async fn add_form(
    ctx: Ctx,
    Path(cid): Path<i32>,
    Query(q): Query<AddQuery>,
) -> AppResult<Response> {
    let cal = get_calendar(&ctx, cid)?;
    let perms = cal_perms(&ctx, cid).await?;
    if !perms.canadd {
        return Err(AppError::no_perm());
    }
    let date = if q.date.is_empty() {
        crate::util::to_local(now(), ctx.tz)
            .format("%Y-%m-%d")
            .to_string()
    } else {
        q.date
    };
    ctx.render("calendar_form.html", minijinja::context! { title => "Add Event", cal => &cal, e => minijinja::Value::UNDEFINED, date => date, enddate => "", starttime => "", endtime => "", repeat => "none", interval => 1, until => "", form => minijinja::context!{ message => "" } }).await
}

#[derive(Deserialize, Default)]
pub struct EventForm {
    #[serde(default, deserialize_with = "de::string")]
    pub name: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
    #[serde(default, deserialize_with = "de::string")]
    pub date: String,
    #[serde(default, deserialize_with = "de::string")]
    pub enddate: String,
    #[serde(default, deserialize_with = "de::string")]
    pub starttime: String,
    #[serde(default, deserialize_with = "de::string")]
    pub endtime: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub private: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub repeat: String,
    #[serde(default, deserialize_with = "de::i64")]
    pub interval: i64,
    #[serde(default, deserialize_with = "de::string")]
    pub until: String,
}

fn build_times(ctx: &Ctx, f: &EventForm) -> AppResult<(i64, i64, bool, serde_json::Value)> {
    use chrono::TimeZone;
    let d = NaiveDate::parse_from_str(f.date.trim(), "%Y-%m-%d")
        .map_err(|_| AppError::user("Please enter a valid start date."))?;
    let usingtime = !f.starttime.trim().is_empty();
    let to_ts = |date: NaiveDate, time: &str| -> AppResult<i64> {
        if usingtime {
            let t = chrono::NaiveTime::parse_from_str(time.trim(), "%H:%M")
                .map_err(|_| AppError::user("Please enter times as HH:MM."))?;
            Ok(ctx
                .tz
                .from_local_datetime(&date.and_time(t))
                .earliest()
                .map(|x| x.timestamp())
                .unwrap_or(0))
        } else {
            Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
        }
    };
    let start = to_ts(d, &f.starttime)?;
    let end = if !f.enddate.trim().is_empty() || !f.endtime.trim().is_empty() {
        let ed = if f.enddate.trim().is_empty() {
            d
        } else {
            NaiveDate::parse_from_str(f.enddate.trim(), "%Y-%m-%d")
                .map_err(|_| AppError::user("Please enter a valid end date."))?
        };
        let t = if f.endtime.trim().is_empty() {
            "23:59"
        } else {
            f.endtime.trim()
        };
        let e = to_ts(ed, t)?;
        if e < start {
            return Err(AppError::user("The event cannot end before it starts."));
        }
        e
    } else {
        0
    };
    let kind = match f.repeat.as_str() {
        "daily" | "weekly" | "monthly" | "yearly" => f.repeat.clone(),
        _ => "none".into(),
    };
    let until = NaiveDate::parse_from_str(f.until.trim(), "%Y-%m-%d")
        .ok()
        .map(|u| u.and_hms_opt(23, 59, 59).unwrap().and_utc().timestamp())
        .unwrap_or(0);
    Ok((
        start,
        end,
        usingtime,
        serde_json::json!({"type": kind, "interval": f.interval.clamp(1, 365), "until": until}),
    ))
}

pub async fn add_submit(
    ctx: Ctx,
    Path(cid): Path<i32>,
    CsrfForm(f): CsrfForm<EventForm>,
) -> AppResult<Response> {
    let cal = get_calendar(&ctx, cid)?;
    let perms = cal_perms(&ctx, cid).await?;
    if !perms.canadd {
        return Err(AppError::no_perm());
    }
    if f.name.trim().is_empty() || f.message.trim().is_empty() {
        return Err(AppError::user(
            "Please enter an event name and description.",
        ));
    }
    let (start, end, usingtime, repeats) = build_times(&ctx, &f)?;
    let visible = !cal.moderation || perms.bypass || perms.moderate || f.private;
    let eid: i32 = sqlx::query_scalar(
        "INSERT INTO events (cid, uid, name, description, visible, private, dateline, starttime, endtime, timezone, ignoretimezone, usingtime, repeats)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING eid",
    )
    .bind(cid)
    .bind(ctx.uid())
    .bind(f.name.trim().chars().take(120).collect::<String>())
    .bind(f.message.trim())
    .bind(visible)
    .bind(f.private)
    .bind(now())
    .bind(start)
    .bind(end)
    .bind(ctx.tz.name())
    .bind(!usingtime)
    .bind(usingtime)
    .bind(repeats)
    .fetch_one(&ctx.app.db)
    .await?;
    let msg = if visible {
        "Your event has been added."
    } else {
        "Your event has been added and is awaiting moderation."
    };
    Ok(ctx.redirect(&format!("/calendar/event/{eid}"), msg))
}

pub async fn edit_form(ctx: Ctx, Path(eid): Path<i32>) -> AppResult<Response> {
    let e = load_event(&ctx, eid).await?;
    let cal = get_calendar(&ctx, e.cid)?;
    let perms = cal_perms(&ctx, e.cid).await?;
    if !(perms.moderate || (e.uid == ctx.uid() && ctx.uid() > 0)) {
        return Err(AppError::no_perm());
    }
    let fmt_d = |ts: i64| event_date(&ctx, &e, ts).format("%Y-%m-%d").to_string();
    let fmt_t = |ts: i64| {
        if e.usingtime {
            crate::util::to_local(ts, ctx.tz)
                .format("%H:%M")
                .to_string()
        } else {
            String::new()
        }
    };
    let until = e.repeats.0["until"]
        .as_i64()
        .filter(|u| *u > 0)
        .map(|u| {
            chrono::DateTime::from_timestamp(u, 0)
                .unwrap_or_default()
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default();
    ctx.render(
        "calendar_form.html",
        minijinja::context! { title => "Edit Event", cal => &cal, e => &e, date => fmt_d(e.starttime), enddate => if e.endtime > 0 { fmt_d(e.endtime) } else { String::new() },
            starttime => fmt_t(e.starttime), endtime => if e.endtime > 0 { fmt_t(e.endtime) } else { String::new() },
            repeat => e.repeats.0["type"].as_str().unwrap_or("none"), interval => e.repeats.0["interval"].as_i64().unwrap_or(1), until => until, form => minijinja::context!{ message => &e.description } },
    )
    .await
}

pub async fn edit_submit(
    ctx: Ctx,
    Path(eid): Path<i32>,
    CsrfForm(f): CsrfForm<EventForm>,
) -> AppResult<Response> {
    let e = load_event(&ctx, eid).await?;
    let perms = cal_perms(&ctx, e.cid).await?;
    if !(perms.moderate || (e.uid == ctx.uid() && ctx.uid() > 0)) {
        return Err(AppError::no_perm());
    }
    let (start, end, usingtime, repeats) = build_times(&ctx, &f)?;
    sqlx::query("UPDATE events SET name = $2, description = $3, private = $4, starttime = $5, endtime = $6, usingtime = $7, ignoretimezone = $8, repeats = $9, timezone = $10 WHERE eid = $1")
        .bind(eid)
        .bind(f.name.trim())
        .bind(f.message.trim())
        .bind(f.private)
        .bind(start)
        .bind(end)
        .bind(usingtime)
        .bind(!usingtime)
        .bind(repeats)
        .bind(ctx.tz.name())
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(
        &format!("/calendar/event/{eid}"),
        "The event has been updated.",
    ))
}

#[derive(Deserialize, Default)]
pub struct Empty {}

pub async fn delete(
    ctx: Ctx,
    Path(eid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    let e = load_event(&ctx, eid).await?;
    let perms = cal_perms(&ctx, e.cid).await?;
    if !(perms.moderate || (e.uid == ctx.uid() && ctx.uid() > 0)) {
        return Err(AppError::no_perm());
    }
    sqlx::query("DELETE FROM events WHERE eid = $1")
        .bind(eid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(
        &format!("/calendar/{}", e.cid),
        "The event has been deleted.",
    ))
}

pub async fn approve(
    ctx: Ctx,
    Path(eid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    let e = load_event(&ctx, eid).await?;
    let perms = cal_perms(&ctx, e.cid).await?;
    if !perms.moderate {
        return Err(AppError::no_perm());
    }
    sqlx::query("UPDATE events SET visible = NOT visible WHERE eid = $1")
        .bind(eid)
        .execute(&ctx.app.db)
        .await?;
    let _ = Redirect::to("/");
    Ok(ctx
        .redirect(
            &format!("/calendar/event/{eid}"),
            "The event's visibility has been updated.",
        )
        .into_response())
}
