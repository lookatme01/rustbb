#!/usr/bin/env python3
"""Throw hostile input at a running board and report every 5xx.

    scripts/fuzz-http.py forms|get|api|all BASE_URL --disposable-board [--password PW]

  forms  crawl as an administrator, then resubmit every POST form with hostile values (empty,
         huge, negative, overflowing, NUL byte, bidi/zero-width, SQL and template fragments...)
  get    mutate path numbers and query parameters of every page it finds (page=-1, limit=2^63...)
  api    JSON bodies against the API's write endpoints (wrong types, nulls, broken JSON...)

A 5xx means a handler failed where it should have said "invalid input" (400/422). The `forms`
mode submits destructive actions (deletes, merges, password changes), so only ever point this at
a throwaway board: install one into a scratch database and serve it on a free port, e.g.

    createdb rbb_fuzz && DATABASE_URL=postgres://.../rbb_fuzz rbb install --admin-password admin12345
    rbb seed --users 100 --threads 200 --posts 1500      # enough content to have real pages
    # set ratelimit_requests to 0 in settings, then: rbb serve   (note the port!)

Check that the process answering BASE_URL is the one you started (a port already in use makes
`rbb serve` fail while the fuzzer happily attacks whatever else is listening).
"""
import argparse
import collections
import html
import http.cookiejar
import json
import re
import sys
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


class Client:
    def __init__(self, base):
        self.base = base.rstrip("/")
        self.jar = http.cookiejar.CookieJar()
        self.op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar), NoRedirect)

    def req(self, path, data=None, method=None, ctype=None, headers=None):
        if isinstance(data, str):
            data = data.encode("utf-8", "replace")
        r = urllib.request.Request(self.base + path, data=data, method=method)
        if ctype:
            r.add_header("Content-Type", ctype)
        for k, v in (headers or {}).items():
            r.add_header(k, v)
        try:
            x = self.op.open(r, timeout=30)
            return x.status, x.read().decode("utf-8", "replace"), x.headers.get("Location", "")
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode("utf-8", "replace"), e.headers.get("Location", "")
        except Exception as e:  # connection reset, timeout: also worth reporting
            return 0, str(e), ""

    def form(self, path, fields):
        return self.req(path, urllib.parse.urlencode(fields, errors="replace"),
                        ctype="application/x-www-form-urlencoded")

    def login(self, password):
        _, b, _ = self.req("/member/login")
        self.form("/member/login", {"my_post_key": key(b), "username": "admin", "password": password})
        _, b, _ = self.req("/admin/verify")
        self.form("/admin/verify", {"my_post_key": key(b), "password": password})


def key(body):
    m = re.search(r'name="my_post_key" value="([^"]+)"', body)
    return m.group(1) if m else ""


def norm(path):
    return re.sub(r"\d+", "N", re.sub(r"=[^&]*", "=", path))


SKIP = re.compile(r"logout|/live|/imgproxy|/export|/backup|/static|/uploads|/attachment/\d+$|/captcha")
START = ["/", "/admin", "/modcp", "/usercp", "/pm", "/calendar", "/search", "/members", "/online",
         "/portal", "/member/register", "/admin/settings", "/admin/users", "/admin/forums",
         "/admin/tools", "/admin/groups", "/admin/promotions", "/admin/massmail", "/admin/automod",
         "/admin/themes", "/api/v1/forums", "/api/v1/me", "/api/v1/search?q=a", "/syndication"]


def crawl(c, limit, per_shape):
    seen, queue, pages, forms = set(), collections.deque(START), [], {}
    shape = collections.Counter()
    form_re = re.compile(r"<form\b([^>]*)>(.*?)</form>", re.S | re.I)
    while queue and len(pages) < limit:
        p = queue.popleft()
        if p in seen or SKIP.search(p) or shape[norm(p)] >= per_shape:
            continue
        seen.add(p)
        shape[norm(p)] += 1
        pages.append(p)
        _, b, _ = c.req(p)
        for h in re.findall(r'href="(/[^"#]*)"', b):
            h = html.unescape(h)
            if not h.startswith("//"):
                queue.append(h)
        for attrs, body in form_re.findall(b):
            if not re.search(r'method="post"', attrs, re.I):
                continue
            a = re.search(r'action="([^"]*)"', attrs)
            action = html.unescape(a.group(1)) if a else p
            if not action.startswith("/") or SKIP.search(action):
                continue
            fields = []
            for tag in re.findall(r"<(?:input|textarea|select)\b[^>]*>", body, re.I):
                nm = re.search(r'name="([^"]+)"', tag)
                if not nm:
                    continue
                low = tag.lower()
                ty = re.search(r'type="([^"]+)"', tag)
                ty = ty.group(1).lower() if ty else ("select" if low.startswith("<select") else
                                                     "textarea" if low.startswith("<textarea") else "text")
                val = re.search(r'value="([^"]*)"', tag)
                fields.append((nm.group(1), ty, html.unescape(val.group(1)) if val else ""))
            forms.setdefault((action, tuple(sorted(f[0] for f in fields))), (action, fields, p))
    return pages, list(forms.values())


class Report:
    def __init__(self):
        self.bad, self.sent = [], 0

    def check(self, code, what, body):
        self.sent += 1
        if code >= 500 or code == 0:
            self.bad.append((code, what, re.sub(r"\s+", " ", body)[:120]))

    def finish(self, label):
        print(f"{label}: sent {self.sent} requests; {len(self.bad)} server errors")
        seen = set()
        for code, what, body in self.bad:
            k = (code, re.sub(r"\d+", "N", what))
            if k not in seen:
                seen.add(k)
                print("  ERR", code, what[:150], "|", body)
        return not self.bad


FORM_PAYLOADS = ["", "A" * 20000, "-1", "0", "99999999999999999999", "1e999", "a\x00b",
                 "\U0001F4A5‮​", "' OR 1=1 --", "<script>alert(1)</script>", "{{7*7}}",
                 "../../../etc/passwd", "%s%n%x", "\n\r\n"]


def fuzz_forms(c, password):
    pages, forms = crawl(c, 500, 3)
    print(f"forms: crawled {len(pages)} pages, {len(forms)} distinct POST forms")
    rep = Report()
    for action, fields, page in forms:
        for pay in FORM_PAYLOADS:
            _, b, _ = c.req(page)
            k = key(b)
            data = []
            for nm, ty, val in fields:
                if nm == "my_post_key":
                    data.append((nm, k))
                elif ty in ("hidden", "submit", "button"):
                    data.append((nm, val))
                elif ty in ("checkbox", "radio"):
                    data.append((nm, val or "1"))
                elif ty == "password":
                    data.append((nm, password if pay in ("", "0") else pay))
                else:
                    data.append((nm, pay))
            if k and not any(n == "my_post_key" for n, _ in data):
                data.append(("my_post_key", k))
            code, rb, _ = c.form(action, data)
            rep.check(code, f"POST {action} [{pay[:20]!r}]", rb)
            if rep.sent % 40 == 0:
                c.login(password)  # a fuzzed password change may have ended the session
    return rep.finish("forms")


HOSTILE = ["0", "-1", "99999999999", "2147483648", "abc", "", "%E2%80%AE%E2%80%8B", "1%2F2", "9" * 400,
           "%27%20OR%201%3D1", "1e999", "-9223372036854775808", "%FF%FE", "%7B%7B7*7%7D%7D"]
PARAMS = ["page", "sort", "sortby", "order", "limit", "per_page", "fid", "tid", "uid", "pid", "q",
          "keywords", "cursor", "since", "before", "after", "mode", "view", "year", "month"]


def fuzz_get(c, _password):
    pages, _ = crawl(c, 260, 2)
    print(f"get: {len(pages)} url shapes")
    rep = Report()
    for u in pages:
        parts = urllib.parse.urlsplit(u)
        segs = parts.path.split("/")
        tail = ("?" + parts.query) if parts.query else ""
        for i, s in enumerate(segs):
            if s.isdigit():
                for h in HOSTILE[:9]:
                    p = "/".join(segs[:i] + [h] + segs[i + 1:]) + tail
                    code, body, _ = c.req(p)
                    rep.check(code, "GET " + p[:120], body)
        qs = urllib.parse.parse_qsl(parts.query, keep_blank_values=True)
        for name in dict.fromkeys([k for k, _ in qs] + PARAMS[:12]):
            for h in HOSTILE:
                q = [(k, v) for k, v in qs if k != name]
                p = parts.path + "?" + urllib.parse.urlencode(q) + ("&" if q else "") + name + "=" + h
                code, body, _ = c.req(p)
                rep.check(code, "GET " + p[:120], body)
    return rep.finish("get")


def fuzz_api(c, _password):
    csrf = next((ck.value for ck in c.jar if ck.name == "__csrf"), "")
    if not csrf:
        m = re.search(r'name="csrf-token" content="([^"]+)"', c.req("/")[1])
        csrf = m.group(1) if m else ""
    hdr = {"X-CSRF-Token": csrf}
    vals = [None, "", "A" * 70000, -1, 0, 2**63, 1e308, True, [], {}, ["x"], {"a": 1}, "‮​",
            "' OR 1=1 --", 99999999999]
    targets = [("/api/v1/forums/2/threads", ["subject", "message", "prefix"]),
               ("/api/v1/threads/1/posts", ["message", "subject"]),
               ("/api/v1/auth/token", ["username", "password", "scopes"])]
    rep = Report()
    for path, fields in targets:
        for f in fields:
            for v in vals:
                body = {k: "valid text here" for k in fields}
                body[f] = v
                code, rb, _ = c.req(path, json.dumps(body), "POST", "application/json", hdr)
                rep.check(code, f"POST {path} {f}={str(v)[:20]!r}", rb)
        for raw in [b"", b"{", b"[]", b"null", b"\xff\xfe", b"{" * 5000,
                    b'{"message":' + b"[" * 3000 + b"]" * 3000 + b"}"]:
            code, rb, _ = c.req(path, raw, "POST", "application/json", hdr)
            rep.check(code, f"POST {path} raw {raw[:12]!r}", rb)
        for ct in [None, "text/plain", "application/json; charset=bogus", "multipart/form-data"]:
            code, rb, _ = c.req(path, b'{"message":"x"}', "POST", ct, hdr)
            rep.check(code, f"POST {path} content-type {ct}", rb)
    return rep.finish("api")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=["forms", "get", "api", "all"])
    ap.add_argument("base", help="e.g. http://127.0.0.1:50352")
    ap.add_argument("--password", default="admin12345", help="the administrator's password")
    ap.add_argument("--disposable-board", action="store_true",
                    help="confirm this board and its database can be wrecked")
    a = ap.parse_args()
    if not a.disposable_board:
        sys.exit("refusing to run: this submits destructive actions. Use a throwaway board and pass --disposable-board")
    c = Client(a.base)
    c.login(a.password)
    modes = {"forms": fuzz_forms, "get": fuzz_get, "api": fuzz_api}
    ok = True
    # `forms` last: a fuzzed password form can change the administrator's password.
    for name in (["get", "api", "forms"] if a.mode == "all" else [a.mode]):
        ok &= modes[name](c, a.password)
        c.login(a.password)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
