//! Guest page cache.
//!
//! Most forum traffic is guests and crawlers reading the same pages. For them, the finished HTML
//! of opted-in pages (index, forums, threads, profiles, …) is kept in memory and served without
//! touching the database or the template engine.
//!
//! Correctness rules:
//! * Only anonymous GET requests without per-visitor state (flash messages, forum-password
//!   cookies) are served from or stored in the cache. The key covers everything else a guest page
//!   varies by: path and query, theme, language, colour mode and whether the client is a bot.
//! * The per-visitor CSRF token is rendered as a placeholder and substituted on every response.
//! * Pages carry tags for what they show (`board`, `forum:<fid>`, `thread:<tid>`). Writes that
//!   know their scope (a reply, a reaction, a vote…) invalidate just those tags; any other
//!   successful write clears the whole cache. Either way every node hears within ~50 ms through
//!   the `rbb_cache` NOTIFY channel, and entries expire after `TTL` regardless. Entries rendered
//!   while an invalidation happened are discarded (epoch/generation check).

use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Stand-in for the CSRF token in cached HTML. Only letters and digits, so it survives escaping.
pub const CSRF_SLOT: &str = "rbbcsrfslot7c1e9a4f2d";
/// Upper bound on how stale a cached guest page can be.
pub const TTL: Duration = Duration::from_secs(30);

pub struct Entry {
    pub html: Bytes,
    /// Tags this page depends on, with their generation when it was rendered.
    pub tags: Vec<(String, u64)>,
    /// Location for "who's online" (forum, thread), replayed on cache hits.
    pub fid: i32,
    pub tid: i32,
}

pub struct PageCache {
    entries: moka::sync::Cache<String, Arc<Entry>>,
    epoch: AtomicU64,
    /// Current generation per tag (absent = 0). Bumping one invalidates every page tagged with it.
    generations: dashmap::DashMap<String, u64>,
    enabled: bool,
}

impl PageCache {
    pub fn new(max_mb: u64) -> Self {
        PageCache {
            entries: moka::sync::Cache::builder()
                .time_to_live(TTL)
                .weigher(|k: &String, v: &Arc<Entry>| {
                    (k.len() + v.html.len()).min(u32::MAX as usize) as u32
                })
                .max_capacity(max_mb.max(1) * 1024 * 1024)
                .build(),
            epoch: AtomicU64::new(0),
            generations: dashmap::DashMap::new(),
            enabled: max_mb > 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        let e = self.entries.get(key)?;
        if e.tags.iter().all(|(t, g)| self.generation(t) == *g) {
            Some(e)
        } else {
            self.entries.invalidate(key);
            None
        }
    }

    pub fn generation(&self, tag: &str) -> u64 {
        self.generations.get(tag).map(|g| *g).unwrap_or(0)
    }

    /// Snapshot of the tags' generations, taken before rendering a page that depends on them.
    pub fn snapshot(&self, tags: &[String]) -> Vec<(String, u64)> {
        tags.iter()
            .map(|t| (t.clone(), self.generation(t)))
            .collect()
    }

    /// Invalidate every page tagged with any of `tags`.
    pub fn invalidate_tags<S: AsRef<str>>(&self, tags: &[S]) {
        // Renders in flight may have read the old data: don't let them store (see `put`).
        self.epoch.fetch_add(1, Ordering::AcqRel);
        for t in tags {
            *self.generations.entry(t.as_ref().to_string()).or_insert(0) += 1;
        }
    }

    /// Store a page rendered while the cache was at `epoch`; dropped if a write happened since.
    pub fn put(&self, key: String, entry: Entry, epoch: u64) {
        if self.enabled && self.epoch() == epoch {
            self.entries.insert(key, Arc::new(entry));
        }
    }

    /// Forget everything (content changed).
    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.entries.invalidate_all();
    }

    pub fn len(&self) -> u64 {
        self.entries.run_pending_tasks();
        self.entries.entry_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Tags for a forum listing and every forum above it (their listings show its last post).
pub fn forum_tags(cache: &crate::cache::Cache, fid: i32) -> Vec<String> {
    let mut v: Vec<String> = cache
        .forum(fid)
        .map(|f| f.parentlist.iter().map(|p| format!("forum:{p}")).collect())
        .unwrap_or_default();
    let own = format!("forum:{fid}");
    if !v.contains(&own) {
        v.push(own);
    }
    v
}

/// What a new or changed post in `tid` (in forum `fid`) invalidates: the thread, its forum
/// listings, and board-wide pages (index last posts, stats, portal, profiles' post counts).
pub fn post_tags(cache: &crate::cache::Cache, fid: i32, tid: i32) -> Vec<String> {
    let mut v = forum_tags(cache, fid);
    v.push(format!("thread:{tid}"));
    v.push("board".into());
    v
}

/// Build the cache key for a guest request.
pub fn key(path: &str, query: &str, theme: i32, lang: &str, colormode: &str, bot: bool) -> String {
    format!("{theme}|{lang}|{colormode}|{}|{path}?{query}", bot as u8)
}

/// Writes that don't change what guests see, so they needn't flush the cache.
pub fn write_is_private(path: &str) -> bool {
    const PRIVATE: &[&str] = &[
        "/member/login",
        "/member/logout",
        "/colormode",
        "/theme/",
        "/lang/",
        "/preview",
        "/drafts/save",
        "/usercp/alerts",
        "/pgp/",
        "/pm",
        "/captcha",
        "/member/checkname",
        "/attachment/upload",
        "/usercp/notepad",
        "/admin/verify",
    ];
    PRIVATE.iter().any(|p| path.starts_with(p))
}

/// Put the visitor's CSRF token into cached (or freshly rendered) guest HTML.
pub fn personalize(html: &[u8], csrf: &str) -> Bytes {
    match std::str::from_utf8(html) {
        Ok(s) if s.contains(CSRF_SLOT) => Bytes::from(s.replace(CSRF_SLOT, csrf)),
        _ => Bytes::copy_from_slice(html),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_guards_stale_renders() {
        let c = PageCache::new(4);
        let e = c.epoch();
        c.clear();
        c.put(
            "k".into(),
            Entry {
                html: Bytes::from_static(b"x"),
                tags: vec![],
                fid: 0,
                tid: 0,
            },
            e,
        );
        assert!(
            c.get("k").is_none(),
            "a page rendered before a write must not be stored"
        );
        let e = c.epoch();
        c.put(
            "k".into(),
            Entry {
                html: Bytes::from_static(b"x"),
                tags: vec![],
                fid: 0,
                tid: 0,
            },
            e,
        );
        assert!(c.get("k").is_some());
        c.clear();
        assert!(c.get("k").is_none());
    }

    #[test]
    fn tags_invalidate_only_their_pages() {
        let c = PageCache::new(4);
        let tags = |t: &[&str]| c.snapshot(&t.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        c.put(
            "t1".into(),
            Entry {
                html: Bytes::from_static(b"1"),
                tags: tags(&["thread:1"]),
                fid: 0,
                tid: 1,
            },
            c.epoch(),
        );
        c.put(
            "t2".into(),
            Entry {
                html: Bytes::from_static(b"2"),
                tags: tags(&["thread:2"]),
                fid: 0,
                tid: 2,
            },
            c.epoch(),
        );
        let stale = tags(&["thread:1"]);
        c.invalidate_tags(&["thread:1"]);
        assert!(c.get("t1").is_none());
        assert!(c.get("t2").is_some(), "other threads stay cached");
        // A page rendered before the invalidation but stored after it is rejected on read.
        c.put(
            "t1".into(),
            Entry {
                html: Bytes::from_static(b"old"),
                tags: stale,
                fid: 0,
                tid: 1,
            },
            c.epoch(),
        );
        assert!(c.get("t1").is_none());
    }

    #[test]
    fn personalizes_every_slot() {
        let html = format!("<meta content=\"{CSRF_SLOT}\"><input value=\"{CSRF_SLOT}\">");
        let out = personalize(html.as_bytes(), "abc123");
        assert_eq!(
            &out[..],
            b"<meta content=\"abc123\"><input value=\"abc123\">"
        );
    }

    #[test]
    fn keys_separate_what_guests_see() {
        assert_ne!(
            key("/", "", 1, "en", "auto", false),
            key("/", "", 1, "de", "auto", false)
        );
        assert_ne!(
            key("/", "", 1, "en", "auto", false),
            key("/", "", 1, "en", "dark", false)
        );
        assert_ne!(
            key("/", "", 1, "en", "auto", false),
            key("/", "", 2, "en", "auto", false)
        );
        assert_ne!(
            key("/", "", 1, "en", "auto", false),
            key("/", "", 1, "en", "auto", true)
        );
        assert_ne!(
            key("/forum/2", "page=2", 1, "en", "auto", false),
            key("/forum/2", "page=3", 1, "en", "auto", false)
        );
    }
}

#[cfg(test)]
mod merge_tests {
    /// rbb relies on the last map winning in `merge_maps` (page values over the global context).
    /// Guard against a minijinja upgrade silently flipping that.
    #[test]
    fn merge_maps_last_wins() {
        let a = minijinja::context! { k => 1 };
        let b = minijinja::context! { k => 2 };
        let m = minijinja::value::merge_maps([a, b]);
        assert_eq!(m.get_attr("k").unwrap().as_i64(), Some(2));
    }
}
