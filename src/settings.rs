//! Board settings registry (the MyBB "Board Settings" ACP). Metadata and defaults live in code;
//! the `settings` table stores the values chosen by the administrator.

use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize, Clone, Copy, Debug)]
pub struct SettingGroup {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

#[derive(Serialize, Clone, Copy, Debug)]
pub struct SettingDef {
    pub group: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// yesno | text | textarea | numeric | select:a=A,b=B | password
    pub kind: &'static str,
    pub default: &'static str,
}

pub static GROUPS: &[SettingGroup] = &[
    SettingGroup {
        name: "general",
        title: "General Configuration",
        description: "Board name, URL, contact details and closing the board.",
    },
    SettingGroup {
        name: "datetime",
        title: "Date and Time Formats",
        description: "Default date/time formats and timezone.",
    },
    SettingGroup {
        name: "forumhome",
        title: "Forum Home Options",
        description: "What is shown on the board index.",
    },
    SettingGroup {
        name: "forumdisplay",
        title: "Forum Display Options",
        description: "Thread listing behaviour.",
    },
    SettingGroup {
        name: "showthread",
        title: "Show Thread Options",
        description: "Thread display behaviour.",
    },
    SettingGroup {
        name: "member",
        title: "User Registration and Profile Options",
        description: "Registration, login and profile rules.",
    },
    SettingGroup {
        name: "posting",
        title: "Posting",
        description: "Message length, flood control, editing, polls.",
    },
    SettingGroup {
        name: "attachments",
        title: "Attachment Settings",
        description: "Uploads and thumbnails.",
    },
    SettingGroup {
        name: "pm",
        title: "Private Messaging",
        description: "Private messaging system.",
    },
    SettingGroup {
        name: "search",
        title: "Search System",
        description: "Search behaviour and flood control.",
    },
    SettingGroup {
        name: "memberlist",
        title: "Member List",
        description: "Member list options.",
    },
    SettingGroup {
        name: "reputation",
        title: "Reputation",
        description: "Reputation system.",
    },
    SettingGroup {
        name: "warning",
        title: "Warning System",
        description: "Warning system.",
    },
    SettingGroup {
        name: "online",
        title: "Who's Online",
        description: "Online tracking.",
    },
    SettingGroup {
        name: "calendar",
        title: "Calendar",
        description: "Calendar system.",
    },
    SettingGroup {
        name: "portal",
        title: "Portal",
        description: "Portal page.",
    },
    SettingGroup {
        name: "mail",
        title: "Mail Settings",
        description: "Outgoing email transport.",
    },
    SettingGroup {
        name: "security",
        title: "Security & Anti-Spam",
        description: "Captcha, security questions, rate limits.",
    },
    SettingGroup {
        name: "features",
        title: "Features",
        description: "Alerts, reactions, syndication, archive, statistics.",
    },
    SettingGroup {
        name: "contact",
        title: "Contact Page",
        description: "Contact form.",
    },
    SettingGroup {
        name: "system",
        title: "System Account",
        description: "What the built-in System account does automatically.",
    },
];

macro_rules! s {
    ($g:expr, $n:expr, $t:expr, $d:expr, $k:expr, $def:expr) => {
        SettingDef {
            group: $g,
            name: $n,
            title: $t,
            description: $d,
            kind: $k,
            default: $def,
        }
    };
}

pub static DEFS: &[SettingDef] = &[
    // general
    s!(
        "general",
        "debugpanel",
        "Admin debug panel",
        "Show administrators page timing and every database query at the bottom of each page.",
        "yesno",
        "1"
    ),
    s!(
        "general",
        "bbname",
        "Board Name",
        "The name of your community.",
        "text",
        "rbb Community Forums"
    ),
    s!(
        "general",
        "bburl",
        "Board URL",
        "The full URL of the board, without trailing slash.",
        "text",
        "http://127.0.0.1:8080"
    ),
    s!(
        "general",
        "homename",
        "Homepage Name",
        "Name of your website's homepage.",
        "text",
        "Home"
    ),
    s!(
        "general",
        "homeurl",
        "Homepage URL",
        "Full URL of your homepage.",
        "text",
        "/"
    ),
    s!(
        "general",
        "adminemail",
        "Admin Email",
        "Administrator's email address (used as the From address).",
        "text",
        "admin@example.com"
    ),
    s!(
        "general",
        "boardclosed",
        "Board Closed",
        "Close the board for maintenance (administrators can still browse).",
        "yesno",
        "0"
    ),
    s!(
        "general",
        "boardclosed_reason",
        "Board Closed Reason",
        "Message shown while the board is closed.",
        "textarea",
        "These forums are currently closed for maintenance. Please check back later."
    ),
    s!(
        "general",
        "seourls",
        "SEO Friendly URLs",
        "Include a slug of the title in forum/thread/user links.",
        "yesno",
        "1"
    ),
    s!(
        "general",
        "gzipoutput",
        "Compress Output",
        "Compress pages with gzip/brotli.",
        "yesno",
        "1"
    ),
    s!(
        "general",
        "tos",
        "Terms of Service",
        "Board rules shown during registration (MyCode allowed).",
        "textarea",
        "By registering you agree to be bound by the rules of this board. Be respectful, stay on topic, and do not post spam or illegal content. Administrators and moderators may remove any content at their discretion."
    ),
    s!(
        "general",
        "privacypolicy",
        "Privacy Policy",
        "Privacy policy (MyCode allowed). Leave empty to hide the page.",
        "textarea",
        "We store the information you provide when registering (username, email address, password hash) and your posts. IP addresses are logged for moderation and anti-abuse purposes. You may request deletion of your account at any time."
    ),
    // datetime
    s!(
        "datetime",
        "dateformat",
        "Date Format",
        "chrono strftime format for dates.",
        "text",
        "%m-%d-%Y"
    ),
    s!(
        "datetime",
        "timeformat",
        "Time Format",
        "chrono strftime format for times.",
        "text",
        "%I:%M %p"
    ),
    s!(
        "datetime",
        "regdateformat",
        "Registered Date Format",
        "Format used for join dates.",
        "text",
        "%b %Y"
    ),
    s!(
        "datetime",
        "timezone",
        "Default Timezone",
        "IANA timezone used for guests and new users (e.g. America/New_York).",
        "text",
        "UTC"
    ),
    // forumhome
    s!(
        "forumhome",
        "showdescriptions",
        "Show Forum Descriptions?",
        "",
        "yesno",
        "1"
    ),
    s!(
        "forumhome",
        "subforumsindex",
        "Subforums to show on index",
        "Number of subforums listed under each forum (0 to disable).",
        "numeric",
        "5"
    ),
    s!(
        "forumhome",
        "showbirthdays",
        "Show Today's Birthdays?",
        "",
        "yesno",
        "1"
    ),
    s!(
        "forumhome",
        "showwol",
        "Show Who's Online?",
        "",
        "yesno",
        "1"
    ),
    s!(
        "forumhome",
        "showindexstats",
        "Show Board Statistics?",
        "",
        "yesno",
        "1"
    ),
    s!(
        "forumhome",
        "showforumviewing",
        "Show number of users viewing each forum?",
        "",
        "yesno",
        "0"
    ),
    s!(
        "forumhome",
        "collapsecategories",
        "Collapsible categories",
        "Allow users to collapse categories.",
        "yesno",
        "1"
    ),
    // forumdisplay
    s!(
        "forumdisplay",
        "threadsperpage",
        "Threads Per Page",
        "",
        "numeric",
        "20"
    ),
    s!(
        "forumdisplay",
        "hottopic",
        "Replies for Hot Topic",
        "",
        "numeric",
        "20"
    ),
    s!(
        "forumdisplay",
        "hottopicviews",
        "Views for Hot Topic",
        "",
        "numeric",
        "150"
    ),
    s!(
        "forumdisplay",
        "announcementlimit",
        "Announcements Limit",
        "Max announcements shown above thread listing.",
        "numeric",
        "2"
    ),
    s!(
        "forumdisplay",
        "browsingthisforum",
        "Users Browsing this Forum",
        "",
        "yesno",
        "1"
    ),
    s!(
        "forumdisplay",
        "dotfolders",
        "Use 'dot' Icons",
        "Show which threads you have posted in.",
        "yesno",
        "1"
    ),
    s!(
        "forumdisplay",
        "allowthreadratings",
        "Use Thread Ratings?",
        "",
        "yesno",
        "1"
    ),
    s!(
        "forumdisplay",
        "showthreadpreview",
        "Thread previews on hover",
        "Show the first post preview when hovering a thread title.",
        "yesno",
        "1"
    ),
    // showthread
    s!(
        "showthread",
        "postsperpage",
        "Posts Per Page",
        "",
        "numeric",
        "20"
    ),
    s!(
        "showthread",
        "userppoptions",
        "User Selectable Posts Per Page",
        "Comma separated options.",
        "text",
        "5,10,20,25,30,40,50"
    ),
    s!(
        "showthread",
        "postlayout",
        "Post Layout",
        "Classic (author on left) or horizontal.",
        "select:classic=Classic,horizontal=Horizontal",
        "classic"
    ),
    s!(
        "showthread",
        "quickreply",
        "Show Quick Reply Form",
        "",
        "yesno",
        "1"
    ),
    s!(
        "showthread",
        "multiquote",
        "Show Multiquote Buttons",
        "",
        "yesno",
        "1"
    ),
    s!(
        "showthread",
        "showsimilarthreads",
        "Show Similar Threads Table",
        "",
        "yesno",
        "1"
    ),
    s!(
        "showthread",
        "similarlimit",
        "Similar Threads Limit",
        "",
        "numeric",
        "5"
    ),
    s!(
        "showthread",
        "threadreadcut",
        "Read Threads in Database (Days)",
        "How long thread read markers are kept.",
        "numeric",
        "7"
    ),
    s!(
        "showthread",
        "showeditedby",
        "Show 'edited by' Messages",
        "",
        "yesno",
        "1"
    ),
    s!(
        "showthread",
        "showeditedbyadmin",
        "Show 'edited by' for moderators",
        "",
        "yesno",
        "1"
    ),
    s!(
        "showthread",
        "delayedthreadviews",
        "Delayed Thread View Updates",
        "Batch thread view counter updates (recommended for large boards).",
        "yesno",
        "1"
    ),
    s!(
        "showthread",
        "livethreadupdates",
        "Live thread updates",
        "Push new replies to readers in real time (Server-Sent Events).",
        "yesno",
        "1"
    ),
    // member
    s!(
        "member",
        "disableregs",
        "Disable Registrations",
        "",
        "yesno",
        "0"
    ),
    s!(
        "member",
        "regtype",
        "Registration Method",
        "",
        "select:instant=Instant Activation,verify=Send Email Verification,randompass=Send Random Password,admin=Administrator Activation,both=Email Verification & Admin Activation",
        "instant"
    ),
    s!(
        "member",
        "minnamelength",
        "Minimum Username Length",
        "",
        "numeric",
        "3"
    ),
    s!(
        "member",
        "maxnamelength",
        "Maximum Username Length",
        "",
        "numeric",
        "30"
    ),
    s!(
        "member",
        "minpasswordlength",
        "Minimum Password Length",
        "",
        "numeric",
        "8"
    ),
    s!(
        "member",
        "requirecomplexpasswords",
        "Require Complex Passwords",
        "Require upper case, lower case and a number.",
        "yesno",
        "0"
    ),
    s!(
        "member",
        "betweenregstime",
        "Time Between Registrations (hours)",
        "Window for registrations from the same IP.",
        "numeric",
        "24"
    ),
    s!(
        "member",
        "maxregsbetweentime",
        "Max Registrations per IP in window",
        "0 to disable.",
        "numeric",
        "5"
    ),
    s!(
        "member",
        "allowmultipleemails",
        "Allow emails to be registered multiple times",
        "",
        "yesno",
        "0"
    ),
    s!(
        "member",
        "usereferrals",
        "Use Referrals System",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "failedlogincount",
        "Number of times to allow failed logins",
        "0 = unlimited.",
        "numeric",
        "5"
    ),
    s!(
        "member",
        "failedlogintime",
        "Lockout time after failed logins (minutes)",
        "",
        "numeric",
        "15"
    ),
    s!(
        "member",
        "usernamemethod",
        "Login Method",
        "",
        "select:0=Username Only,1=Email Only,2=Username or Email",
        "2"
    ),
    s!("member", "allowaway", "Allow Away Status", "", "yesno", "1"),
    s!(
        "member",
        "maxsiglines",
        "Maximum Signature Lines",
        "0 = unlimited.",
        "numeric",
        "5"
    ),
    s!(
        "member",
        "siglength",
        "Signature Length Limit",
        "",
        "numeric",
        "500"
    ),
    s!(
        "member",
        "sigmycode",
        "Allow MyCode in Signatures",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "sigsmilies",
        "Allow Smilies in Signatures",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "sigimgcode",
        "Allow [img] in Signatures",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "avatarsize",
        "Max Uploaded Avatar Size (KB)",
        "",
        "numeric",
        "256"
    ),
    s!(
        "member",
        "maxavatardims",
        "Maximum Avatar Dimensions",
        "WxH; uploaded avatars are resized.",
        "text",
        "100x100"
    ),
    s!(
        "member",
        "allowremoteavatars",
        "Allow Remote Avatars",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "allowgravatar",
        "Allow Gravatar",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "customtitlemaxlength",
        "Custom User Title Max Length",
        "",
        "numeric",
        "40"
    ),
    s!(
        "member",
        "allowbuddyonly",
        "Allow 'buddy only' PMs",
        "",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "usertppoptions",
        "User Selectable Threads Per Page",
        "",
        "text",
        "10,20,25,30,40,50"
    ),
    s!(
        "member",
        "allowusernamechange",
        "Username change cooldown (days)",
        "Minimum days between username changes for users allowed to change them.",
        "numeric",
        "30"
    ),
    s!(
        "member",
        "allowaccountdeletion",
        "Allow users to delete their account",
        "Self-service account deletion from the User CP.",
        "yesno",
        "1"
    ),
    // posting
    s!(
        "posting",
        "maxmessagelength",
        "Maximum Message Length",
        "Characters (0 = unlimited).",
        "numeric",
        "65535"
    ),
    s!(
        "posting",
        "minmessagelength",
        "Minimum Message Length",
        "",
        "numeric",
        "2"
    ),
    s!(
        "posting",
        "subjectlength",
        "Maximum Subject Length",
        "",
        "numeric",
        "120"
    ),
    s!(
        "posting",
        "postfloodcheck",
        "Post Flood Checking",
        "",
        "yesno",
        "1"
    ),
    s!(
        "posting",
        "postfloodsecs",
        "Post Flood Time (seconds)",
        "",
        "numeric",
        "15"
    ),
    s!(
        "posting",
        "postmergemins",
        "Automatic Post Merge (minutes)",
        "Merge consecutive posts by the same user within this time (0 = disabled).",
        "numeric",
        "0"
    ),
    s!(
        "posting",
        "maxpostimages",
        "Maximum Images per Post",
        "0 = unlimited.",
        "numeric",
        "0"
    ),
    s!(
        "posting",
        "maxpostvideos",
        "Maximum Videos per Post",
        "0 = unlimited.",
        "numeric",
        "0"
    ),
    s!(
        "posting",
        "edittimelimit",
        "Edit Time Limit (minutes)",
        "0 = unlimited. Group limit overrides.",
        "numeric",
        "0"
    ),
    s!(
        "posting",
        "maxquotedepth",
        "Maximum Nested Quote Depth",
        "0 = unlimited.",
        "numeric",
        "5"
    ),
    s!(
        "posting",
        "polloptionlimit",
        "Max Poll Option Length",
        "",
        "numeric",
        "250"
    ),
    s!(
        "posting",
        "maxpolloptions",
        "Maximum Number of Poll Options",
        "",
        "numeric",
        "20"
    ),
    s!(
        "posting",
        "soft_delete",
        "Soft Delete",
        "Deleting a post/thread by its author soft deletes it (moderators can restore).",
        "yesno",
        "1"
    ),
    s!(
        "posting",
        "savedrafts",
        "Allow saving drafts",
        "",
        "yesno",
        "1"
    ),
    s!(
        "posting",
        "autosavedrafts",
        "Autosave editor contents locally",
        "Save editor contents in the browser while typing.",
        "yesno",
        "1"
    ),
    s!(
        "posting",
        "mycodemessagelength",
        "Count MyCode towards message length",
        "",
        "yesno",
        "1"
    ),
    s!(
        "posting",
        "keepedithistory",
        "Keep edit history",
        "Store previous versions of edited posts.",
        "yesno",
        "1"
    ),
    s!(
        "posting",
        "linknofollow",
        "Add rel=nofollow to links",
        "",
        "yesno",
        "1"
    ),
    // attachments
    s!(
        "attachments",
        "enableattachments",
        "Enable Attachments",
        "",
        "yesno",
        "1"
    ),
    s!(
        "attachments",
        "maxattachments",
        "Maximum Attachments Per Post",
        "",
        "numeric",
        "5"
    ),
    s!(
        "attachments",
        "attachthumbnails",
        "Show Attached Thumbnails in Posts",
        "",
        "select:yes=Thumbnails,no=Full size,download=Download link",
        "yes"
    ),
    s!(
        "attachments",
        "attachthumbw",
        "Thumbnail Max Width",
        "",
        "numeric",
        "300"
    ),
    s!(
        "attachments",
        "attachthumbh",
        "Thumbnail Max Height",
        "",
        "numeric",
        "300"
    ),
    // pm
    s!(
        "pm",
        "enablepms",
        "Enable Private Messaging",
        "",
        "yesno",
        "1"
    ),
    s!(
        "pm",
        "pmsallowmycode",
        "Allow MyCode in PMs",
        "",
        "yesno",
        "1"
    ),
    s!(
        "pm",
        "pmsallowsmilies",
        "Allow Smilies in PMs",
        "",
        "yesno",
        "1"
    ),
    s!(
        "pm",
        "pmsallowimgcode",
        "Allow [img] in PMs",
        "",
        "yesno",
        "1"
    ),
    s!(
        "pm",
        "pmsallowvideocode",
        "Allow [video] in PMs",
        "",
        "yesno",
        "1"
    ),
    s!(
        "pm",
        "pmfloodsecs",
        "PM Flood Time (seconds)",
        "",
        "numeric",
        "20"
    ),
    s!("pm", "pmsperpage", "Messages per page", "", "numeric", "20"),
    // search
    s!(
        "search",
        "searchfloodtime",
        "Search Flood Time (seconds)",
        "",
        "numeric",
        "10"
    ),
    s!(
        "search",
        "minsearchword",
        "Minimum Search Word Length",
        "",
        "numeric",
        "3"
    ),
    s!(
        "search",
        "searchhardlimit",
        "Maximum search results",
        "",
        "numeric",
        "1000"
    ),
    s!(
        "search",
        "searchresultsperpage",
        "Results per page",
        "",
        "numeric",
        "20"
    ),
    // memberlist
    s!(
        "memberlist",
        "enablememberlist",
        "Enable Member List",
        "",
        "yesno",
        "1"
    ),
    s!(
        "memberlist",
        "membersperpage",
        "Members Per Page",
        "",
        "numeric",
        "20"
    ),
    s!(
        "memberlist",
        "default_mlsort",
        "Default Sort Field",
        "",
        "select:username=Username,regdate=Registration date,postnum=Posts,lastvisit=Last visit,reputation=Reputation",
        "regdate"
    ),
    // reputation
    s!(
        "reputation",
        "enablereputation",
        "Enable Reputation System",
        "",
        "yesno",
        "1"
    ),
    s!(
        "reputation",
        "posrep",
        "Allow Positive Reputation",
        "",
        "yesno",
        "1"
    ),
    s!(
        "reputation",
        "negrep",
        "Allow Negative Reputation",
        "",
        "yesno",
        "1"
    ),
    s!(
        "reputation",
        "neurep",
        "Allow Neutral Reputation",
        "",
        "yesno",
        "1"
    ),
    s!(
        "reputation",
        "postrep",
        "Allow Post Reputation",
        "Reputation tied to specific posts.",
        "yesno",
        "1"
    ),
    s!(
        "reputation",
        "multirep",
        "Allow Multiple Reputation",
        "Rate the same user for different posts.",
        "yesno",
        "1"
    ),
    s!(
        "reputation",
        "repsperpage",
        "Reputation Comments Per Page",
        "",
        "numeric",
        "15"
    ),
    s!(
        "reputation",
        "maxreplength",
        "Max Reputation Comment Length",
        "",
        "numeric",
        "300"
    ),
    // warning
    s!(
        "warning",
        "enablewarningsystem",
        "Enable Warning System",
        "",
        "yesno",
        "1"
    ),
    s!(
        "warning",
        "allowcustomwarnings",
        "Allow Custom Warning Types",
        "",
        "yesno",
        "1"
    ),
    s!(
        "warning",
        "canviewownwarning",
        "Users can view own warnings",
        "",
        "yesno",
        "1"
    ),
    s!(
        "warning",
        "maxwarningpoints",
        "Maximum Warning Points",
        "",
        "numeric",
        "10"
    ),
    s!(
        "warning",
        "allowwarningsnopost",
        "Allow warnings not tied to posts",
        "",
        "yesno",
        "1"
    ),
    // online
    s!(
        "online",
        "wolcutoffmins",
        "Cut-off Time (minutes)",
        "",
        "numeric",
        "15"
    ),
    s!(
        "online",
        "wolorder",
        "Order users by",
        "",
        "select:username=Username,activity=Activity",
        "activity"
    ),
    // calendar
    s!(
        "calendar",
        "enablecalendar",
        "Enable Calendar",
        "",
        "yesno",
        "1"
    ),
    // portal
    s!("portal", "portal", "Enable Portal", "", "yesno", "1"),
    s!(
        "portal",
        "portal_announcementsfid",
        "Announcement Forums",
        "Comma separated forum ids whose threads appear as portal announcements (empty = all).",
        "text",
        ""
    ),
    s!(
        "portal",
        "portal_numannouncements",
        "Number of Announcements",
        "",
        "numeric",
        "10"
    ),
    s!(
        "portal",
        "portal_showwelcome",
        "Show Welcome Box",
        "",
        "yesno",
        "1"
    ),
    s!("portal", "portal_showpms", "Show PMs Box", "", "yesno", "1"),
    s!(
        "portal",
        "portal_showstats",
        "Show Statistics Box",
        "",
        "yesno",
        "1"
    ),
    s!(
        "portal",
        "portal_showwol",
        "Show Who's Online Box",
        "",
        "yesno",
        "1"
    ),
    s!(
        "portal",
        "portal_showsearch",
        "Show Search Box",
        "",
        "yesno",
        "1"
    ),
    s!(
        "portal",
        "portal_showdiscussions",
        "Show Latest Discussions",
        "",
        "yesno",
        "1"
    ),
    s!(
        "portal",
        "portal_showdiscussionsnum",
        "Number of Latest Discussions",
        "",
        "numeric",
        "10"
    ),
    // mail
    s!(
        "mail",
        "mail_handler",
        "Mail Handler",
        "",
        "select:log=Log only (development),smtp=SMTP",
        "log"
    ),
    s!(
        "mail",
        "smtp_host",
        "SMTP Hostname",
        "",
        "text",
        "localhost"
    ),
    s!("mail", "smtp_port", "SMTP Port", "", "numeric", "587"),
    s!("mail", "smtp_user", "SMTP Username", "", "text", ""),
    s!("mail", "smtp_pass", "SMTP Password", "", "password", ""),
    s!(
        "mail",
        "secure_smtp",
        "SMTP Encryption",
        "",
        "select:none=None,starttls=STARTTLS,tls=TLS",
        "starttls"
    ),
    s!(
        "mail",
        "mail_logging",
        "Mail Logging",
        "Log emails sent via the board (member-to-member, contact).",
        "yesno",
        "1"
    ),
    // security
    s!(
        "security",
        "captchaimage",
        "CAPTCHA Images for Registration & Posting",
        "",
        "select:0=No CAPTCHA,1=Built-in CAPTCHA",
        "1"
    ),
    s!(
        "security",
        "guestcaptcha",
        "Require CAPTCHA for guest posting",
        "",
        "yesno",
        "1"
    ),
    s!(
        "security",
        "securityquestion",
        "Use Security Questions at registration",
        "",
        "yesno",
        "0"
    ),
    s!(
        "security",
        "honeypot",
        "Registration Honeypot",
        "Hidden form field that bots fill in.",
        "yesno",
        "1"
    ),
    s!(
        "security",
        "minregtime",
        "Minimum Registration Time (seconds)",
        "Reject registration forms submitted faster than this.",
        "numeric",
        "3"
    ),
    s!(
        "security",
        "ratelimit_requests",
        "Requests per minute per IP",
        "Global request rate limit (0 = disabled).",
        "numeric",
        "600"
    ),
    s!(
        "security",
        "acp2fa",
        "Require 2FA for Admin CP",
        "Administrators must set up TOTP two-factor authentication to use the Admin CP.",
        "yesno",
        "0"
    ),
    s!(
        "security",
        "loginsessionlength",
        "Login duration (days)",
        "How long 'remember me' logins last.",
        "numeric",
        "30"
    ),
    // features
    s!(
        "features",
        "enablealerts",
        "Enable Alerts",
        "Notifications for quotes, replies, PMs, reputation, mentions.",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "enablereactions",
        "Enable Post Reactions",
        "",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "reactiontypes",
        "Reaction Types",
        "Comma separated name=emoji pairs.",
        "text",
        "like=👍,love=❤️,laugh=😂,wow=😮,sad=😢,thanks=🙏"
    ),
    s!(
        "features",
        "enablementions",
        "Enable @mentions",
        "",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "enablesyndication",
        "Enable RSS/Atom syndication",
        "",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "syndicationitems",
        "Items in feeds",
        "",
        "numeric",
        "20"
    ),
    s!(
        "features",
        "enablearchive",
        "Enable Lite (archive) mode",
        "",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "statsenabled",
        "Enable Statistics Page",
        "",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "statstopcount",
        "Statistics top N",
        "",
        "numeric",
        "5"
    ),
    s!(
        "features",
        "enablehelp",
        "Enable Help Documents",
        "",
        "yesno",
        "1"
    ),
    s!(
        "features",
        "enableshowteam",
        "Enable Forum Team page",
        "",
        "yesno",
        "1"
    ),
    // contact
    s!(
        "member",
        "banappeals",
        "Allow Ban Appeals",
        "Banned members can ask staff to review their ban from the banned page.",
        "yesno",
        "1"
    ),
    s!(
        "member",
        "banappeal_cooldown_days",
        "Days Before Appealing Again",
        "After a rejected appeal, how long a member must wait before appealing the same ban again. 0 allows one appeal per ban.",
        "numeric",
        "30"
    ),
    s!(
        "contact",
        "contact",
        "Enable Contact Page",
        "",
        "yesno",
        "1"
    ),
    s!(
        "contact",
        "contact_guests",
        "Disable Contact Page for Guests",
        "",
        "yesno",
        "0"
    ),
    s!(
        "contact",
        "contactemail",
        "Contact Email",
        "Where contact form emails are delivered (empty = admin email).",
        "text",
        ""
    ),
    s!(
        "system",
        "system_welcome_pm",
        "Welcome New Members",
        "System sends new members a private message when their account becomes active.",
        "yesno",
        "0"
    ),
    s!(
        "system",
        "system_welcome_subject",
        "Welcome Message Subject",
        "Placeholders: {username}, {boardname}.",
        "text",
        "Welcome to {boardname}!"
    ),
    s!(
        "system",
        "system_welcome_message",
        "Welcome Message",
        "MyCode is allowed. Placeholders: {username}, {boardname}.",
        "textarea",
        "Hi {username},\n\nWelcome to {boardname}! Take a moment to read the forum rules, then say hello.\n\nThis is an automated message, so replies aren't read. If you need help, use the contact page."
    ),
    s!(
        "system",
        "system_autoclose_days",
        "Close Inactive Threads After (days)",
        "System closes threads with no new posts for this many days. Sticky threads are left open. 0 turns this off.",
        "numeric",
        "0"
    ),
    s!(
        "system",
        "system_autoclose_forums",
        "Close Inactive Threads In",
        "Comma-separated forum IDs (subforums are not included automatically). Empty means every forum.",
        "text",
        ""
    ),
    s!(
        "system",
        "system_log_expiries",
        "Log Expiries",
        "Record lifted bans, ended suspensions and expired warnings in the moderator log as System.",
        "yesno",
        "1"
    ),
];

#[derive(Clone, Debug, Default)]
pub struct Settings {
    map: HashMap<String, String>,
}

impl Settings {
    pub fn from_rows(rows: Vec<(String, String)>) -> Self {
        let mut map: HashMap<String, String> = DEFS
            .iter()
            .map(|d| (d.name.to_string(), d.default.to_string()))
            .collect();
        for (k, v) in rows {
            map.insert(k, v);
        }
        Settings { map }
    }
    pub fn get(&self, k: &str) -> &str {
        self.map.get(k).map(String::as_str).unwrap_or("")
    }
    pub fn bool(&self, k: &str) -> bool {
        matches!(self.get(k), "1" | "yes" | "true" | "on")
    }
    pub fn int(&self, k: &str) -> i64 {
        self.get(k).trim().parse().unwrap_or(0)
    }
    pub fn map(&self) -> &HashMap<String, String> {
        &self.map
    }
    /// Settings safe to expose to templates (no secrets).
    pub fn public_map(&self) -> HashMap<String, String> {
        self.map
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "smtp_pass" | "smtp_user"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

pub fn def(name: &str) -> Option<&'static SettingDef> {
    DEFS.iter().find(|d| d.name == name)
}
