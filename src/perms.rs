//! Usergroup, forum and moderator permission models.
//!
//! Permissions are stored as JSONB and deserialized into typed structs with defaults, so new
//! permissions can be added without migrations. A user's effective permissions are the
//! combination of all of their groups (primary + additional): booleans OR together, numeric
//! limits take the most generous value (where `0` means "unlimited" for limit-style fields).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combine {
    Or,
    Max,
    /// 0 = unlimited wins, otherwise max.
    ZeroUnlimited,
    Min,
}

#[derive(Serialize, Clone, Debug)]
pub struct PermMeta {
    pub name: &'static str,
    pub title: &'static str,
    pub section: &'static str,
    pub is_bool: bool,
}

macro_rules! perm_struct {
    ($name:ident, $meta:ident { $( [$section:expr] $( $field:ident : $ty:ident = $default:expr, $combine:ident, $title:expr; )* )* }) => {
        #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
        #[serde(default)]
        pub struct $name {
            $( $( pub $field: $ty, )* )*
        }
        impl Default for $name {
            fn default() -> Self {
                Self { $( $( $field: $default, )* )* }
            }
        }
        impl $name {
            /// Combine permissions of another group into self.
            pub fn merge(&mut self, o: &Self) {
                $( $( perm_struct!(@merge self.$field, o.$field, $ty, $combine); )* )*
            }
            pub fn none() -> Self {
                let mut s = Self::default();
                $( $( perm_struct!(@zero s.$field, $ty); )* )*
                s
            }
            pub fn all() -> Self {
                let mut s = Self::default();
                $( $( perm_struct!(@all s.$field, $ty, $combine); )* )*
                s
            }
        }
        pub static $meta: &[PermMeta] = &[
            $( $( PermMeta { name: stringify!($field), title: $title, section: $section, is_bool: perm_struct!(@isbool $ty) }, )* )*
        ];
    };
    (@merge $a:expr, $b:expr, bool, Or) => { $a = $a || $b; };
    (@merge $a:expr, $b:expr, i32, Max) => { $a = $a.max($b); };
    (@merge $a:expr, $b:expr, i32, Min) => { $a = $a.min($b); };
    (@merge $a:expr, $b:expr, i32, ZeroUnlimited) => { $a = if $a == 0 || $b == 0 { 0 } else { $a.max($b) }; };
    (@zero $a:expr, bool) => { $a = false; };
    (@zero $a:expr, i32) => { };
    (@all $a:expr, bool, $c:ident) => { $a = true; };
    (@all $a:expr, i32, ZeroUnlimited) => { $a = 0; };
    (@all $a:expr, i32, $c:ident) => { };
    (@isbool bool) => { true };
    (@isbool i32) => { false };
}

perm_struct!(GroupPerms, GROUP_PERM_META {
    ["Viewing"]
    canview: bool = true, Or, "Can view board?";
    canviewthreads: bool = true, Or, "Can view threads?";
    canviewprofiles: bool = true, Or, "Can view user profiles?";
    candlattachments: bool = true, Or, "Can download attachments?";
    canviewboardclosed: bool = false, Or, "Can view board when closed?";
    canviewmemberlist: bool = true, Or, "Can view member list?";
    canviewcalendar: bool = true, Or, "Can view calendar?";
    canviewonline: bool = true, Or, "Can view who's online?";
    canviewwolinvis: bool = false, Or, "Can view invisible users?";
    canviewonlineips: bool = false, Or, "Can view IP addresses on who's online?";
    cansearch: bool = true, Or, "Can search forums?";
    canviewdeletionnotice: bool = true, Or, "Can view deletion notices?";
    ["Posting"]
    canpostthreads: bool = true, Or, "Can post new threads?";
    canpostreplys: bool = true, Or, "Can post replies?";
    canpostattachments: bool = true, Or, "Can post attachments?";
    canratethreads: bool = true, Or, "Can rate threads?";
    caneditposts: bool = true, Or, "Can edit own posts?";
    candeleteposts: bool = true, Or, "Can delete own posts?";
    candeletethreads: bool = true, Or, "Can delete own threads?";
    caneditattachments: bool = true, Or, "Can update own attachments?";
    canpostpolls: bool = true, Or, "Can post polls?";
    canvotepolls: bool = true, Or, "Can vote in polls?";
    canundovotes: bool = false, Or, "Can undo votes in polls?";
    canreact: bool = true, Or, "Can react to posts?";
    modposts: bool = false, Or, "New posts require moderation?";
    modthreads: bool = false, Or, "New threads require moderation?";
    mod_edit_posts: bool = false, Or, "Edited posts require moderation?";
    modattachments: bool = false, Or, "Attachments require moderation?";
    edittimelimit: i32 = 0, ZeroUnlimited, "Edit time limit (minutes, 0 = unlimited)";
    maxposts: i32 = 0, ZeroUnlimited, "Maximum posts per day (0 = unlimited)";
    attachquota: i32 = 0, ZeroUnlimited, "Attachment quota (KB, 0 = unlimited)";
    ["Private Messaging"]
    canusepms: bool = true, Or, "Can use private messaging?";
    cansendpms: bool = true, Or, "Can send private messages?";
    cantrackpms: bool = true, Or, "Can track sent private messages?";
    candenypmreceipts: bool = true, Or, "Can deny message receipt requests?";
    canoverridepm: bool = false, Or, "Can send PMs to users who disabled them?";
    pmquota: i32 = 200, ZeroUnlimited, "Message quota (0 = unlimited)";
    maxpmrecipients: i32 = 5, ZeroUnlimited, "Max recipients per message (0 = unlimited)";
    cansendemail: bool = true, Or, "Can send email to other members?";
    cansendemailoverride: bool = false, Or, "Can email users who hide their email?";
    maxemails: i32 = 5, ZeroUnlimited, "Max emails per day (0 = unlimited)";
    ["Account"]
    canusercp: bool = true, Or, "Can access User CP?";
    canbeinvisible: bool = true, Or, "Can be invisible?";
    canuploadavatars: bool = true, Or, "Can upload avatars?";
    canchangename: bool = false, Or, "Can change username?";
    canchangewebsite: bool = true, Or, "Can change website?";
    cancustomtitle: bool = false, Or, "Can use custom user title?";
    canusesig: bool = true, Or, "Can use signature?";
    canusesigxposts: i32 = 0, Min, "Signature allowed after x posts";
    candisplaygroup: bool = true, Or, "Can set as display group?";
    canbereported: bool = true, Or, "Can be reported?";
    showinbirthdaylist: bool = true, Or, "Show in birthday list?";
    showforumteam: bool = false, Or, "Show on forum team page?";
    showmemberlist: bool = true, Or, "Show in member list?";
    ["Reputation & Warnings"]
    usereputationsystem: bool = true, Or, "Show reputation?";
    cangivereputations: bool = true, Or, "Can give reputation?";
    candeletereputations: bool = true, Or, "Can delete reputation they gave?";
    reputationpower: i32 = 1, Max, "Reputation power";
    maxreputationsday: i32 = 5, ZeroUnlimited, "Max reputations per day";
    maxreputationsperuser: i32 = 0, ZeroUnlimited, "Max reputations per user per day";
    canwarnusers: bool = false, Or, "Can warn users?";
    canreceivewarnings: bool = true, Or, "Can receive warnings?";
    maxwarningsday: i32 = 3, ZeroUnlimited, "Max warnings given per day";
    ["Calendar"]
    canaddevents: bool = true, Or, "Can post events?";
    canbypasseventmod: bool = false, Or, "Can bypass event moderation?";
    canmoderateevents: bool = false, Or, "Can moderate events?";
    ["Moderation & Administration"]
    issupermod: bool = false, Or, "Is super moderator?";
    canmodcp: bool = false, Or, "Can access Mod CP?";
    cancp: bool = false, Or, "Can access Admin CP?";
    canmanageannounce: bool = false, Or, "Can manage announcements?";
    canmanagemodqueue: bool = false, Or, "Can manage moderator queue?";
    canmanagereportedcontent: bool = false, Or, "Can manage reported content?";
    canviewmodlogs: bool = false, Or, "Can view moderator logs?";
    caneditprofiles: bool = false, Or, "Can edit user profiles?";
    canbanusers: bool = false, Or, "Can ban users?";
    canviewwarnlogs: bool = false, Or, "Can view warning logs?";
    canuseipsearch: bool = false, Or, "Can use IP search?";
    canviewmodnotes: bool = false, Or, "Can read moderator notes about members?";
    canaddmodnotes: bool = false, Or, "Can add moderator notes about members?";
    canviewpmreports: bool = false, Or, "Can see reported private messages?";
    canviewallmodhistory: bool = false, Or, "Can see members' moderation history in all forums?";
    canpostassystem: bool = false, Or, "Can post as the System account?";
});

perm_struct!(ForumPerms, FORUM_PERM_META {
    ["Viewing"]
    canview: bool = true, Or, "Can view forum?";
    canviewthreads: bool = true, Or, "Can view threads?";
    canonlyviewownthreads: bool = false, Or, "Can only view own threads?";
    candlattachments: bool = true, Or, "Can download attachments?";
    canviewdeletionnotice: bool = true, Or, "Can view deletion notices?";
    cansearch: bool = true, Or, "Can search?";
    ["Posting"]
    canpostthreads: bool = true, Or, "Can post threads?";
    canpostreplys: bool = true, Or, "Can post replies?";
    canonlyreplyownthreads: bool = false, Or, "Can only reply to own threads?";
    canpostattachments: bool = true, Or, "Can post attachments?";
    canratethreads: bool = true, Or, "Can rate threads?";
    canpostpolls: bool = true, Or, "Can post polls?";
    canvotepolls: bool = true, Or, "Can vote in polls?";
    caneditposts: bool = true, Or, "Can edit own posts?";
    candeleteposts: bool = true, Or, "Can delete own posts?";
    candeletethreads: bool = true, Or, "Can delete own threads?";
    caneditattachments: bool = true, Or, "Can update own attachments?";
    ["Moderation"]
    modposts: bool = false, Or, "Moderate new posts?";
    modthreads: bool = false, Or, "Moderate new threads?";
    mod_edit_posts: bool = false, Or, "Moderate edited posts?";
    modattachments: bool = false, Or, "Moderate attachments?";
});

perm_struct!(ModPerms, MOD_PERM_META {
    ["Post Moderation"]
    caneditposts: bool = true, Or, "Can edit posts?";
    cansoftdeleteposts: bool = true, Or, "Can soft delete posts?";
    canrestoreposts: bool = true, Or, "Can restore soft deleted posts?";
    candeleteposts: bool = true, Or, "Can delete posts?";
    canviewips: bool = true, Or, "Can view IP addresses?";
    canapproveunapproveposts: bool = true, Or, "Can approve/unapprove posts?";
    canapproveunapproveattachs: bool = true, Or, "Can approve/unapprove attachments?";
    ["Thread Moderation"]
    cansoftdeletethreads: bool = true, Or, "Can soft delete threads?";
    canrestorethreads: bool = true, Or, "Can restore soft deleted threads?";
    candeletethreads: bool = true, Or, "Can delete threads?";
    canviewunapprove: bool = true, Or, "Can view unapproved threads/posts?";
    canviewdeleted: bool = true, Or, "Can view soft deleted threads/posts?";
    canopenclosethreads: bool = true, Or, "Can open/close threads?";
    canstickunstickthreads: bool = true, Or, "Can stick/unstick threads?";
    canapproveunapprovethreads: bool = true, Or, "Can approve/unapprove threads?";
    canmanagethreads: bool = true, Or, "Can manage threads (move, merge, split, copy)?";
    canmanagepolls: bool = true, Or, "Can manage polls?";
    canpostclosedthreads: bool = true, Or, "Can post in closed threads?";
    canmovetononmodforum: bool = true, Or, "Can move to forums they don't moderate?";
    canusecustomtools: bool = true, Or, "Can use custom moderator tools?";
    ["Mod CP"]
    canmanageannouncements: bool = true, Or, "Can manage announcements?";
    canmanagereportedposts: bool = true, Or, "Can manage reported posts?";
    canviewmodlog: bool = true, Or, "Can view moderator log?";
});

impl ForumPerms {
    /// Derive forum-level permissions from a usergroup's global permissions.
    pub fn from_group(g: &GroupPerms) -> Self {
        ForumPerms {
            canview: g.canview,
            canviewthreads: g.canviewthreads,
            canonlyviewownthreads: false,
            candlattachments: g.candlattachments,
            canviewdeletionnotice: g.canviewdeletionnotice,
            cansearch: g.cansearch,
            canpostthreads: g.canpostthreads,
            canpostreplys: g.canpostreplys,
            canonlyreplyownthreads: false,
            canpostattachments: g.canpostattachments,
            canratethreads: g.canratethreads,
            canpostpolls: g.canpostpolls,
            canvotepolls: g.canvotepolls,
            caneditposts: g.caneditposts,
            candeleteposts: g.candeleteposts,
            candeletethreads: g.candeletethreads,
            caneditattachments: g.caneditattachments,
            modposts: g.modposts,
            modthreads: g.modthreads,
            mod_edit_posts: g.mod_edit_posts,
            modattachments: g.modattachments,
        }
    }

    /// Like `merge`, but "own threads only" restrictions are AND-ed (least restrictive wins)
    /// and moderation requirements are AND-ed (if any group is exempt, the user is exempt).
    pub fn merge_groups(&mut self, o: &Self) {
        let only_view = self.canonlyviewownthreads && o.canonlyviewownthreads;
        let only_reply = self.canonlyreplyownthreads && o.canonlyreplyownthreads;
        let (mp, mt, me, ma) = (
            self.modposts && o.modposts,
            self.modthreads && o.modthreads,
            self.mod_edit_posts && o.mod_edit_posts,
            self.modattachments && o.modattachments,
        );
        self.merge(o);
        self.canonlyviewownthreads = only_view;
        self.canonlyreplyownthreads = only_reply;
        self.modposts = mp;
        self.modthreads = mt;
        self.mod_edit_posts = me;
        self.modattachments = ma;
    }
}

impl GroupPerms {
    pub fn merge_groups(&mut self, o: &Self) {
        let (mp, mt, me, ma) = (
            self.modposts && o.modposts,
            self.modthreads && o.modthreads,
            self.mod_edit_posts && o.mod_edit_posts,
            self.modattachments && o.modattachments,
        );
        self.merge(o);
        self.modposts = mp;
        self.modthreads = mt;
        self.mod_edit_posts = me;
        self.modattachments = ma;
    }

    pub fn guest() -> Self {
        GroupPerms {
            canpostthreads: false,
            canpostreplys: false,
            canpostattachments: false,
            canratethreads: false,
            caneditposts: false,
            candeleteposts: false,
            candeletethreads: false,
            caneditattachments: false,
            canpostpolls: false,
            canvotepolls: false,
            canreact: false,
            canusepms: false,
            cansendpms: false,
            cantrackpms: false,
            candenypmreceipts: false,
            pmquota: 0,
            cansendemail: false,
            canusercp: false,
            canbeinvisible: false,
            canuploadavatars: false,
            canchangewebsite: false,
            canusesig: false,
            candisplaygroup: false,
            showinbirthdaylist: false,
            cangivereputations: false,
            candeletereputations: false,
            reputationpower: 0,
            canreceivewarnings: false,
            canaddevents: false,
            ..Default::default()
        }
    }

    pub fn awaiting_activation() -> Self {
        GroupPerms {
            canpostthreads: false,
            canpostreplys: false,
            canpostattachments: false,
            canpostpolls: false,
            canvotepolls: false,
            canratethreads: false,
            canreact: false,
            canusepms: false,
            cansendpms: false,
            cansendemail: false,
            canuploadavatars: false,
            cangivereputations: false,
            canaddevents: false,
            ..Default::default()
        }
    }

    pub fn banned() -> Self {
        let mut p = GroupPerms::none();
        p.canview = true;
        p.canusercp = false;
        p
    }

    pub fn moderator() -> Self {
        GroupPerms {
            canmodcp: true,
            canviewwolinvis: true,
            canviewonlineips: true,
            canwarnusers: true,
            canmanagemodqueue: true,
            canmanagereportedcontent: true,
            canviewmodlogs: true,
            canviewwarnlogs: true,
            canuseipsearch: true,
            canviewmodnotes: true,
            canaddmodnotes: true,
            showforumteam: true,
            cancustomtitle: true,
            reputationpower: 2,
            pmquota: 500,
            ..Default::default()
        }
    }

    pub fn super_moderator() -> Self {
        GroupPerms {
            issupermod: true,
            canmanageannounce: true,
            caneditprofiles: true,
            canbanusers: true,
            canviewboardclosed: true,
            canoverridepm: true,
            cansendemailoverride: true,
            canviewpmreports: true,
            canviewallmodhistory: true,
            maxwarningsday: 0,
            ..Self::moderator()
        }
    }

    pub fn administrator() -> Self {
        let mut p = GroupPerms::all();
        p.modposts = false;
        p.modthreads = false;
        p.mod_edit_posts = false;
        p.modattachments = false;
        p.canusesigxposts = 0;
        p.reputationpower = 2;
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merging() {
        let mut a = GroupPerms::guest();
        let b = GroupPerms::default();
        a.merge_groups(&b);
        assert!(a.canpostthreads);
        assert_eq!(a.pmquota, 0); // guest 0 = unlimited semantics wins
        let admin = GroupPerms::administrator();
        assert!(admin.cancp && !admin.modposts);
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn group() -> impl Strategy<Value = GroupPerms> {
            (
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
                0i32..500,
                0i32..10,
                any::<bool>(),
            )
                .prop_map(|(view, post, modposts, pmquota, rep, cp)| GroupPerms {
                    canview: view,
                    canpostthreads: post,
                    modposts,
                    pmquota,
                    reputationpower: rep,
                    cancp: cp,
                    ..GroupPerms::default()
                })
        }

        proptest! {
            /// Combining groups does not depend on their order, and adding a group never takes
            /// a permission away (except moderation requirements, which any exempt group lifts).
            #[test]
            fn merging_is_order_independent_and_monotone(a in group(), b in group(), c in group()) {
                let mut ab = a.clone(); ab.merge_groups(&b);
                let mut ba = b.clone(); ba.merge_groups(&a);
                prop_assert_eq!(&ab, &ba);
                let mut ab_c = ab.clone(); ab_c.merge_groups(&c);
                let mut bc = b.clone(); bc.merge_groups(&c);
                let mut a_bc = a.clone(); a_bc.merge_groups(&bc);
                prop_assert_eq!(&ab_c, &a_bc);
                prop_assert!(!a.canview || ab.canview);
                prop_assert!(!a.cancp || ab.cancp);
                prop_assert!(ab.modposts == (a.modposts && b.modposts));
                prop_assert!(ab.reputationpower == a.reputationpower.max(b.reputationpower));
                // 0 means unlimited and wins.
                prop_assert!((a.pmquota == 0 || b.pmquota == 0) == (ab.pmquota == 0));
            }
        }
    }
}
