//! Staff capabilities: what a member may do as staff, decided in one place.
//!
//! Being a moderator of *some* forum used to unlock staff features board-wide. Now each feature
//! has its own capability, granted by usergroup permissions, and forum moderators act only within
//! the forums they moderate:
//!
//! | Capability | Granted by |
//! | --- | --- |
//! | Mod CP | `canmodcp`, or moderating any forum |
//! | Read / write moderator notes | `canviewmodnotes` / `canaddmodnotes` |
//! | Warn / see warning logs / ban | `canwarnusers` / `canviewwarnlogs` / `canbanusers` |
//! | Post reports | `canmodcp` + `canmanagereportedcontent` (all forums), or forum moderators (their forums) |
//! | Profile and reputation reports | `canmodcp` + `canmanagereportedcontent` |
//! | Private message reports | `canviewpmreports` (they contain private messages) |
//! | Moderator log | `canmodcp` + `canviewmodlogs` (all forums), or forum moderators (their forums) |
//! | A member's history across all forums | `canviewallmodhistory`; others see their forums only |
//! | IP addresses | `canuseipsearch` |
//!
//! Administrators and super moderators have every capability in every forum.

use crate::cache::Cache;
use crate::perms::GroupPerms;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    ModCp,
    ReadModNotes,
    WriteModNotes,
    Warn,
    ViewWarnLogs,
    Ban,
    /// Reports of posts (in the forums of [`Staff::scope`]).
    PostReports,
    /// Reports of profiles and reputation comments.
    MemberReports,
    /// Reports of private messages.
    PmReports,
    /// The moderator log (entries of the forums in [`Staff::scope`]).
    ModLog,
    /// The moderation queue (unapproved content in the forums of [`Staff::scope`]).
    ModQueue,
    /// A member's moderation history in forums the viewer does not moderate.
    CrossForumHistory,
    IpSearch,
    /// Exempt from posting limits meant for members (minimum length, flood control).
    PostingExempt,
}

/// The forums a staff member acts in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Forums(Vec<i32>),
}

impl Scope {
    pub fn contains(&self, fid: i32) -> bool {
        match self {
            Scope::All => true,
            Scope::Forums(f) => f.contains(&fid),
        }
    }

    /// For SQL: (`all`, forum ids) as in `($1 OR fid = ANY($2))`.
    pub fn sql(&self) -> (bool, Vec<i32>) {
        match self {
            Scope::All => (true, vec![]),
            Scope::Forums(f) => (false, f.clone()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Staff {
    perms: GroupPerms,
    global: bool,
    /// Forums moderated through moderator assignments (with their subforums).
    moderated: Vec<i32>,
}

impl Staff {
    pub fn new(cache: &Cache, uid: i32, groups: &[i32], perms: &GroupPerms) -> Staff {
        let global = perms.cancp || perms.issupermod;
        let moderated = if global || uid == 0 {
            vec![]
        } else {
            cache
                .moderated_forums(uid, groups, perms)
                .unwrap_or_default()
        };
        Staff {
            perms: perms.clone(),
            global,
            moderated,
        }
    }

    /// Moderates at least one forum (or everything).
    pub fn is_moderator(&self) -> bool {
        self.global || !self.moderated.is_empty()
    }

    pub fn can(&self, cap: Cap) -> bool {
        let p = &self.perms;
        if self.global {
            return true;
        }
        match cap {
            Cap::ModCp => p.canmodcp || self.is_moderator(),
            Cap::ReadModNotes => p.canviewmodnotes,
            Cap::WriteModNotes => p.canaddmodnotes,
            Cap::Warn => p.canwarnusers,
            Cap::ViewWarnLogs => p.canviewwarnlogs,
            Cap::Ban => p.canbanusers,
            Cap::PostReports => {
                (p.canmodcp && p.canmanagereportedcontent) || !self.moderated.is_empty()
            }
            Cap::MemberReports => p.canmodcp && p.canmanagereportedcontent,
            Cap::PmReports => p.canviewpmreports,
            Cap::ModLog => (p.canmodcp && p.canviewmodlogs) || !self.moderated.is_empty(),
            Cap::ModQueue => (p.canmodcp && p.canmanagemodqueue) || !self.moderated.is_empty(),
            Cap::CrossForumHistory => p.canviewallmodhistory,
            Cap::IpSearch => p.canuseipsearch,
            Cap::PostingExempt => self.is_moderator(),
        }
    }

    /// The forums `cap` applies to (meaningful for forum-bound capabilities).
    pub fn scope(&self, cap: Cap) -> Scope {
        let p = &self.perms;
        let board_wide = self.global
            || match cap {
                Cap::PostReports => p.canmodcp && p.canmanagereportedcontent,
                Cap::ModLog => p.canmodcp && p.canviewmodlogs,
                Cap::ModQueue => p.canmodcp && p.canmanagemodqueue,
                _ => self.can(cap),
            };
        if board_wide {
            Scope::All
        } else {
            Scope::Forums(self.moderated.clone())
        }
    }

    /// Which reports the viewer may see and act on.
    pub fn report_scope(&self) -> ReportScope {
        let (posts_all, post_forums) = self.scope(Cap::PostReports).sql();
        ReportScope {
            posts: self.can(Cap::PostReports),
            posts_all,
            post_forums,
            members: self.can(Cap::MemberReports),
            pms: self.can(Cap::PmReports),
        }
    }

    /// Forums whose moderation history of a member the viewer may see.
    pub fn history_scope(&self) -> Scope {
        if self.can(Cap::CrossForumHistory) {
            Scope::All
        } else {
            Scope::Forums(self.moderated.clone())
        }
    }
}

/// Which reports a staff member may see: post reports in some forums, member (profile and
/// reputation) reports, private message reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportScope {
    pub posts: bool,
    pub posts_all: bool,
    pub post_forums: Vec<i32>,
    pub members: bool,
    pub pms: bool,
}

impl ReportScope {
    /// SQL condition on `reportedcontent` (as `alias`), using parameters `$n` to `$n+4`, bound
    /// in this order: `posts`, `posts_all`, `post_forums`, `members`, `pms`.
    pub fn clause(alias: &str, n: usize) -> String {
        let a = if alias.is_empty() {
            String::new()
        } else {
            format!("{alias}.")
        };
        format!(
            "(({a}type = 'post' AND ${n} AND (${p1} OR {a}id3 = ANY(${p2})))
              OR ({a}type IN ('profile', 'reputation') AND ${p3})
              OR ({a}type = 'pm' AND ${p4}))",
            p1 = n + 1,
            p2 = n + 2,
            p3 = n + 3,
            p4 = n + 4
        )
    }

    /// Whether a report of `kind` about content in forum `id3` (posts) is visible.
    pub fn allows(&self, kind: &str, id3: i32) -> bool {
        match kind {
            "post" => self.posts && (self.posts_all || self.post_forums.contains(&id3)),
            "profile" | "reputation" => self.members,
            "pm" => self.pms,
            _ => false,
        }
    }

    pub fn any(&self) -> bool {
        self.posts || self.members || self.pms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staff(perms: GroupPerms, moderated: Vec<i32>) -> Staff {
        let global = perms.cancp || perms.issupermod;
        Staff {
            perms,
            global,
            moderated,
        }
    }

    #[test]
    fn forum_moderators_are_scoped_to_their_forums() {
        let s = staff(GroupPerms::default(), vec![3, 4]);
        assert!(s.can(Cap::ModCp));
        assert!(s.can(Cap::PostReports));
        assert_eq!(s.scope(Cap::PostReports), Scope::Forums(vec![3, 4]));
        assert_eq!(s.scope(Cap::ModLog), Scope::Forums(vec![3, 4]));
        assert_eq!(s.history_scope(), Scope::Forums(vec![3, 4]));
        // Moderating a forum is not a licence for board-wide staff features.
        for cap in [
            Cap::ReadModNotes,
            Cap::WriteModNotes,
            Cap::Warn,
            Cap::Ban,
            Cap::MemberReports,
            Cap::PmReports,
            Cap::CrossForumHistory,
            Cap::IpSearch,
        ] {
            assert!(!s.can(cap), "{cap:?}");
        }
    }

    #[test]
    fn report_scope_separates_pm_reports() {
        let fm = staff(GroupPerms::default(), vec![3]).report_scope();
        assert!(fm.allows("post", 3) && !fm.allows("post", 4));
        assert!(!fm.allows("profile", 0) && !fm.allows("pm", 0));
        let m = staff(GroupPerms::moderator(), vec![]).report_scope();
        assert!(m.allows("post", 4) && m.allows("reputation", 0) && !m.allows("pm", 0));
        let sm = staff(GroupPerms::super_moderator(), vec![]).report_scope();
        assert!(sm.allows("pm", 0));
        assert!(ReportScope::clause("r", 2).contains("r.id3 = ANY($4)"));
    }

    #[test]
    fn members_have_no_staff_capabilities() {
        let s = staff(GroupPerms::default(), vec![]);
        assert!(!s.can(Cap::ModCp));
        assert!(!s.can(Cap::PostReports));
        assert!(!s.can(Cap::ModLog));
        assert!(!s.can(Cap::PostingExempt));
    }

    #[test]
    fn moderator_group_gets_notes_but_not_pm_reports() {
        let s = staff(GroupPerms::moderator(), vec![]);
        assert!(s.can(Cap::ReadModNotes) && s.can(Cap::WriteModNotes));
        assert!(s.can(Cap::PostReports) && s.can(Cap::MemberReports));
        assert_eq!(s.scope(Cap::PostReports), Scope::All);
        assert!(!s.can(Cap::PmReports));
        assert!(!s.can(Cap::CrossForumHistory));
    }

    #[test]
    fn super_moderators_and_admins_have_everything() {
        for p in [GroupPerms::super_moderator(), GroupPerms::administrator()] {
            let s = staff(p, vec![]);
            assert!(s.can(Cap::PmReports) && s.can(Cap::CrossForumHistory));
            assert_eq!(s.scope(Cap::ModLog), Scope::All);
        }
    }
}
