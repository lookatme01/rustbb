#!/usr/bin/env python3
"""Compare historical queries with the optimized queries and migration 0031.

Creates only a generated rbb_query_bench_* database, applies migrations 0001–0030,
loads deterministic synthetic data, then applies 0031. Read and write plans use
EXPLAIN ANALYZE inside rolled-back transactions. Drops its database on completion.
Results are medians of five executions after one warm-up execution per query.
"""
import argparse
import json
import subprocess
import uuid
import re
from urllib.parse import urlsplit, urlunsplit
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(
    description="Benchmark SQL in a disposable PostgreSQL database (requires psql and CREATE DATABASE)."
)
parser.add_argument("--server", default="postgres://rbb@127.0.0.1:5433/postgres")
parser.add_argument("--output", type=Path, default=Path("sql-benchmark.json"))
args = parser.parse_args()
server = urlsplit(args.server)
if server.scheme not in ("postgres", "postgresql"):
    parser.error("--server must be a PostgreSQL connection URL")
NAME = "rbb_query_bench_" + uuid.uuid4().hex[:12]
BASE = ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"]


def sql(q, db=NAME):
    connection = urlunsplit(server._replace(path="/" + db))
    p = subprocess.run(
        BASE + ["-d", connection], input=q, text=True, capture_output=True
    )
    if p.returncode:
        raise RuntimeError(p.stderr)
    return p.stdout


def plan(q):
    p = json.loads(
        sql(
            "BEGIN; EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) "
            + q
            + "; ROLLBACK;"
        )
    )[0]

    def nodes(n):
        yield {
            k: n[k]
            for k in (
                "Node Type",
                "Index Name",
                "Actual Rows",
                "Actual Loops",
                "Rows Removed by Filter",
                "Shared Hit Blocks",
                "Shared Read Blocks",
            )
            if k in n
        }
        for child in n.get("Plans", []):
            yield from nodes(child)

    return {"ms": p["Execution Time"], "nodes": list(nodes(p["Plan"]))}


def median(q):
    plan(q)  # warm the statement and its pages before collecting samples
    results = [plan(q) for _ in range(5)]
    return sorted(results, key=lambda x: x["ms"])[len(results) // 2]


def recount(source):
    source = source[source.index("pub async fn rebuild_user_counters") :]
    return re.search(r'sqlx::query\(\s*"(.*?)"', source, re.S).group(1)


sql("CREATE DATABASE " + NAME, "postgres")
try:
    migrations = sorted((ROOT / "migrations").glob("*.sql"))
    for path in migrations:
        if int(path.name.split("_")[0]) > 30:
            continue
        sql("BEGIN;" + path.read_text() + ";COMMIT;")
    sql(
        """
 INSERT INTO usergroups(gid,title) VALUES (2,'Members'),(6,'Moderators');
 INSERT INTO forums(fid,name) SELECT g,'Forum '||g FROM generate_series(1,20) g;
 INSERT INTO users(uid,username,password,email,usergroup,additionalgroups,regdate)
 SELECT g,'User'||g,'','u'||g||'@example.test',2, CASE WHEN g%100=0 THEN ARRAY[6] ELSE '{}'::int[] END,1 FROM generate_series(1,10000) g;
 INSERT INTO threads(tid,fid,subject,uid,dateline,lastpost,sticky)
 SELECT g,1+g%20,CASE WHEN g%1000=0 THEN 'Zebracorn rare title '||g ELSE 'Ordinary discussion '||g END,1+g%10000,g,g,g%23=0
 FROM generate_series(1,100000) g;
 INSERT INTO posts(tid,fid,uid,username,dateline,visible,message)
 SELECT 1+g%100000,1+(1+g%100000)%20,1+g%10000,'User'||(1+g%10000),g,CASE WHEN g%11=0 THEN 0 ELSE 1 END,'Benchmark message'
 FROM generate_series(1,200000) g;
 INSERT INTO posts(tid,fid,uid,username,dateline,visible,message)
 SELECT 1,2,1,'User1',g,CASE WHEN g%2=0 THEN 0 ELSE 1 END,'Mega thread message' FROM generate_series(1,100000) g;
 VACUUM ANALYZE;
 """
    )
    old = """pub async fn rebuild_user_counters sqlx::query(
 "UPDATE users u SET postnum = COALESCE(s.c, 0) FROM (SELECT u2.uid, (SELECT COUNT(*) FROM posts p JOIN threads t ON t.tid = p.tid JOIN forums f ON f.fid = p.fid WHERE p.uid = u2.uid AND p.visible = 1 AND t.visible = 1 AND f.usepostcounts) AS c FROM users u2) s WHERE u.uid = s.uid"
 )"""
    new = (ROOT / "src/ops.rs").read_text()
    pairs = {
        "title_search": (
            "SELECT tid FROM threads WHERE subject ILIKE ALL(ARRAY['%Zebracorn%','%rare%']) ORDER BY lastpost DESC LIMIT 50",
            "SELECT tid FROM threads WHERE subject ILIKE (ARRAY['%Zebracorn%','%rare%']::text[])[1] AND subject ILIKE (ARRAY['%Zebracorn%','%rare%']::text[])[2] ORDER BY lastpost DESC LIMIT 50",
        ),
        "feed_forum": (
            "SELECT tid,subject,dateline FROM threads WHERE fid=2 AND visible=1 AND closed NOT LIKE 'moved|%' ORDER BY dateline DESC LIMIT 20",
        )
        * 2,
        "last_visible_post": (
            "SELECT pid,uid,dateline FROM posts WHERE tid=1 AND visible=1 ORDER BY dateline DESC,pid DESC LIMIT 1",
        )
        * 2,
        "visibility_counts": (
            "SELECT COUNT(*) FILTER(WHERE visible=1),COUNT(*) FILTER(WHERE visible=0),COUNT(*) FILTER(WHERE visible=-1) FROM posts WHERE tid=1",
        )
        * 2,
        "cursor_boundary": (
            "SELECT COUNT(*) FROM posts WHERE tid=1 AND (dateline<50000 OR (dateline=50000 AND pid<=250000))",
            "SELECT COUNT(*) FROM posts WHERE tid=1 AND (dateline,pid)<=(50000,250000)",
        ),
        "group_membership": (
            "SELECT uid FROM users WHERE usergroup=6 OR 6=ANY(additionalgroups)",
            "SELECT uid FROM users WHERE usergroup=6 OR additionalgroups @> ARRAY[6]",
        ),
        "rebuild_user_posts": (recount(old), recount(new)),
    }
    results = {}
    for key, (before, after) in pairs.items():
        print("baseline " + key, flush=True)
        results[key] = {"before": median(before)}
    sql((ROOT / "migrations/0031_query_performance.sql").read_text())
    sql("VACUUM ANALYZE;")
    for key, (before, after) in pairs.items():
        print("optimized " + key, flush=True)
        results[key]["after"] = median(after)
        results[key]["speedup"] = round(
            results[key]["before"]["ms"] / results[key]["after"]["ms"], 2
        )
    args.output.write_text(
        json.dumps(
            {
                "server": sql("SHOW server_version").strip(),
                "fixture": {"users": 10000, "threads": 100000, "posts": 300000},
                "runs": 5,
                "results": results,
            },
            indent=2,
        )
    )
    for key, v in results.items():
        print(key, v["before"]["ms"], v["after"]["ms"], v["speedup"])
finally:
    sql("DROP DATABASE " + NAME + " WITH (FORCE)", "postgres")
