//! Row types shared across modules.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(FromRow, Serialize, Deserialize, Clone, Debug, Default)]
pub struct User {
    pub uid: i32,
    pub username: String,
    #[serde(skip)]
    pub password: String,
    #[serde(skip_serializing)]
    pub email: String,
    pub usergroup: i32,
    pub additionalgroups: Vec<i32>,
    pub displaygroup: i32,
    pub usertitle: String,
    pub regdate: i64,
    pub lastactive: i64,
    pub lastvisit: i64,
    pub lastpost: i64,
    pub website: String,
    pub avatar: String,
    pub avatardimensions: String,
    pub avatartype: String,
    pub signature: String,
    pub birthday: String,
    pub birthdayprivacy: String,
    pub timezone: String,
    pub postnum: i32,
    pub threadnum: i32,
    pub reputation: i32,
    pub warningpoints: i32,
    pub moderateposts: bool,
    pub moderationtime: i64,
    pub suspendposting: bool,
    pub suspensiontime: i64,
    pub suspendsignature: bool,
    pub suspendsigtime: i64,
    #[serde(skip_serializing)]
    pub regip: String,
    #[serde(skip_serializing)]
    pub lastip: String,
    pub language: String,
    pub style: i32,
    pub away: bool,
    pub awaydate: i64,
    pub returndate: String,
    pub awayreason: String,
    pub pmnotice: bool,
    pub pmnotify: bool,
    pub receivepms: bool,
    pub receivefrombuddy: bool,
    pub buddylist: Vec<i32>,
    pub ignorelist: Vec<i32>,
    pub hideemail: bool,
    pub allownotices: bool,
    pub subscriptionmethod: i16,
    pub invisible: bool,
    pub showsigs: bool,
    pub showavatars: bool,
    pub showimages: bool,
    pub showvideos: bool,
    pub showquickreply: bool,
    pub showredirect: bool,
    pub tpp: i16,
    pub ppp: i16,
    pub threadmode: String,
    pub daysprune: i16,
    pub dateformat: String,
    pub timeformat: String,
    pub colormode: String,
    pub referrer: i32,
    pub referrals: i32,
    #[serde(skip_serializing)]
    pub usernotes: String,
    #[serde(skip_serializing)]
    pub notepad: String,
    pub pmfolders: sqlx::types::Json<Vec<PmFolder>>,
    pub unreadpms: i32,
    pub totalpms: i32,
    pub unreadalerts: i32,
    pub timeonline: i64,
    #[serde(skip)]
    pub loginattempts: i32,
    #[serde(skip)]
    pub loginlockoutexpiry: i64,
    #[serde(skip)]
    pub totp_secret: String,
    #[serde(skip)]
    pub session_version: i32,
    pub coppauser: bool,
    /// The built-in System account (see `crate::system`).
    #[sqlx(default)]
    pub is_system: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PmFolder {
    pub id: i32,
    pub name: String,
}

impl User {
    pub fn all_groups(&self) -> Vec<i32> {
        let mut g = vec![self.usergroup];
        for a in &self.additionalgroups {
            if !g.contains(a) {
                g.push(*a);
            }
        }
        g
    }
    pub fn display_group(&self) -> i32 {
        if self.displaygroup > 0 {
            self.displaygroup
        } else {
            self.usergroup
        }
    }
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct UserGroup {
    pub gid: i32,
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: i16,
    pub title: String,
    pub description: String,
    pub namestyle: String,
    pub usertitle: String,
    pub stars: i16,
    pub starimage: String,
    pub image: String,
    pub disporder: i32,
    pub isbannedgroup: bool,
    pub perms: sqlx::types::Json<crate::perms::GroupPerms>,
    /// The built-in System account's group (see `crate::system`).
    #[sqlx(default)]
    pub is_system: bool,
}

/// Forum configuration (no counters — those are read live from the DB).
#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Forum {
    pub fid: i32,
    pub name: String,
    pub description: String,
    pub linkto: String,
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: String,
    pub pid: i32,
    pub parentlist: Vec<i32>,
    pub disporder: i32,
    pub active: bool,
    pub open: bool,
    pub allowhtml: bool,
    pub allowmycode: bool,
    pub allowsmilies: bool,
    pub allowimgcode: bool,
    pub allowvideocode: bool,
    pub allowpicons: bool,
    pub allowtratings: bool,
    pub usepostcounts: bool,
    pub usethreadcounts: bool,
    pub requireprefix: bool,
    /// Argon2id verifier of the forum password ('' = none).
    #[serde(skip_serializing)]
    pub password: String,
    /// Changes whenever the password does; unlock cookies are bound to it.
    #[serde(skip_serializing)]
    pub password_version: i32,
    pub showinjump: bool,
    pub style: i32,
    pub overridestyle: bool,
    pub rulestype: i16,
    pub rulestitle: String,
    pub rules: String,
    pub defaultdatecut: i32,
    pub defaultsortby: String,
    pub defaultsortorder: String,
}

impl Forum {
    pub fn is_category(&self) -> bool {
        self.kind == "c"
    }
    pub fn has_password(&self) -> bool {
        !self.password.is_empty()
    }
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug, Default)]
pub struct ForumCounters {
    pub fid: i32,
    pub threads: i32,
    pub posts: i32,
    pub unapprovedthreads: i32,
    pub unapprovedposts: i32,
    pub deletedthreads: i32,
    pub deletedposts: i32,
    pub lastpost: i64,
    pub lastposter: String,
    pub lastposteruid: i32,
    pub lastposttid: i32,
    pub lastpostsubject: String,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug, Default)]
pub struct Thread {
    pub tid: i32,
    pub fid: i32,
    pub subject: String,
    pub prefix: i32,
    pub icon: i32,
    pub poll: i32,
    pub uid: i32,
    pub username: String,
    pub dateline: i64,
    pub firstpost: i32,
    pub lastpost: i64,
    pub lastposter: String,
    pub lastposteruid: i32,
    pub views: i32,
    pub replies: i32,
    pub closed: String,
    pub sticky: bool,
    pub numratings: i32,
    pub totalratings: i32,
    pub notes: String,
    pub visible: i16,
    pub unapprovedposts: i32,
    pub deletedposts: i32,
    pub attachmentcount: i32,
    pub deletetime: i64,
    pub redirect_expires: i64,
}

impl Thread {
    pub fn is_closed(&self) -> bool {
        self.closed == "1"
    }
    pub fn moved_to(&self) -> Option<i32> {
        self.closed
            .strip_prefix("moved|")
            .and_then(|s| s.parse().ok())
    }
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug, Default)]
pub struct Post {
    pub pid: i32,
    pub tid: i32,
    pub replyto: i32,
    pub fid: i32,
    pub subject: String,
    pub icon: i32,
    pub uid: i32,
    pub username: String,
    pub dateline: i64,
    pub message: String,
    pub message_html: String,
    pub parser_rev: i32,
    pub ipaddress: String,
    pub includesig: bool,
    pub smilieoff: bool,
    pub edituid: i32,
    pub edittime: i64,
    pub editreason: String,
    pub visible: i16,
}

pub const POST_COLUMNS: &str = "pid, tid, replyto, fid, subject, icon, uid, username, dateline, message, message_html, parser_rev, ipaddress, includesig, smilieoff, edituid, edittime, editreason, visible";

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Theme {
    pub tid: i32,
    pub name: String,
    pub pid: i32,
    pub def: bool,
    pub properties: sqlx::types::Json<serde_json::Value>,
    pub stylesheet: String,
    pub allowedgroups: Vec<i32>,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Icon {
    pub iid: i32,
    pub name: String,
    pub path: String,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Prefix {
    pub pid: i32,
    pub prefix: String,
    pub displaystyle: String,
    pub forums: Vec<i32>,
    pub groups: Vec<i32>,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Moderator {
    pub mid: i32,
    pub fid: i32,
    pub id: i32,
    pub isgroup: bool,
    pub perms: sqlx::types::Json<crate::perms::ModPerms>,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct AttachType {
    pub atid: i32,
    pub name: String,
    pub mimetype: String,
    pub extension: String,
    pub maxsize: i32,
    pub icon: String,
    pub enabled: bool,
    pub groups: Vec<i32>,
    pub forums: Vec<i32>,
    pub avatarfile: bool,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct ProfileField {
    pub fid: i32,
    pub name: String,
    pub description: String,
    pub disporder: i32,
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub kind: String,
    pub options: String,
    pub regex: String,
    pub length: i32,
    pub maxlength: i32,
    pub required: bool,
    pub registration: bool,
    pub profile: bool,
    pub postbit: bool,
    pub viewableby: Vec<i32>,
    pub editableby: Vec<i32>,
    pub postnum: i32,
    pub allowhtml: bool,
    pub allowmycode: bool,
    pub allowsmilies: bool,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct UserTitle {
    pub utid: i32,
    pub posts: i32,
    pub title: String,
    pub stars: i16,
    pub starimage: String,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct SmilieRow {
    pub sid: i32,
    pub name: String,
    pub find: String,
    pub image: String,
    pub disporder: i32,
    pub showclickable: bool,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Announcement {
    pub aid: i32,
    pub fid: i32,
    pub uid: i32,
    pub subject: String,
    pub message: String,
    pub startdate: i64,
    pub enddate: i64,
    pub allowhtml: bool,
    pub allowmycode: bool,
    pub allowsmilies: bool,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct ReportReason {
    pub rid: i32,
    pub title: String,
    pub appliesto: String,
    pub extra: bool,
    pub disporder: i32,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Calendar {
    pub cid: i32,
    pub name: String,
    pub disporder: i32,
    pub startofweek: i16,
    pub showbirthdays: bool,
    pub eventlimit: i32,
    pub moderation: bool,
    pub allowhtml: bool,
    pub allowmycode: bool,
    pub allowimgcode: bool,
    pub allowvideocode: bool,
    pub allowsmilies: bool,
}
