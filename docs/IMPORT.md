# Migrating from MyBB

`rbb import-mybb` reads a MyBB 1.8 database directly over MySQL/MariaDB and
replaces the content of the rbb database it is pointed at.

```sh
# 1. Create an empty rbb database (or reuse one; its content will be replaced).
createdb rbb

# 2. Import. MyBB can stay online; the importer only reads.
DATABASE_URL=postgres://rbb@localhost/rbb \
  rbb import-mybb --mysql-url mysql://mybb_user:pass@localhost/mybb --prefix mybb_ --yes

# 3. Copy uploads.
cp -r /var/www/mybb/uploads/avatars/.  "$RBB_UPLOAD_DIR/avatars/"
mkdir -p "$RBB_UPLOAD_DIR/mybb" && cp -r /var/www/mybb/uploads/. "$RBB_UPLOAD_DIR/mybb/"
```

Tested against the MyBB 1.8.41 schema.

## What is imported

| MyBB | rbb | Notes |
|---|---|---|
| Settings | `bbname`, `bburl`, `homename`, `homeurl`, `adminemail`, `contactemail` | Everything else keeps rbb's defaults. |
| Usergroups | usergroups | Built-in groups (1–7) are updated in place. Custom groups start from the Registered group's permissions. Every MyBB permission column with a matching rbb permission is copied. |
| Users | users | IDs, post counts, reputation, IPs (converted from binary), additional groups, buddy and ignore lists, avatars. |
| Passwords | kept as `mybb$salt$hash` | Users sign in with their existing password, which is then upgraded to argon2id. |
| Forums, forum permissions, moderators, thread prefixes | same | Custom forum permissions and moderator rights are mapped by column name. |
| Threads, posts | same | IDs are preserved. Moved-thread redirects are kept. MyCode is re-rendered by rbb's parser the first time each post is viewed. |
| Polls and votes | same | `||~|~||`-separated options become arrays. |
| Private messages | privatemessages | Folders, read status and receipts are kept. |
| Attachments | attachments | Paths are prefixed with `mybb/` (see `--uploads-prefix`). |
| Subscriptions, reputation | same | |

Not imported: themes and templates (rbb has its own template set), plugins,
calendars, warnings, the admin log and search indexes. Search needs no rebuild,
since PostgreSQL indexes posts as they are written.

## Links

Old URLs such as `showthread.php?tid=5`, `forumdisplay.php?fid=2` and
`member.php?action=profile&uid=1` permanently redirect (308) to their rbb
equivalents, so search engine rankings and old bookmarks keep working.

## After importing

- Counters are rebuilt automatically. `rbb check` verifies them.
- Orphans are repaired or removed: posts without a thread, threads without a
  first post, and subscriptions or permissions that point at deleted rows.
  Posts by deleted users keep their username and show as guest posts.
- If the PostgreSQL role is a superuser, foreign keys are checked after loading
  rather than row by row. This makes large imports much faster.
