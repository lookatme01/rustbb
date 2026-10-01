//! First-time installation: default groups, forums, theme, smilies, and the admin account.

use crate::perms::GroupPerms;
use crate::util::now;
use sqlx::PgPool;

pub struct TaskDef {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub interval: i32,
}

pub static TASKS: &[TaskDef] = &[
    TaskDef {
        key: "hourlycleanup",
        title: "Hourly Cleanup",
        description: "Removes expired sessions, captchas, search results and old activation codes.",
        interval: 3600,
    },
    TaskDef {
        key: "dailycleanup",
        title: "Daily Cleanup",
        description: "Prunes old read markers, logs and stale data.",
        interval: 86400,
    },
    TaskDef {
        key: "delayedmoderation",
        title: "Delayed Moderation",
        description: "Performs scheduled moderation actions.",
        interval: 300,
    },
    TaskDef {
        key: "promotions",
        title: "User Promotions",
        description: "Runs usergroup promotions.",
        interval: 1200,
    },
    TaskDef {
        key: "banlifter",
        title: "Ban Lifter",
        description: "Lifts expired bans and suspensions.",
        interval: 300,
    },
    TaskDef {
        key: "warnings",
        title: "Warning Expiry",
        description: "Expires warnings and recalculates warning levels.",
        interval: 900,
    },
    TaskDef {
        key: "threadviews",
        title: "Redirect Cleanup",
        description: "Removes expired thread redirects.",
        interval: 3600,
    },
    TaskDef {
        key: "dailystats",
        title: "Daily Statistics",
        description: "Records daily board statistics for the Admin CP.",
        interval: 86400,
    },
    TaskDef {
        key: "massmail",
        title: "Mass Mail",
        description: "Sends queued mass mailings in batches.",
        interval: 60,
    },
    TaskDef {
        key: "userpruning",
        title: "User Pruning",
        description: "Removes users who never activated their account after 30 days.",
        interval: 86400,
    },
    TaskDef {
        key: "systemautoclose",
        title: "Close Inactive Threads",
        description: "System closes threads that have been inactive for the configured number of days.",
        interval: 3600,
    },
    TaskDef {
        key: "recyclebin",
        title: "Soft-delete Pruning",
        description: "Hard deletes soft-deleted content older than 90 days (if enabled).",
        interval: 86400,
    },
];

fn perms_json(p: GroupPerms) -> serde_json::Value {
    serde_json::to_value(p).unwrap()
}

pub async fn install(
    db: &PgPool,
    admin: &str,
    password: &str,
    email: &str,
    bbname: &str,
    bburl: &str,
) -> anyhow::Result<()> {
    let exists: Option<i32> = sqlx::query_scalar("SELECT gid FROM usergroups LIMIT 1")
        .fetch_optional(db)
        .await?;
    if exists.is_some() {
        anyhow::bail!("the board is already installed");
    }
    let t = now();
    let mut tx = db.begin().await?;
    let groups: Vec<(i32, i16, &str, &str, &str, &str, i16, bool, GroupPerms)> = vec![
        (
            1,
            1,
            "Guests",
            "The default group that all visitors are assigned to unless they're logged in.",
            "{username}",
            "Unregistered",
            0,
            false,
            GroupPerms::guest(),
        ),
        (
            2,
            1,
            "Registered",
            "After registration, all users are placed in this group by default.",
            "{username}",
            "",
            0,
            false,
            GroupPerms::default(),
        ),
        (
            3,
            1,
            "Super Moderators",
            "These users can moderate any forum.",
            "<span style=\"color: #CC00CC;\"><strong>{username}</strong></span>",
            "Super Moderator",
            6,
            false,
            GroupPerms::super_moderator(),
        ),
        (
            4,
            1,
            "Administrators",
            "The group all administrators belong to.",
            "<span style=\"color: #1e8e3e;\"><strong><em>{username}</em></strong></span>",
            "Administrator",
            7,
            false,
            GroupPerms::administrator(),
        ),
        (
            5,
            1,
            "Awaiting Activation",
            "Users who have not activated their account yet.",
            "{username}",
            "Account not Activated",
            0,
            false,
            GroupPerms::awaiting_activation(),
        ),
        (
            6,
            1,
            "Moderators",
            "These users moderate specific forums.",
            "<span style=\"color: #CC00CC;\"><strong>{username}</strong></span>",
            "Moderator",
            5,
            false,
            GroupPerms::moderator(),
        ),
        (
            7,
            1,
            "Banned",
            "The default user group to which members that are banned are moved.",
            "<s>{username}</s>",
            "Banned",
            0,
            true,
            GroupPerms::banned(),
        ),
    ];
    for (gid, ty, title, desc, style, utitle, stars, banned, perms) in groups {
        sqlx::query(
            "INSERT INTO usergroups (gid, type, title, description, namestyle, usertitle, stars, starimage, disporder, isbannedgroup, perms)
             VALUES ($1, $2, $3, $4, $5, $6, $7, '/static/images/star.svg', $1, $8, $9)",
        )
        .bind(gid)
        .bind(ty)
        .bind(title)
        .bind(desc)
        .bind(style)
        .bind(utitle)
        .bind(stars)
        .bind(banned)
        .bind(perms_json(perms))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("SELECT setval('usergroups_gid_seq', 7)")
        .execute(&mut *tx)
        .await?;

    for (posts, title, stars) in [
        (0, "Newbie", 1),
        (1, "Junior Member", 2),
        (50, "Member", 3),
        (250, "Senior Member", 4),
        (500, "Posting Freak", 5),
    ] {
        sqlx::query("INSERT INTO usertitles (posts, title, stars, starimage) VALUES ($1, $2, $3, '/static/images/star.svg')")
            .bind(posts)
            .bind(title)
            .bind(stars as i16)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("INSERT INTO themes (tid, name, pid, def, properties, stylesheet) VALUES (1, 'Default', 0, TRUE, $1, '')")
        .bind(serde_json::json!({"logo": "/static/images/logo.svg", "colormode": "auto"}))
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO themes (tid, name, pid, def, properties, stylesheet) VALUES (2, 'Midnight', 1, FALSE, $1, $2)")
        .bind(serde_json::json!({"logo": "/static/images/logo.svg", "colormode": "dark", "brand": MIDNIGHT_BRAND}))
        .bind(MIDNIGHT_CSS)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT setval('themes_tid_seq', 2)")
        .execute(&mut *tx)
        .await?;

    // Forums: category + forums, MyBB style.
    let forums: Vec<(i32, &str, &str, &str, i32, Vec<i32>, i32)> = vec![
        (1, "General", "", "c", 0, vec![1], 1),
        (
            2,
            "Announcements",
            "News and announcements from the staff.",
            "f",
            1,
            vec![1, 2],
            1,
        ),
        (
            3,
            "General Discussion",
            "Talk about anything related to the community.",
            "f",
            1,
            vec![1, 3],
            2,
        ),
        (
            4,
            "Introductions",
            "New here? Say hello!",
            "f",
            1,
            vec![1, 4],
            3,
        ),
        (5, "Off Topic", "", "c", 0, vec![5], 2),
        (6, "The Lounge", "Everything else.", "f", 5, vec![5, 6], 1),
    ];
    for (fid, name, desc, ty, pid, parents, order) in forums {
        sqlx::query("INSERT INTO forums (fid, name, description, type, pid, parentlist, disporder) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(fid)
            .bind(name)
            .bind(desc)
            .bind(ty)
            .bind(pid)
            .bind(&parents)
            .bind(order)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("SELECT setval('forums_fid_seq', 6)")
        .execute(&mut *tx)
        .await?;
    // Only staff may start threads in Announcements.
    let mut ann = crate::perms::ForumPerms::from_group(&GroupPerms::default());
    ann.canpostthreads = false;
    sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES (2, 2, $1)")
        .bind(serde_json::to_value(&ann)?)
        .execute(&mut *tx)
        .await?;
    let mut ann_guest = crate::perms::ForumPerms::from_group(&GroupPerms::guest());
    ann_guest.canpostthreads = false;
    sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES (2, 1, $1)")
        .bind(serde_json::to_value(&ann_guest)?)
        .execute(&mut *tx)
        .await?;

    let smilies = [
        ("Smile", ":)\n:-)", "smile"),
        ("Wink", ";)\n;-)", "wink"),
        ("Big Grin", ":D\n:-D", "biggrin"),
        ("Tongue", ":P\n:-P\n:p", "tongue"),
        ("Sad", ":(\n:-(", "sad"),
        ("Cool", ":cool:", "cool"),
        ("Angry", ":@\n:angry:", "angry"),
        ("Confused", ":s\n:S\n:confused:", "confused"),
        ("Cry", ":cry:\n:'(", "cry"),
        ("Blush", ":blush:", "blush"),
        ("Huh", ":huh:", "huh"),
        ("Shocked", ":O\n:o", "shocked"),
        ("Rolleyes", ":rolleyes:", "rolleyes"),
        ("Heart", ":heart:\n<3", "heart"),
        ("Idea", ":idea:", "idea"),
        ("Undecided", ":-/\n:undecided:", "undecided"),
    ];
    for (i, (name, find, img)) in smilies.iter().enumerate() {
        sqlx::query("INSERT INTO smilies (name, find, image, disporder, showclickable) VALUES ($1, $2, $3, $4, TRUE)")
            .bind(name)
            .bind(find)
            .bind(format!("/static/smilies/{img}.svg"))
            .bind(i as i32 + 1)
            .execute(&mut *tx)
            .await?;
    }
    for (name, file) in [
        ("Information", "information"),
        ("Exclamation", "exclamation"),
        ("Question", "question"),
        ("Lightbulb", "lightbulb"),
        ("Thumbs Up", "thumbsup"),
        ("Thumbs Down", "thumbsdown"),
        ("Star", "star"),
        ("Check", "check"),
        ("Fire", "fire"),
        ("Heart", "heart"),
    ] {
        sqlx::query("INSERT INTO icons (name, path) VALUES ($1, $2)")
            .bind(name)
            .bind(format!("/static/icons/{file}.svg"))
            .execute(&mut *tx)
            .await?;
    }
    for (name, mime, ext, size) in [
        ("ZIP File", "application/zip", "zip", 10240),
        ("JPEG Image", "image/jpeg", "jpg", 5120),
        ("JPEG Image", "image/jpeg", "jpeg", 5120),
        ("PNG Image", "image/png", "png", 5120),
        ("GIF Image", "image/gif", "gif", 5120),
        ("WebP Image", "image/webp", "webp", 5120),
        ("PDF Document", "application/pdf", "pdf", 10240),
        ("Text Document", "text/plain", "txt", 1024),
        (
            "Word Document",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
            5120,
        ),
        ("Gzip Archive", "application/gzip", "gz", 10240),
        ("7-Zip Archive", "application/x-7z-compressed", "7z", 10240),
        ("MP4 Video", "video/mp4", "mp4", 51200),
        ("MP3 Audio", "audio/mpeg", "mp3", 20480),
    ] {
        sqlx::query("INSERT INTO attachtypes (name, mimetype, extension, maxsize, avatarfile) VALUES ($1, $2, $3, $4, $5)")
            .bind(name)
            .bind(mime)
            .bind(ext)
            .bind(size)
            .bind(matches!(ext, "jpg" | "jpeg" | "png" | "gif" | "webp"))
            .execute(&mut *tx)
            .await?;
    }
    for (i, (title, extra)) in [
        ("Rules Violation", false),
        ("Inappropriate Content", false),
        ("Spam", false),
        ("Other", true),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO reportreasons (title, appliesto, extra, disporder) VALUES ($1, 'all', $2, $3)")
            .bind(title)
            .bind(extra)
            .bind(i as i32 + 1)
            .execute(&mut *tx)
            .await?;
    }
    for (title, points, days) in [
        ("Spam", 2, 14),
        ("Offensive Language", 1, 7),
        ("Flaming / Personal Attacks", 2, 30),
        ("Off-topic Posting", 1, 7),
    ] {
        sqlx::query("INSERT INTO warningtypes (title, points, expirationtime) VALUES ($1, $2, $3)")
            .bind(title)
            .bind(points)
            .bind(days as i64 * 86400)
            .execute(&mut *tx)
            .await?;
    }
    for (pct, action) in [
        (
            50,
            serde_json::json!({"type": "moderate", "length": 7 * 86400}),
        ),
        (
            80,
            serde_json::json!({"type": "suspend", "length": 7 * 86400}),
        ),
        (
            100,
            serde_json::json!({"type": "ban", "length": 30 * 86400, "usergroup": 7}),
        ),
    ] {
        sqlx::query("INSERT INTO warninglevels (percentage, action) VALUES ($1, $2)")
            .bind(pct)
            .bind(action)
            .execute(&mut *tx)
            .await?;
    }
    for (i, (name, ty, opts)) in [
        ("Location", "text", ""),
        ("Bio", "textarea", ""),
        ("Sex", "select", "Undisclosed\nMale\nFemale\nOther"),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO profilefields (name, description, disporder, type, options, maxlength) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(name)
            .bind(match *name {
                "Location" => "Where in the world do you live?",
                "Bio" => "Enter a few short details about yourself, your life story etc.",
                _ => "Please select your sex from the list below.",
            })
            .bind(i as i32 + 1)
            .bind(ty)
            .bind(opts)
            .bind(if *ty == "textarea" { 1000 } else { 255 })
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO questions (question, answer) VALUES ('What is two plus three?', '5\nfive')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO calendars (name, disporder) VALUES ('Default Calendar', 1)")
        .execute(&mut *tx)
        .await?;

    let help: &[(&str, &str, &[(&str, &str, &str)])] = &[
        (
            "User Maintenance",
            "Basic instructions for maintaining a forum account.",
            &[
                (
                    "Registration",
                    "Why should I register?",
                    "By registering you can post threads and replies, send private messages, subscribe to threads and forums, receive alerts, customise your profile and much more. Registration is free.\n\nTo register, click [b]Register[/b] at the top of any page.",
                ),
                (
                    "Updating Profile",
                    "How do I change my profile?",
                    "Visit the [url=/usercp]User Control Panel[/url] to edit your profile, signature, avatar and options.",
                ),
                (
                    "Two-Factor Authentication",
                    "How do I secure my account?",
                    "In the User CP, open [b]Security[/b] to enable two-factor authentication with any TOTP authenticator app.",
                ),
            ],
        ),
        (
            "Using the Board",
            "Posting, MyCode, smilies and more.",
            &[
                (
                    "Posting",
                    "How do I post?",
                    "Open a forum and click [b]New Thread[/b], or open a thread and use [b]Post Reply[/b] or the quick reply box.",
                ),
                (
                    "MyCode",
                    "How do I format posts?",
                    "Posts use MyCode (BBCode). See the [url=/mycode]MyCode reference[/url] for all supported tags.",
                ),
                (
                    "Mentions & Alerts",
                    "How do notifications work?",
                    "Mention someone with @username (or @\"Name With Spaces\") and they will receive an alert. You are also alerted when someone quotes you, replies to a thread you subscribe to, sends you a private message or gives you reputation.",
                ),
            ],
        ),
    ];
    for (i, (name, desc, docs)) in help.iter().enumerate() {
        let sid: i32 = sqlx::query_scalar("INSERT INTO helpsections (name, description, disporder) VALUES ($1, $2, $3) RETURNING sid")
            .bind(name)
            .bind(desc)
            .bind(i as i32 + 1)
            .fetch_one(&mut *tx)
            .await?;
        for (j, (dname, ddesc, doc)) in docs.iter().enumerate() {
            sqlx::query("INSERT INTO helpdocs (sid, name, description, document, disporder) VALUES ($1, $2, $3, $4, $5)")
                .bind(sid)
                .bind(dname)
                .bind(ddesc)
                .bind(doc)
                .bind(j as i32 + 1)
                .execute(&mut *tx)
                .await?;
        }
    }

    for (k, v) in [
        ("bbname", bbname),
        ("bburl", bburl.trim_end_matches('/')),
        ("adminemail", email),
        ("parser_rev", "1"),
    ] {
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = $2").bind(k).bind(v).execute(&mut *tx).await?;
    }

    let hash = crate::auth::hash_password(password)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let uid: i32 = sqlx::query_scalar(
        "INSERT INTO users (username, password, email, usergroup, regdate, lastactive, lastvisit, pmfolders) VALUES ($1, $2, $3, 4, $4, $4, $4, '[]') RETURNING uid",
    )
    .bind(admin)
    .bind(hash)
    .bind(email)
    .bind(t)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE counters SET numusers = 1, lastuid = $1, lastusername = $2 WHERE id = 1")
        .bind(uid)
        .bind(admin)
        .execute(&mut *tx)
        .await?;

    // Welcome thread.
    let msg = format!(
        "[b]Welcome to {bbname}![/b]\n\nThis board is powered by [url=https://github.com/]rbb[/url], a fast forum engine written in Rust.\n\nLog in to the [url=/admin]Admin CP[/url] to configure your board. :)"
    );
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, lastposter, lastposteruid, visible) VALUES (2, 'Welcome to your new board', $1, $2, $3, $3, $2, $1, 1) RETURNING tid",
    )
    .bind(uid)
    .bind(admin)
    .bind(t)
    .fetch_one(&mut *tx)
    .await?;
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible) VALUES ($1, 2, 'Welcome to your new board', $2, $3, $4, $5, 1) RETURNING pid",
    )
    .bind(tid)
    .bind(uid)
    .bind(admin)
    .bind(t)
    .bind(&msg)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE threads SET firstpost = $2 WHERE tid = $1")
        .bind(tid)
        .bind(pid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE forums SET threads = 1, posts = 1, lastpost = $1, lastposter = $2, lastposteruid = $3, lastposttid = $4, lastpostsubject = 'Welcome to your new board' WHERE fid = 2")
        .bind(t)
        .bind(admin)
        .bind(uid)
        .bind(tid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET postnum = 1, threadnum = 1, lastpost = $2 WHERE uid = $1")
        .bind(uid)
        .bind(t)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    upgrade(db).await?;
    Ok(())
}

/// Idempotent data upgrades run at every startup.
/// Midnight: the default theme in dark mode on deep navy, with a violet brand colour.
pub const MIDNIGHT_CSS: &str = ":root, :root[data-colormode] {\n  --canvas: #070914; --surface: #0e1122; --surface-2: #12162b; --surface-3: #1a1f3a;\n}\n";
pub const MIDNIGHT_BRAND: &str = "#a78bfa";
/// Midnight's stylesheet before the Halo theme (0.5), which fixed the accent colour in CSS.
const MIDNIGHT_CSS_05: &str = ":root, :root[data-colormode] {\n  --canvas: #080b16; --surface: #0f1326; --surface-2: #13182f; --surface-3: #1a2040;\n  --signal: #a78bfa; --signal-strong: #c4b5fd; --signal-wash: #221b45; --link: #c4b5fd;\n}\n";

pub async fn upgrade(db: &PgPool) -> anyhow::Result<()> {
    crate::system::ensure(db).await?;
    // 0.6 (Halo): an unedited Midnight takes its accent from the brand colour instead of CSS.
    sqlx::query("UPDATE themes SET stylesheet = $1, properties = properties || jsonb_build_object('brand', $2::text) WHERE tid = 2 AND name = 'Midnight' AND stylesheet = $3")
        .bind(MIDNIGHT_CSS)
        .bind(MIDNIGHT_BRAND)
        .bind(MIDNIGHT_CSS_05)
        .execute(db)
        .await?;
    // 0.5: Midnight moves to the Relay tokens (only if the admin never edited it).
    sqlx::query("UPDATE themes SET stylesheet = $1 WHERE tid = 2 AND name = 'Midnight' AND stylesheet = ':root { --accent: #7c9cff; --accent-strong: #5b7cfa; }\n'")
        .bind(MIDNIGHT_CSS)
        .execute(db)
        .await?;
    for t in TASKS {
        sqlx::query("INSERT INTO tasks (key, title, description, interval_secs, nextrun) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (key) DO NOTHING")
            .bind(t.key)
            .bind(t.title)
            .bind(t.description)
            .bind(t.interval)
            .bind(now() + 60)
            .execute(db)
            .await?;
    }
    Ok(())
}
