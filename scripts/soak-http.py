#!/usr/bin/env python3
"""Concurrent soak of a running board, to be followed by `rbb check`.

    DATABASE_URL=postgres://.../scratchdb scripts/soak-http.py PORT_FILE SECONDS

Logs in 24 members and 3 administrators and has them post, reply, edit, soft-delete, restore,
close, stick, approve, move, merge, split and delete at random against live ids, all at once.
It prints the response classes per operation. Afterwards, check the server log for
"deadlock detected" and run `rbb check` against the same database: every denormalized counter
(replies, post counts, last-post fields) must still match the data, and no 5xx should be a
deadlock.

It needs a throwaway board that you installed and seeded yourself (see scripts/fuzz-http.py for
how), users `user1`..`user24` whose password equals the administrator's (copy the hash with SQL),
`admin`/`admin12345`, and ratelimit_requests/postfloodcheck/postmergemins/searchfloodtime set to 0.
PORT_FILE is a file containing the port, so a script can pick a free one. It uses psql and
DATABASE_URL to pick random ids and to clear the login throttle, so never point it at a real board.
"""
import importlib.util, os, random, re, subprocess, sys, threading, time, collections
spec = importlib.util.spec_from_file_location("fz", "scripts/fuzz-http.py"); fz = importlib.util.module_from_spec(spec); spec.loader.exec_module(fz)
PORT = open(sys.argv[1]).read().strip(); BASE = f"http://127.0.0.1:{PORT}"; DURATION = int(sys.argv[2])
import shutil
PSQL = os.environ.get("PSQL") or shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"; DB = os.environ["DATABASE_URL"]
def q(sql): return [r for r in subprocess.run([PSQL, DB, "-Atc", sql], capture_output=True, text=True).stdout.split("\n") if r]
ids = {"tids": [], "pids": [], "fids": []}
def refresh():
    ids["tids"] = [int(x) for x in q("select tid from threads where closed not like 'moved|%' order by random() limit 300")]
    ids["pids"] = [int(x) for x in q("select pid from posts order by random() limit 300")]
    ids["pt"] = [tuple(int(y) for y in x.split("|")) for x in q("select pid, tid from posts order by random() limit 300")]
    ids["fids"] = [int(x) for x in q("select fid from forums where type='f'")]
    ids["polls"] = [int(x) for x in q("select tid from polls")]
    ids["uids"] = [tuple(x.split("|")) for x in q("select uid, username from users where username ~ '^user(2[5-9]|[3-5][0-9])$'")]
    ids["banned"] = [int(x) for x in q("select uid from banned")]
def prepare():
    # Features beyond posting: warnings on, and some polls to vote in.
    q("insert into settings (name,value) values ('enablewarningsystem','1'),('allowcustomwarnings','1'),('allowwarningsnopost','1'),('maxwarningpoints','10'),('enablepms','1') on conflict (name) do update set value=excluded.value")
    q("insert into polls (tid, question, dateline, options, votes, numvotes) select tid, 'Soak poll', 1, '{A,B,C}', '{0,0,0}', 0 from threads where poll = 0 and closed not like 'moved|%' order by random() limit 20")
    q("update threads t set poll = p.pid from polls p where p.tid = t.tid and t.poll = 0")
prepare()
refresh()
def mk(name, admin=False):
    c = fz.Client(BASE)
    _, b, _ = c.req("/member/login")
    c.form("/member/login", {"my_post_key": fz.key(b), "username": name, "password": "admin12345"})
    if admin:
        _, b, _ = c.req("/admin/verify"); c.form("/admin/verify", {"my_post_key": fz.key(b), "password": "admin12345"})
    return c
def reset_throttle(): q("delete from ratelimits")
members, admins = [], []
for i in range(1, 25):
    if i % 10 == 1: reset_throttle()
    members.append(mk(f"user{i}"))
for _ in range(3):
    reset_throttle(); admins.append(mk("admin", True))
for c in members + admins:
    assert c.req("/usercp")[0] == 200, "a client is not signed in"
print("all", len(members) + len(admins), "clients signed in")
stats = collections.Counter(); errors = collections.Counter(); stop = time.time() + DURATION
def key_of(c, path):
    _, b, _ = c.req(path)
    m = re.search(r'name="csrf-token" content="([^"]+)"', b)
    return fz.key(b) or (m.group(1) if m else "")
def member_op(c):
    op = random.choice(["reply", "reply", "reply", "thread", "pm", "rate", "subscribe", "vote", "rep"])
    if op == "pm":
        k = key_of(c, "/pm/send")
        code, _, _ = c.form("/pm/send", {"my_post_key": k, "to": f"user{random.randint(1, 60)}", "subject": "soak pm", "message": "soak private message text", "savecopy": "1"})
    elif op == "rate":
        tid = random.choice(ids["tids"]); k = key_of(c, f"/thread/{tid}")
        code, _, _ = c.form(f"/thread/{tid}/rate", {"my_post_key": k, "rating": random.randint(1, 5)})
    elif op == "subscribe":
        tid = random.choice(ids["tids"]); k = key_of(c, f"/thread/{tid}")
        d = {"my_post_key": k, "notification": random.choice([0, 1])}
        if random.random() < 0.4: d["unsubscribe"] = "1"
        code, _, _ = c.form(f"/thread/{tid}/subscribe", d)
    elif op == "vote":
        tid = random.choice(ids["polls"]); k = key_of(c, f"/thread/{tid}")
        if random.random() < 0.3: code, _, _ = c.form(f"/thread/{tid}/poll/undo", {"my_post_key": k})
        else: code, _, _ = c.form(f"/thread/{tid}/poll/vote", {"my_post_key": k, "option": random.randint(1, 3)})
    elif op == "rep":
        uid = random.randint(25, 60); k = key_of(c, "/")
        code, _, _ = c.form(f"/reputation/{uid}/add", {"my_post_key": k, "reputation": random.choice([1, -1, 2]), "comments": "soak rep"})
    elif op == "reply":
        tid = random.choice(ids["tids"]); k = key_of(c, f"/newreply/{tid}")
        code, _, _ = c.form(f"/newreply/{tid}", {"my_post_key": k, "message": f"soak reply {random.random()} to the thread"})
    else:
        fid = random.choice(ids["fids"]); k = key_of(c, f"/newthread/{fid}")
        code, _, _ = c.form(f"/newthread/{fid}", {"my_post_key": k, "subject": f"soak thread {random.random()}", "message": "soak opening post text"})
    return op, code
def admin_op(c):
    op = random.choice(["softdelete", "restore", "close", "open", "stick", "unstick", "approve", "unapprove", "move", "move", "delete", "postdelete", "postsoft", "postrestore", "postapprove", "edit", "split", "merge", "warn", "ban", "liftban", "usermerge", "userdelete"])
    k = key_of(c, "/")
    if op == "warn" and ids["uids"]:
        uid, _ = random.choice(ids["uids"])
        code, _, _ = c.form(f"/warnings/{uid}/warn", {"my_post_key": k, "custom_reason": "soak warning", "custom_points": random.randint(1, 4)})
    elif op == "ban" and ids["uids"]:
        _, name = random.choice(ids["uids"])
        code, _, _ = c.form("/modcp/ban", {"my_post_key": k, "username": name, "reason": "soak", "days": 0, "gid": 7})
    elif op == "liftban" and ids["banned"]:
        code, _, _ = c.form("/modcp/liftban", {"my_post_key": k, "uid": random.choice(ids["banned"])})
    elif op == "usermerge" and len(ids["uids"]) > 6 and random.random() < 0.15:
        (_, a), (_, b) = random.sample(ids["uids"], 2)
        kk = key_of(c, "/admin/users/merge")
        code, _, _ = c.form("/admin/users/merge", {"my_post_key": kk, "source": a, "destination": b})
    elif op == "userdelete" and len(ids["uids"]) > 6 and random.random() < 0.1:
        uid, _ = random.choice(ids["uids"])
        kk = key_of(c, "/admin/users")
        code, _, _ = c.form(f"/admin/users/{uid}/delete", {"my_post_key": kk, "deletecontent": random.choice(["", "1"])})
    elif op in ("warn", "ban", "liftban", "usermerge", "userdelete"):
        code, _, _ = c.form("/moderation/threads", {"my_post_key": k, "action": "close", "tids": random.choice(ids["tids"])})
    elif op.startswith("post"):
        a = {"postdelete": "delete", "postsoft": "softdelete", "postrestore": "restore", "postapprove": "approve"}[op]
        pid, tid = random.choice(ids["pt"])
        code, _, _ = c.form("/moderation/posts", {"my_post_key": k, "action": a, "tid": tid, "pids": pid})
    elif op == "split":
        pid, tid = random.choice(ids["pt"])
        code, _, _ = c.form("/moderation/split", {"my_post_key": k, "tid": tid, "pids": pid, "subject": "split off"})
    elif op == "merge":
        a, b = random.sample(ids["tids"], 2)
        code, _, _ = c.form("/moderation/merge", {"my_post_key": k, "tid": a, "target": str(b)})
    elif op == "move":
        code, _, _ = c.form("/moderation/move", {"my_post_key": k, "tids": random.choice(ids["tids"]), "target": random.choice(ids["fids"]), "method": "move"})
    elif op == "edit":
        pid = random.choice(ids["pids"]); k2 = key_of(c, f"/editpost/{pid}")
        code, _, _ = c.form(f"/editpost/{pid}", {"my_post_key": k2, "subject": "edited", "message": f"soak edit {random.random()} text body"})
    elif op == "delete" and random.random() > 0.25:
        op = "close"; code, _, _ = c.form("/moderation/threads", {"my_post_key": k, "action": "close", "tids": random.choice(ids["tids"])})
    else:
        code, _, _ = c.form("/moderation/threads", {"my_post_key": k, "action": op, "tids": random.choice(ids["tids"])})
    return op, code
def worker(c, fn):
    while time.time() < stop:
        try:
            op, code = fn(c); stats[(op, code // 100)] += 1
        except Exception as e: errors[str(e)[:80]] += 1
def refresher():
    while time.time() < stop: time.sleep(10); refresh()
ts = [threading.Thread(target=worker, args=(c, member_op)) for c in members] + [threading.Thread(target=worker, args=(c, admin_op)) for c in admins] + [threading.Thread(target=refresher)]
t0 = time.time(); [t.start() for t in ts]; [t.join() for t in ts]
tot = sum(stats.values())
print(f"{tot} operations in {time.time()-t0:.0f}s")
byop = collections.defaultdict(dict)
for (op, cls), n in sorted(stats.items()): byop[op][f"{cls}xx"] = n
for op, d in byop.items(): print(f"  {op:12} {d}")
print("client errors:", dict(errors))
server_errors = sum(n for (_, cls), n in stats.items() if cls == 5)
print(f"{server_errors} server errors (5xx)")
sys.exit(1 if server_errors or errors else 0)
