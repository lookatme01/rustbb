//! Forum access: the one place that decides what a viewer may see.
//!
//! Every reader of forum content — forum and thread pages, the API, search, feeds, the sitemap,
//! the portal, the archive, similar threads, attachments, moderation tools and the counters
//! aggregated into parent forums — asks [`ForumAccess`], so a rule is enforced everywhere or
//! nowhere. The rules look at the forum *and all of its ancestors*:
//!
//! * the forum and every ancestor must exist;
//! * every ancestor must be active, unless the viewer moderates it;
//! * the viewer's groups must be allowed to view every ancestor (`canview`);
//! * every password-protected ancestor must be unlocked (or moderated by the viewer);
//! * threads are listed only with `canviewthreads`, and only the viewer's own with
//!   `canonlyviewownthreads` (moderators see all);
//! * unapproved and soft-deleted content only for moderators with the matching permission.
//!
//! Access is computed from the shared cache snapshot (forums, permissions, moderators), which is
//! reloaded on every node from the durable cluster log when any of them changes, so a missed
//! notification cannot keep stale permissions alive.

use crate::cache::Cache;
use crate::perms::{ForumPerms, GroupPerms, ModPerms};
use std::collections::HashMap;

/// Which threads of a forum the viewer may read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threads {
    None,
    /// Only threads the viewer started.
    Own,
    All,
}

/// Why a forum is not visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    NotFound,
    /// No permission (here or on an ancestor), or an ancestor is inactive.
    Forbidden,
    /// A password-protected forum (this one or an ancestor) must be unlocked first.
    Password(i32),
}

/// What a viewer may do in one forum.
#[derive(Clone, Debug)]
pub struct Access {
    pub fid: i32,
    pub perms: ForumPerms,
    pub moderator: Option<ModPerms>,
    pub threads: Threads,
}

impl Access {
    /// `visible` states of threads/posts whose content the viewer may see.
    pub fn visible_states(&self) -> Vec<i16> {
        let mut v = vec![1];
        if let Some(m) = &self.moderator {
            if m.canviewunapprove {
                v.push(0);
            }
            if m.canviewdeleted {
                v.push(-1);
            }
        }
        v
    }

    pub fn is_moderator(&self) -> bool {
        self.moderator.is_some()
    }
}

/// Who is looking.
pub struct Viewer<'a> {
    pub uid: i32,
    pub groups: &'a [i32],
    pub perms: &'a GroupPerms,
    /// Forums whose password the viewer has entered (checked by the caller against the
    /// forum's current password version).
    pub unlocked: &'a dyn Fn(i32) -> bool,
}

/// What a forum list is for: each purpose adds its own permission on top of readability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Reading threads (feeds, portal, archive, similar threads, sitemaps, API listings).
    Read,
    /// Searching (also needs `cansearch`).
    Search,
}

/// Access of one viewer to every forum, computed once per request.
pub struct ForumAccess {
    by_fid: HashMap<i32, Result<Access, Denied>>,
}

impl ForumAccess {
    pub fn compute(cache: &Cache, v: &Viewer<'_>) -> ForumAccess {
        let mut by_fid = HashMap::with_capacity(cache.forums.len());
        for f in cache.forums.iter() {
            by_fid.insert(f.fid, Self::evaluate(cache, v, f.fid));
        }
        ForumAccess { by_fid }
    }

    fn evaluate(cache: &Cache, v: &Viewer<'_>, fid: i32) -> Result<Access, Denied> {
        let forum = cache.forum(fid).ok_or(Denied::NotFound)?;
        let moderator = cache.mod_perms(v.uid, v.groups, v.perms, fid);
        // `forum_perms` already denies `canview` when any ancestor is not viewable.
        let perms = cache.forum_perms(v.groups, fid);
        if !perms.canview {
            return Err(Denied::Forbidden);
        }
        for anc in &forum.parentlist {
            let Some(a) = cache.forum(*anc) else {
                return Err(Denied::NotFound);
            };
            let moderates = cache.mod_perms(v.uid, v.groups, v.perms, *anc).is_some();
            if !a.active && !moderates {
                return Err(Denied::Forbidden);
            }
            if a.has_password() && !moderates && !(v.unlocked)(*anc) {
                return Err(Denied::Password(*anc));
            }
        }
        let threads = if forum.is_category() || !forum.linkto.is_empty() || !perms.canviewthreads {
            Threads::None
        } else if perms.canonlyviewownthreads && moderator.is_none() {
            Threads::Own
        } else {
            Threads::All
        };
        Ok(Access {
            fid,
            perms,
            moderator,
            threads,
        })
    }

    /// The viewer's access to `fid`, or why there is none.
    pub fn forum(&self, fid: i32) -> Result<&Access, Denied> {
        match self.by_fid.get(&fid) {
            Some(Ok(a)) => Ok(a),
            Some(Err(d)) => Err(*d),
            None => Err(Denied::NotFound),
        }
    }

    /// Whether the forum itself (its name, its place in lists) may be shown.
    pub fn can_see(&self, fid: i32) -> bool {
        self.forum(fid).is_ok()
    }

    /// Whether the forum appears in forum lists: visible, or locked only by its own password
    /// (shown with a lock so members can find the password form).
    pub fn listed(&self, fid: i32) -> bool {
        match self.forum(fid) {
            Ok(_) => true,
            Err(Denied::Password(p)) => p == fid,
            Err(_) => false,
        }
    }

    /// Whether threads of `fid` started by `author` may be read.
    pub fn can_read_thread(&self, fid: i32, author: i32, viewer: i32) -> bool {
        match self.forum(fid) {
            Ok(a) => match a.threads {
                Threads::All => true,
                Threads::Own => viewer > 0 && author == viewer,
                Threads::None => false,
            },
            Err(_) => false,
        }
    }

    /// Forums whose threads may be read for `purpose`: (all threads, only own threads).
    pub fn readable(&self, purpose: Purpose) -> (Vec<i32>, Vec<i32>) {
        let mut all = vec![];
        let mut own = vec![];
        for a in self.by_fid.values().filter_map(|r| r.as_ref().ok()) {
            if purpose == Purpose::Search && !a.perms.cansearch {
                continue;
            }
            match a.threads {
                Threads::All => all.push(a.fid),
                Threads::Own => own.push(a.fid),
                Threads::None => {}
            }
        }
        all.sort_unstable();
        own.sort_unstable();
        (all, own)
    }

    /// Forums whose latest activity and counters may be shown to this viewer (aggregated into
    /// parent forums on the index): readable with all threads, not just the viewer's own.
    pub fn counted(&self) -> Vec<i32> {
        self.readable(Purpose::Read).0
    }
}

/// What guests may see (sitemaps and other public listings, whoever asks for them).
pub fn guest(cache: &Cache) -> std::sync::Arc<ForumAccess> {
    let perms = cache.group_perms(&[1]);
    let none = |_: i32| false;
    cache.forum_access(
        &Viewer {
            uid: 0,
            groups: &[1],
            perms: &perms,
            unlocked: &none,
        },
        Some("[1]|0".into()),
    )
}

/// What a member (not the current viewer) may see, e.g. the recipient of a notification.
/// Password-protected forums count as locked: a notification must not carry their content.
pub fn member(cache: &Cache, uid: i32, groups: &[i32]) -> std::sync::Arc<ForumAccess> {
    let perms = cache.group_perms(groups);
    let none = |_: i32| false;
    let mut g = groups.to_vec();
    g.sort_unstable();
    let personal = cache.is_any_moderator(uid, groups, &perms);
    cache.forum_access(
        &Viewer {
            uid,
            groups,
            perms: &perms,
            unlocked: &none,
        },
        Some(format!("{g:?}|{}", if personal { uid } else { 0 })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Forum;

    fn forum(fid: i32, pid: i32, parents: Vec<i32>) -> Forum {
        Forum {
            fid,
            name: format!("f{fid}"),
            description: String::new(),
            linkto: String::new(),
            kind: "f".into(),
            pid,
            parentlist: parents,
            disporder: fid,
            active: true,
            open: true,
            allowhtml: false,
            allowmycode: true,
            allowsmilies: true,
            allowimgcode: true,
            allowvideocode: true,
            allowpicons: true,
            allowtratings: true,
            usepostcounts: true,
            usethreadcounts: true,
            requireprefix: false,
            password: String::new(),
            showinjump: true,
            style: 0,
            overridestyle: false,
            rulestype: 0,
            rulestitle: String::new(),
            rules: String::new(),
            defaultdatecut: 0,
            defaultsortby: String::new(),
            defaultsortorder: String::new(),
            password_version: 0,
        }
    }

    /// 1 (category) → 2 → 3 → 4, plus 5 at the top; guests (group 1) may view everything.
    fn board() -> Cache {
        let mut c = Cache {
            forum_perms: std::sync::Arc::new(
                [1, 5]
                    .into_iter()
                    .map(|fid| ((fid, 1), ForumPerms::default()))
                    .collect(),
            ),
            ..Default::default()
        };
        c.forum_perms = std::sync::Arc::new(
            [1, 5]
                .into_iter()
                .map(|fid| ((fid, 1), ForumPerms::default()))
                .collect(),
        );
        let mut cat = forum(1, 0, vec![1]);
        cat.kind = "c".into();
        c.forums = std::sync::Arc::new(vec![
            cat,
            forum(2, 1, vec![1, 2]),
            forum(3, 2, vec![1, 2, 3]),
            forum(4, 3, vec![1, 2, 3, 4]),
            forum(5, 0, vec![5]),
        ]);
        c.reindex_forums();
        c
    }

    fn guest(c: &Cache, unlocked: &dyn Fn(i32) -> bool) -> ForumAccess {
        let perms = c.group_perms(&[1]);
        ForumAccess::compute(
            c,
            &Viewer {
                uid: 0,
                groups: &[1],
                perms: &perms,
                unlocked,
            },
        )
    }

    #[test]
    fn inactive_ancestor_hides_descendants() {
        let mut c = board();
        std::sync::Arc::make_mut(&mut c.forums)[1].active = false; // forum 2
        c.reindex_forums();
        let a = guest(&c, &|_| false);
        assert!(a.can_see(1));
        for f in [2, 3, 4] {
            assert_eq!(a.forum(f).err(), Some(Denied::Forbidden), "forum {f}");
        }
        assert!(a.can_see(5));
        assert!(!a.readable(Purpose::Search).0.contains(&4));
    }

    #[test]
    fn password_on_ancestor_locks_descendants_until_unlocked() {
        let mut c = board();
        std::sync::Arc::make_mut(&mut c.forums)[2].password = "argon2-hash".into(); // forum 3
        c.reindex_forums();
        let locked = guest(&c, &|_| false);
        assert_eq!(locked.forum(4).err(), Some(Denied::Password(3)));
        assert!(locked.can_see(2));
        assert!(!locked.readable(Purpose::Read).0.contains(&4));
        let open = guest(&c, &|fid| fid == 3);
        assert!(open.can_see(4));
        assert!(open.readable(Purpose::Read).0.contains(&4));
    }

    #[test]
    fn categories_have_no_threads() {
        let c = board();
        let a = guest(&c, &|_| false);
        assert_eq!(a.forum(1).unwrap().threads, Threads::None);
        assert!(!a.readable(Purpose::Read).0.contains(&1));
    }

    #[test]
    fn unknown_forum_is_not_found() {
        let c = board();
        assert_eq!(
            guest(&c, &|_| false).forum(99).err(),
            Some(Denied::NotFound)
        );
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// A random forest: forum i (1-based) hangs under a random earlier forum or the root,
        /// with random active / password / guest-viewable flags.
        fn forest() -> impl Strategy<Value = Vec<(usize, bool, bool, bool)>> {
            prop::collection::vec(
                (0usize..100, any::<bool>(), any::<bool>(), any::<bool>()),
                1..25,
            )
        }

        fn build(spec: &[(usize, bool, bool, bool)]) -> Cache {
            let mut forums = vec![];
            let mut parents: Vec<Vec<i32>> = vec![];
            let mut perms = std::collections::HashMap::new();
            for (i, (p, active, pw, view)) in spec.iter().enumerate() {
                let fid = i as i32 + 1;
                let pid = if i == 0 { 0 } else { (*p % (i + 1)) as i32 };
                let mut pl = if pid == 0 {
                    vec![]
                } else {
                    parents[(pid - 1) as usize].clone()
                };
                pl.push(fid);
                parents.push(pl.clone());
                let mut f = forum(fid, pid, pl);
                f.active = *active;
                if *pw {
                    f.password = "hash".into();
                }
                forums.push(f);
                let fp = ForumPerms {
                    canview: *view,
                    ..ForumPerms::default()
                };
                perms.insert((fid, 1), fp);
            }
            let mut c = Cache {
                forum_perms: std::sync::Arc::new(perms),
                ..Default::default()
            };
            c.forums = std::sync::Arc::new(forums);
            c.reindex_forums();
            c
        }

        proptest! {
            /// Seeing a forum implies every ancestor is active, viewable and unlocked; readable
            /// forums are always visible; nothing is readable that is not visible.
            #[test]
            fn visibility_is_inherited(spec in forest(), unlock_mask in any::<u32>()) {
                let c = build(&spec);
                let unlocked = move |fid: i32| unlock_mask & (1 << (fid % 32)) != 0;
                let a = guest(&c, &unlocked);
                for f in c.forums.iter() {
                    if a.can_see(f.fid) {
                        for anc in &f.parentlist {
                            let af = c.forum(*anc).unwrap();
                            prop_assert!(af.active, "inactive ancestor {anc} of visible {}", f.fid);
                            prop_assert!(c.forum_perms(&[1], *anc).canview);
                            prop_assert!(!af.has_password() || unlocked(*anc));
                        }
                    }
                }
                let (all, own) = a.readable(Purpose::Read);
                for fid in all.iter().chain(own.iter()) {
                    prop_assert!(a.can_see(*fid));
                }
                // A forum that is listed but not visible is locked by its own password only.
                for f in c.forums.iter() {
                    if a.listed(f.fid) && !a.can_see(f.fid) {
                        prop_assert_eq!(a.forum(f.fid).err(), Some(Denied::Password(f.fid)));
                    }
                }
            }
        }
    }
}
