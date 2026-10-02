//! Synthetic data generator for load testing (`rbb seed`). Uses set-based SQL so millions of
//! rows are generated inside Postgres in minutes. Secondary indexes on posts are dropped during
//! the bulk load and rebuilt afterwards.

use sqlx::PgPool;
use std::time::Instant;

const WORDS: &str = "the,quick,brown,fox,jumps,over,lazy,dog,forum,thread,post,reply,rust,server,database,postgres,performance,cache,index,query,\
community,member,moderator,admin,board,topic,discussion,question,answer,help,guide,tutorial,release,update,version,feature,bug,issue,fix,patch,\
design,theme,template,style,color,layout,mobile,desktop,browser,network,latency,throughput,memory,storage,disk,backup,restore,security,password,\
login,account,profile,signature,avatar,message,private,public,search,result,page,link,image,video,music,game,movie,book,science,history,travel,\
food,coffee,weather,summer,winter,holiday,weekend,project,team,idea,plan,review,opinion,agree,disagree,great,awesome,terrible,interesting,maybe";

pub async fn seed(db: &PgPool, users: i64, threads: i64, posts: i64) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let installed: Option<i32> = sqlx::query_scalar("SELECT gid FROM usergroups LIMIT 1")
        .fetch_optional(db)
        .await?;
    if installed.is_none() {
        crate::install::install(
            db,
            "admin",
            "admin12345",
            "admin@example.com",
            "rbb Load Test",
            "http://127.0.0.1:8080",
        )
        .await?;
    }
    let words: Vec<&str> = WORDS.split(',').collect();
    let mut conn = db.acquire().await?;
    sqlx::query("SET synchronous_commit = off")
        .execute(&mut *conn)
        .await?;
    sqlx::query("SET maintenance_work_mem = '512MB'")
        .execute(&mut *conn)
        .await?;
    sqlx::query("SET work_mem = '64MB'")
        .execute(&mut *conn)
        .await?;

    // Forums: 8 categories x 6 forums, plus a few subforums.
    println!("creating forums…");
    let base_order: i32 =
        sqlx::query_scalar("SELECT COALESCE(MAX(disporder), 0) FROM forums WHERE pid = 0")
            .fetch_one(&mut *conn)
            .await?;
    let mut forum_ids = vec![];
    for c in 0..8 {
        let cid: i32 = sqlx::query_scalar("INSERT INTO forums (name, description, type, pid, disporder) VALUES ($1, '', 'c', 0, $2) RETURNING fid")
            .bind(format!("Category {}", c + 1))
            .bind(base_order + c + 1)
            .fetch_one(&mut *conn)
            .await?;
        sqlx::query("UPDATE forums SET parentlist = ARRAY[fid] WHERE fid = $1")
            .bind(cid)
            .execute(&mut *conn)
            .await?;
        for f in 0..6 {
            let fid: i32 = sqlx::query_scalar("INSERT INTO forums (name, description, type, pid, disporder, parentlist) VALUES ($1, $2, 'f', $3, $4, ARRAY[$3]) RETURNING fid")
                .bind(format!("Forum {}.{}", c + 1, f + 1))
                .bind(format!("Discussion forum number {} in category {}", f + 1, c + 1))
                .bind(cid)
                .bind(f + 1)
                .fetch_one(&mut *conn)
                .await?;
            sqlx::query("UPDATE forums SET parentlist = parentlist || fid WHERE fid = $1")
                .bind(fid)
                .execute(&mut *conn)
                .await?;
            forum_ids.push(fid);
            if f < 2 {
                let sid: i32 = sqlx::query_scalar("INSERT INTO forums (name, type, pid, disporder, parentlist) VALUES ($1, 'f', $2, 1, ARRAY[$3, $2]) RETURNING fid")
                    .bind(format!("Subforum {}.{}.1", c + 1, f + 1))
                    .bind(fid)
                    .bind(cid)
                    .fetch_one(&mut *conn)
                    .await?;
                sqlx::query("UPDATE forums SET parentlist = parentlist || fid WHERE fid = $1")
                    .bind(sid)
                    .execute(&mut *conn)
                    .await?;
                forum_ids.push(sid);
            }
        }
    }
    // Skew: the first forum gets a big share of threads (a "hot" forum with a very deep thread list).
    let hot = forum_ids[0];

    println!("creating {users} users…");
    let hash = crate::auth::hash_password("password123")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let now = crate::util::now();
    sqlx::query(
        "INSERT INTO users (username, password, email, usergroup, regdate, lastactive, lastvisit, pmfolders, regip, lastip, signature)
         SELECT 'user' || g, $1, 'user' || g || '@example.com', 2, $2 - (random() * 3 * 365 * 86400)::bigint, $2 - (random() * 30 * 86400)::bigint, $2 - (random() * 30 * 86400)::bigint,
                '[]', ('10.' || (g % 250) || '.' || (g / 250 % 250) || '.1')::inet, '10.0.0.1'::inet, CASE WHEN g % 5 = 0 THEN '[i]Signature of user ' || g || '[/i]' ELSE '' END
         FROM generate_series(1, $3) g
         ON CONFLICT DO NOTHING",
    )
    .bind(&hash)
    .bind(now)
    .bind(users)
    .execute(&mut *conn)
    .await?;
    let (umin, umax): (i32, i32) =
        sqlx::query_as("SELECT MIN(uid), MAX(uid) FROM users WHERE username LIKE 'user%'")
            .fetch_one(&mut *conn)
            .await?;
    println!("  users done ({:.1}s)", t0.elapsed().as_secs_f64());

    println!("dropping secondary post indexes for bulk load…");
    for idx in [
        "posts_search",
        "posts_tid_dateline",
        "posts_uid_dateline",
        "posts_fid_dateline",
        "posts_dateline",
        "posts_visible_mod",
        "posts_ip",
    ] {
        sqlx::query(&format!("DROP INDEX IF EXISTS {idx}"))
            .execute(&mut *conn)
            .await?;
    }

    println!("creating {threads} threads…");
    sqlx::query(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible, sticky, views)
         SELECT CASE WHEN g % 4 = 0 THEN $1 ELSE ($2::int[])[1 + (g % cardinality($2::int[]))] END,
                initcap((SELECT string_agg(w[1 + floor(random() * cardinality(w))::int], ' ') FROM (SELECT $3::text[] AS w) x, generate_series(1, 4 + (g % 5)))) || ' #' || g,
                u.uid, u.username, d.t, d.t, 1, g % 997 = 0, (random() * 5000)::int
         FROM generate_series(1, $4) g
         CROSS JOIN LATERAL (SELECT $6 - (random() * 3 * 365 * 86400)::bigint + g * 0 AS t) d
         JOIN LATERAL (SELECT uid, username FROM users WHERE uid = $7 + (g * 7919) % ($8 - $7 + 1)) u ON TRUE",
    )
    .bind(hot)
    .bind(&forum_ids)
    .bind(&words)
    .bind(threads)
    .bind(0)
    .bind(now)
    .bind(umin)
    .bind(umax)
    .execute(&mut *conn)
    .await?;
    let (tmin, tmax): (i32, i32) =
        sqlx::query_as("SELECT MIN(tid), MAX(tid) FROM threads WHERE subject LIKE '% #%'")
            .fetch_one(&mut *conn)
            .await?;
    println!("  threads done ({:.1}s)", t0.elapsed().as_secs_f64());

    // Posts: each thread gets a first post; remaining posts are distributed with a long tail
    // (a few mega-threads with thousands of replies).
    let extra = (posts - threads).max(0);
    println!("creating {posts} posts…");
    sqlx::query(
        "INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible, ipaddress)
         SELECT t.tid, t.fid, t.subject, t.uid, t.username, t.dateline,
                (SELECT string_agg(w[1 + floor(random() * cardinality(w))::int], ' ') FROM (SELECT $1::text[] AS w) x, generate_series(1, 30 + (t.tid % 60))),
                1, ('10.1.' || (t.uid % 250) || '.' || (t.tid % 250))::inet
         FROM threads t WHERE t.tid BETWEEN $2 AND $3",
    )
    .bind(&words)
    .bind(tmin)
    .bind(tmax)
    .execute(&mut *conn)
    .await?;
    println!("  first posts done ({:.1}s)", t0.elapsed().as_secs_f64());
    let span = (tmax - tmin + 1) as i64;
    let batch = 500_000i64;
    let mut done = 0i64;
    while done < extra {
        let n = batch.min(extra - done);
        sqlx::query(
            "INSERT INTO posts (tid, fid, subject, uid, username, dateline, message, visible, ipaddress)
             SELECT t.tid, t.fid, 'RE: ' || t.subject, u.uid, u.username, LEAST($8, t.dateline + (r.g % 5000) * 600 + (random() * 3600)::bigint),
                    (SELECT string_agg(w[1 + floor(random() * cardinality(w))::int], ' ') FROM (SELECT $1::text[] AS w) x, generate_series(1, 10 + (r.g % 80)))
                    || CASE WHEN r.g % 50 = 0 THEN E'\n[quote]' || 'Earlier message quoted here' || E'[/quote]\nAgreed! :)' WHEN r.g % 30 = 0 THEN E'\n[b]Bold point[/b] and a link https://example.com/page/' || r.g ELSE '' END,
                    1, ('10.2.' || (u.uid % 250) || '.' || (r.g % 250))::inet
             FROM (SELECT g, $4 + (CASE WHEN g % 10 = 0 THEN (g / 10) % 20 ELSE floor(power(random(), 2) * $5)::int END) AS rtid,
                          $6 + (g * 104729) % ($7 - $6 + 1) AS ruid FROM generate_series($2, $3) g) r
             JOIN threads t ON t.tid = r.rtid
             JOIN users u ON u.uid = r.ruid",
        )
        .bind(&words)
        .bind(done + 1)
        .bind(done + n)
        .bind(tmin)
        .bind(span as i32)
        .bind(umin)
        .bind(umax)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        done += n;
        println!(
            "  {done}/{extra} replies ({:.1}s)",
            t0.elapsed().as_secs_f64()
        );
    }

    println!("rebuilding post indexes…");
    for sql in [
        "CREATE INDEX posts_tid_dateline ON posts (tid, dateline, pid)",
        "CREATE INDEX posts_uid_dateline ON posts (uid, dateline DESC)",
        "CREATE INDEX posts_fid_dateline ON posts (fid, dateline DESC)",
        "CREATE INDEX posts_dateline ON posts (dateline DESC)",
        "CREATE INDEX posts_visible_mod ON posts (visible, fid) WHERE visible <> 1",
        "CREATE INDEX posts_ip ON posts (ipaddress)",
        "CREATE INDEX posts_search ON posts USING gin (search_tsv)",
    ] {
        let t = Instant::now();
        sqlx::query(sql).execute(&mut *conn).await?;
        println!(
            "  {} ({:.1}s)",
            sql.split(" ON ").next().unwrap_or(sql),
            t.elapsed().as_secs_f64()
        );
    }

    println!("computing thread counters…");
    sqlx::query(
        "UPDATE threads t SET replies = s.c - 1, firstpost = s.fp, lastpost = s.lp,
            lastposter = (SELECT username FROM posts WHERE pid = s.lpid), lastposteruid = (SELECT uid FROM posts WHERE pid = s.lpid)
         FROM (SELECT tid, COUNT(*) c, MIN(pid) fp, MAX(dateline) lp, (array_agg(pid ORDER BY dateline DESC, pid DESC))[1] lpid FROM posts WHERE tid BETWEEN $1 AND $2 GROUP BY tid) s
         WHERE t.tid = s.tid",
    )
    .bind(tmin)
    .bind(tmax)
    .execute(&mut *conn)
    .await?;
    println!("computing forum and user counters…");
    let app_less = SeedApp { db: db.clone() };
    app_less.rebuild().await?;
    sqlx::query("ANALYZE").execute(&mut *conn).await?;
    println!("seed complete in {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}

struct SeedApp {
    db: PgPool,
}

impl SeedApp {
    async fn rebuild(&self) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE forums f SET threads = COALESCE(s.threads, 0), posts = COALESCE(s.posts, 0)
             FROM (SELECT fx.fid, COUNT(t.tid) FILTER (WHERE t.visible = 1) AS threads, COALESCE(SUM(t.replies + 1) FILTER (WHERE t.visible = 1), 0) AS posts
                   FROM forums fx LEFT JOIN threads t ON t.fid = fx.fid GROUP BY fx.fid) s WHERE f.fid = s.fid",
        )
        .execute(&self.db)
        .await?;
        sqlx::query(
            "UPDATE forums f SET lastpost = x.lastpost, lastposter = x.lastposter, lastposteruid = x.lastposteruid, lastposttid = x.tid, lastpostsubject = x.subject
             FROM (SELECT DISTINCT ON (fid) fid, lastpost, lastposter, lastposteruid, tid, subject FROM threads WHERE visible = 1 ORDER BY fid, lastpost DESC) x WHERE f.fid = x.fid",
        )
        .execute(&self.db)
        .await?;
        sqlx::query("UPDATE users u SET postnum = s.c, lastpost = s.lp FROM (SELECT uid, COUNT(*) c, MAX(dateline) lp FROM posts GROUP BY uid) s WHERE u.uid = s.uid").execute(&self.db).await?;
        sqlx::query("UPDATE users u SET threadnum = s.c FROM (SELECT uid, COUNT(*) c FROM threads GROUP BY uid) s WHERE u.uid = s.uid").execute(&self.db).await?;
        sqlx::query("UPDATE counters SET numusers = (SELECT COUNT(*) FROM users), lastuid = (SELECT MAX(uid) FROM users WHERE NOT is_system)").execute(&self.db).await?;
        sqlx::query("UPDATE counters SET lastusername = (SELECT username FROM users WHERE uid = counters.lastuid)").execute(&self.db).await?;
        Ok(())
    }
}
