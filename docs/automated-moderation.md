# System automated moderation

Added September 30, 2026. This is local, deterministic rule-based moderation; no external services,
content upload, permanent deletion, warning points or automatic bans are involved.

## Start and configure

Build and restart the application normally. Startup applies the additive
`migrations/0008_automod.sql` migration. The existing System user is resolved by its protected
`is_system` marker, even if its display name changes.

Open **Admin CP → Automated moderation** (`/admin/automod`). Access requires the existing
ACP verification, applicable 2FA and `content` admin permission. Every write requires CSRF.

The initial mode is **Observe**. New/edited posts enter a durable database queue; existing
posts are not backfilled. Every 30 seconds the `automoderation` scheduled task evaluates
up to 100 queued posts. `RBB_RUN_TASKS` and the task's Enabled switch must both be on.
Observation logs matching rules without changing content. Switch to **Quarantine** to
send matches to the standard moderation queue. **Off** drains queued entries without
rule evaluation and leaves existing quarantines intact.

Default rules:

- Five or more links for members with fewer than five currently visible counted posts.
- Three or more identical messages by one member in the last ten minutes, with the same
  trust threshold. Matching ignores leading/trailing spaces and ASCII case in SQL.
- Administrator-configured literal phrases in subject/body, ignoring case, for every
  non-staff member. The phrase list starts empty. Matches also include quotes and MyCode.

Link detection recognizes HTTP(S), `www.` and MyCode URL tags; it is a spam heuristic,
not URL reputation checking. Counts use the current member count, not lifetime posting
history. Staff (including forum/group moderators), administrators and System are exempt.
Guests share uid 0 for repetition checks. No automatic decision is made about private
messages, attachments, reports or existing historical content.

## Audit and undo

Each match saves action ID, System UID, post/thread/forum IDs, time, rule explanations,
policy revision and affected row revisions. Content is never rewritten. First-post matches
quarantine the whole thread; reply matches quarantine only that reply. Counter updates,
queue consumption and moderator-log writes share the action's transaction. Failed items
remain queued for retry; one failed item does not prevent the rest of the batch from running.
PostgreSQL transaction locks serialize automated workers and policy/undo operations across
nodes. Guest-page and moderation-count caches are invalidated after committed changes.

The review page shows the latest 100 decisions; the full history remains in
`automod_actions` and the moderator log. All policy changes retain both previous and new
values in `automod_config_history` and the admin log. Saving checks the displayed revision
to avoid silently overwriting another administrator's settings. The latest 20 policy
changes offer **Restore previous policy**; older revisions remain available in the database.
Restoring policy records a new change and does not alter existing quarantine decisions.

**Undo** restores an unchanged quarantine to its original public visibility, including
forum/user/thread counters. It records who restored it and when. An undone post remains
exempt from future automation, including after edits, to avoid repeatedly quarantining a
reviewed false positive. Staff may continue to moderate that post normally.

**Disable and undo all quarantines** first saves mode Off, then restores actions in reverse
order. Each undo commits independently, so an interrupted request can safely be repeated.
It reports restored and conflicting action counts. Later content edits, post moves,
manual visibility changes, removed content, or relevant whole-thread changes cause an
undo conflict. Conflicts stay recorded and must be reviewed in the normal moderation queue;
the system will not overwrite a human decision. A thread's independent close/open state
is never changed when restoring a reply. Avoid changing policy in another admin session
while a bulk rollback is running.

## Turning it off completely

**Off** mode stops evaluation while leaving everything in place. To also remove the database
triggers and disable the task:

1. In Admin CP, use **Disable and undo all quarantines**. Review any conflicts manually.
2. Stop the application server.
3. Run `scripts/automod-disable.sql` with `psql -v ON_ERROR_STOP=1`, for example
   `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f scripts/automod-disable.sql`. It records the mode
   change, disables the task and removes the triggers and functions from migration 0008, keeping
   all audit rows and schema additions.
4. Restart and run `rbb check` to verify counters.

Keep `0008_automod.sql` and the database's migration history intact: deleting an applied
migration breaks SQLx's start-up checks. Unused tables and columns are kept on purpose, to preserve
the audit trail and avoid a destructive schema rollback. To turn automated moderation back on,
reinstall the trigger and function definitions from 0008 (the migration won't run again by
itself) and re-enable the task.

## Validation

Rule tests run with `cargo test`. The ignored PostgreSQL test requires a fresh, disposable
database named `rbb_automod_test*` and refuses an installed database:

```
RBB_AUTOMOD_TEST_DATABASE_URL='postgresql://.../rbb_automod_test' \
  cargo test durable_quarantine_and_rollback -- --ignored --nocapture
```

It verifies observe/off modes, edit queueing, multi-worker idempotency, System attribution,
staff exemptions, reply/whole-thread restoration, permanent review exemptions, undo
conflicts, failure/retry atomicity and counters after each change. It also renders the
new admin template. Tests never use `.env`'s forum database for this integration test.
