-- rbb initial schema. Timestamps are unix seconds (BIGINT), matching MyBB's `dateline` convention.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE settings (
    name        TEXT PRIMARY KEY,
    value       TEXT NOT NULL
);

CREATE TABLE usergroups (
    gid          SERIAL PRIMARY KEY,
    type         SMALLINT NOT NULL DEFAULT 2,          -- 1 = core, 2 = custom, 3 = public (join freely), 4 = public (requests)
    title        TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    namestyle    TEXT NOT NULL DEFAULT '{username}',
    usertitle    TEXT NOT NULL DEFAULT '',
    stars        SMALLINT NOT NULL DEFAULT 0,
    starimage    TEXT NOT NULL DEFAULT '',
    image        TEXT NOT NULL DEFAULT '',
    disporder    INT NOT NULL DEFAULT 0,
    isbannedgroup BOOLEAN NOT NULL DEFAULT FALSE,
    perms        JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE users (
    uid            SERIAL PRIMARY KEY,
    username       TEXT NOT NULL,
    password       TEXT NOT NULL,
    email          TEXT NOT NULL,
    usergroup      INT NOT NULL REFERENCES usergroups(gid),
    additionalgroups INT[] NOT NULL DEFAULT '{}',
    displaygroup   INT NOT NULL DEFAULT 0,
    usertitle      TEXT NOT NULL DEFAULT '',
    regdate        BIGINT NOT NULL,
    lastactive     BIGINT NOT NULL DEFAULT 0,
    lastvisit      BIGINT NOT NULL DEFAULT 0,
    lastpost       BIGINT NOT NULL DEFAULT 0,
    website        TEXT NOT NULL DEFAULT '',
    avatar         TEXT NOT NULL DEFAULT '',
    avatardimensions TEXT NOT NULL DEFAULT '',
    avatartype     TEXT NOT NULL DEFAULT '',
    signature      TEXT NOT NULL DEFAULT '',
    birthday       TEXT NOT NULL DEFAULT '',             -- d-m-yyyy (year optional) like MyBB
    birthdayprivacy TEXT NOT NULL DEFAULT 'all',
    timezone       TEXT NOT NULL DEFAULT '',
    postnum        INT NOT NULL DEFAULT 0,
    threadnum      INT NOT NULL DEFAULT 0,
    reputation     INT NOT NULL DEFAULT 0,
    warningpoints  INT NOT NULL DEFAULT 0,
    moderateposts  BOOLEAN NOT NULL DEFAULT FALSE,
    moderationtime BIGINT NOT NULL DEFAULT 0,
    suspendposting BOOLEAN NOT NULL DEFAULT FALSE,
    suspensiontime BIGINT NOT NULL DEFAULT 0,
    suspendsignature BOOLEAN NOT NULL DEFAULT FALSE,
    suspendsigtime BIGINT NOT NULL DEFAULT 0,
    regip          TEXT NOT NULL DEFAULT '',
    lastip         TEXT NOT NULL DEFAULT '',
    language       TEXT NOT NULL DEFAULT '',
    style          INT NOT NULL DEFAULT 0,
    away           BOOLEAN NOT NULL DEFAULT FALSE,
    awaydate       BIGINT NOT NULL DEFAULT 0,
    returndate     TEXT NOT NULL DEFAULT '',
    awayreason     TEXT NOT NULL DEFAULT '',
    pmnotice       BOOLEAN NOT NULL DEFAULT TRUE,
    pmnotify       BOOLEAN NOT NULL DEFAULT TRUE,
    receivepms     BOOLEAN NOT NULL DEFAULT TRUE,
    receivefrombuddy BOOLEAN NOT NULL DEFAULT FALSE,
    buddylist      INT[] NOT NULL DEFAULT '{}',
    ignorelist     INT[] NOT NULL DEFAULT '{}',
    hideemail      BOOLEAN NOT NULL DEFAULT TRUE,
    allownotices   BOOLEAN NOT NULL DEFAULT TRUE,
    subscriptionmethod SMALLINT NOT NULL DEFAULT 0,      -- 0 none, 1 subscribe no notify, 2 email, 3 pm
    invisible      BOOLEAN NOT NULL DEFAULT FALSE,
    showsigs       BOOLEAN NOT NULL DEFAULT TRUE,
    showavatars    BOOLEAN NOT NULL DEFAULT TRUE,
    showimages     BOOLEAN NOT NULL DEFAULT TRUE,
    showvideos     BOOLEAN NOT NULL DEFAULT TRUE,
    showquickreply BOOLEAN NOT NULL DEFAULT TRUE,
    showredirect   BOOLEAN NOT NULL DEFAULT FALSE,
    tpp            SMALLINT NOT NULL DEFAULT 0,
    ppp            SMALLINT NOT NULL DEFAULT 0,
    threadmode     TEXT NOT NULL DEFAULT '',
    daysprune      SMALLINT NOT NULL DEFAULT 0,
    dateformat     TEXT NOT NULL DEFAULT '',
    timeformat     TEXT NOT NULL DEFAULT '',
    colormode      TEXT NOT NULL DEFAULT 'auto',
    referrer       INT NOT NULL DEFAULT 0,
    referrals      INT NOT NULL DEFAULT 0,
    usernotes      TEXT NOT NULL DEFAULT '',
    notepad        TEXT NOT NULL DEFAULT '',
    pmfolders      JSONB NOT NULL DEFAULT '[]',
    unreadpms      INT NOT NULL DEFAULT 0,
    totalpms       INT NOT NULL DEFAULT 0,
    unreadalerts   INT NOT NULL DEFAULT 0,
    timeonline     BIGINT NOT NULL DEFAULT 0,
    loginattempts  INT NOT NULL DEFAULT 0,
    loginlockoutexpiry BIGINT NOT NULL DEFAULT 0,
    totp_secret    TEXT NOT NULL DEFAULT '',
    session_version INT NOT NULL DEFAULT 0,
    coppauser      BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE UNIQUE INDEX users_username_lower ON users (lower(username));
CREATE INDEX users_email_lower ON users (lower(email));
CREATE INDEX users_usergroup ON users (usergroup);
CREATE INDEX users_lastactive ON users (lastactive);
CREATE INDEX users_postnum ON users (postnum DESC);
CREATE INDEX users_regdate ON users (regdate);
CREATE INDEX users_birthday ON users (birthday) WHERE birthday <> '';
CREATE INDEX users_username_trgm ON users USING gin (username gin_trgm_ops);
CREATE INDEX users_additionalgroups ON users USING gin (additionalgroups);

CREATE TABLE profilefields (
    fid          SERIAL PRIMARY KEY,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    disporder    INT NOT NULL DEFAULT 0,
    type         TEXT NOT NULL DEFAULT 'text',       -- text, textarea, select, multiselect, radio, checkbox
    options      TEXT NOT NULL DEFAULT '',            -- newline separated
    regex        TEXT NOT NULL DEFAULT '',
    length       INT NOT NULL DEFAULT 0,
    maxlength    INT NOT NULL DEFAULT 0,
    required     BOOLEAN NOT NULL DEFAULT FALSE,
    registration BOOLEAN NOT NULL DEFAULT FALSE,
    profile      BOOLEAN NOT NULL DEFAULT TRUE,
    postbit      BOOLEAN NOT NULL DEFAULT FALSE,
    viewableby   INT[] NOT NULL DEFAULT '{}',          -- empty = everyone
    editableby   INT[] NOT NULL DEFAULT '{}',
    postnum      INT NOT NULL DEFAULT 0,
    allowhtml    BOOLEAN NOT NULL DEFAULT FALSE,
    allowmycode  BOOLEAN NOT NULL DEFAULT TRUE,
    allowsmilies BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE userfields (
    uid    INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    fid    INT NOT NULL REFERENCES profilefields(fid) ON DELETE CASCADE,
    value  TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (uid, fid)
);

CREATE TABLE awaitingactivation (
    aid       SERIAL PRIMARY KEY,
    uid       INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    dateline  BIGINT NOT NULL,
    code      TEXT NOT NULL,
    type      TEXT NOT NULL,          -- r = registration, p = password reset, e = email change, b = admin activation
    misc      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX awaitingactivation_uid ON awaitingactivation (uid, type);

CREATE UNLOGGED TABLE sessions (
    sid        TEXT PRIMARY KEY,
    uid        INT NOT NULL DEFAULT 0,
    ip         TEXT NOT NULL DEFAULT '',
    time       BIGINT NOT NULL,
    location   TEXT NOT NULL DEFAULT '',
    useragent  TEXT NOT NULL DEFAULT '',
    anonymous  BOOLEAN NOT NULL DEFAULT FALSE,
    location1  INT NOT NULL DEFAULT 0,
    location2  INT NOT NULL DEFAULT 0,
    bot        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX sessions_time ON sessions (time);
CREATE INDEX sessions_uid ON sessions (uid);
CREATE INDEX sessions_location1 ON sessions (location1);
CREATE INDEX sessions_location2 ON sessions (location2);

-- Persistent login tokens (the "remember me" cookie). Sessions above are just activity tracking.
CREATE TABLE logins (
    token_hash  TEXT PRIMARY KEY,
    uid         INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    created     BIGINT NOT NULL,
    lastused    BIGINT NOT NULL,
    expires     BIGINT NOT NULL,
    ip          TEXT NOT NULL DEFAULT '',
    useragent   TEXT NOT NULL DEFAULT '',
    csrf        TEXT NOT NULL,
    acp_verified BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX logins_uid ON logins (uid);

CREATE TABLE forums (
    fid          SERIAL PRIMARY KEY,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    linkto       TEXT NOT NULL DEFAULT '',
    type         CHAR(1) NOT NULL DEFAULT 'f',   -- f = forum, c = category
    pid          INT NOT NULL DEFAULT 0,
    parentlist   INT[] NOT NULL DEFAULT '{}',
    disporder    INT NOT NULL DEFAULT 0,
    active       BOOLEAN NOT NULL DEFAULT TRUE,
    open         BOOLEAN NOT NULL DEFAULT TRUE,
    threads      INT NOT NULL DEFAULT 0,
    posts        INT NOT NULL DEFAULT 0,
    unapprovedthreads INT NOT NULL DEFAULT 0,
    unapprovedposts   INT NOT NULL DEFAULT 0,
    deletedthreads    INT NOT NULL DEFAULT 0,
    deletedposts      INT NOT NULL DEFAULT 0,
    lastpost        BIGINT NOT NULL DEFAULT 0,
    lastposter      TEXT NOT NULL DEFAULT '',
    lastposteruid   INT NOT NULL DEFAULT 0,
    lastposttid     INT NOT NULL DEFAULT 0,
    lastpostsubject TEXT NOT NULL DEFAULT '',
    allowhtml     BOOLEAN NOT NULL DEFAULT FALSE,
    allowmycode   BOOLEAN NOT NULL DEFAULT TRUE,
    allowsmilies  BOOLEAN NOT NULL DEFAULT TRUE,
    allowimgcode  BOOLEAN NOT NULL DEFAULT TRUE,
    allowvideocode BOOLEAN NOT NULL DEFAULT TRUE,
    allowpicons   BOOLEAN NOT NULL DEFAULT TRUE,
    allowtratings BOOLEAN NOT NULL DEFAULT TRUE,
    usepostcounts BOOLEAN NOT NULL DEFAULT TRUE,
    usethreadcounts BOOLEAN NOT NULL DEFAULT TRUE,
    requireprefix BOOLEAN NOT NULL DEFAULT FALSE,
    password      TEXT NOT NULL DEFAULT '',
    showinjump    BOOLEAN NOT NULL DEFAULT TRUE,
    style         INT NOT NULL DEFAULT 0,
    overridestyle BOOLEAN NOT NULL DEFAULT FALSE,
    rulestype     SMALLINT NOT NULL DEFAULT 0,   -- 0 none, 1 display, 2 link
    rulestitle    TEXT NOT NULL DEFAULT '',
    rules         TEXT NOT NULL DEFAULT '',
    defaultdatecut INT NOT NULL DEFAULT 0,
    defaultsortby  TEXT NOT NULL DEFAULT '',
    defaultsortorder TEXT NOT NULL DEFAULT ''
);
CREATE INDEX forums_pid ON forums (pid, disporder);

CREATE TABLE forumpermissions (
    pid    SERIAL PRIMARY KEY,
    fid    INT NOT NULL REFERENCES forums(fid) ON DELETE CASCADE,
    gid    INT NOT NULL REFERENCES usergroups(gid) ON DELETE CASCADE,
    perms  JSONB NOT NULL DEFAULT '{}',
    UNIQUE (fid, gid)
);

CREATE TABLE moderators (
    mid      SERIAL PRIMARY KEY,
    fid      INT NOT NULL REFERENCES forums(fid) ON DELETE CASCADE,
    id       INT NOT NULL,
    isgroup  BOOLEAN NOT NULL DEFAULT FALSE,
    perms    JSONB NOT NULL DEFAULT '{}',
    UNIQUE (fid, id, isgroup)
);

CREATE TABLE threadprefixes (
    pid          SERIAL PRIMARY KEY,
    prefix       TEXT NOT NULL,
    displaystyle TEXT NOT NULL DEFAULT '',
    forums       INT[] NOT NULL DEFAULT '{}',    -- empty = all
    groups       INT[] NOT NULL DEFAULT '{}'     -- empty = all
);

CREATE TABLE icons (
    iid   SERIAL PRIMARY KEY,
    name  TEXT NOT NULL,
    path  TEXT NOT NULL
);

CREATE TABLE threads (
    tid          SERIAL PRIMARY KEY,
    fid          INT NOT NULL REFERENCES forums(fid),
    subject      TEXT NOT NULL,
    prefix       INT NOT NULL DEFAULT 0,
    icon         INT NOT NULL DEFAULT 0,
    poll         INT NOT NULL DEFAULT 0,
    uid          INT NOT NULL DEFAULT 0,
    username     TEXT NOT NULL DEFAULT '',
    dateline     BIGINT NOT NULL,
    firstpost    INT NOT NULL DEFAULT 0,
    lastpost     BIGINT NOT NULL DEFAULT 0,
    lastposter   TEXT NOT NULL DEFAULT '',
    lastposteruid INT NOT NULL DEFAULT 0,
    views        INT NOT NULL DEFAULT 0,
    replies      INT NOT NULL DEFAULT 0,
    closed       TEXT NOT NULL DEFAULT '',      -- '' open, '1' closed, 'moved|<tid>' redirect
    sticky       BOOLEAN NOT NULL DEFAULT FALSE,
    numratings   INT NOT NULL DEFAULT 0,
    totalratings INT NOT NULL DEFAULT 0,
    notes        TEXT NOT NULL DEFAULT '',
    visible      SMALLINT NOT NULL DEFAULT 1,  -- 1 visible, 0 unapproved, -1 soft deleted
    unapprovedposts INT NOT NULL DEFAULT 0,
    deletedposts INT NOT NULL DEFAULT 0,
    attachmentcount INT NOT NULL DEFAULT 0,
    deletetime   BIGINT NOT NULL DEFAULT 0,
    redirect_expires BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX threads_forum_list ON threads (fid, sticky DESC, lastpost DESC, tid DESC);
CREATE INDEX threads_fid_dateline ON threads (fid, sticky DESC, dateline DESC, tid DESC);
CREATE INDEX threads_fid_lastpost ON threads (fid, visible, lastpost DESC);
CREATE INDEX threads_redirects ON threads (fid) WHERE closed LIKE 'moved|%';
CREATE INDEX threads_poll ON threads (poll) WHERE poll > 0;
CREATE INDEX threads_unapproved ON threads (fid) WHERE visible <> 1;
CREATE INDEX threads_uid ON threads (uid, dateline DESC);
CREATE INDEX threads_lastpost ON threads (lastpost DESC) WHERE visible = 1;
CREATE INDEX threads_subject_trgm ON threads USING gin (subject gin_trgm_ops);

CREATE TABLE posts (
    pid          SERIAL PRIMARY KEY,
    tid          INT NOT NULL REFERENCES threads(tid) ON DELETE CASCADE,
    replyto      INT NOT NULL DEFAULT 0,
    fid          INT NOT NULL,
    subject      TEXT NOT NULL DEFAULT '',
    icon         INT NOT NULL DEFAULT 0,
    uid          INT NOT NULL DEFAULT 0,
    username     TEXT NOT NULL DEFAULT '',
    dateline     BIGINT NOT NULL,
    message      TEXT NOT NULL,
    message_html TEXT NOT NULL DEFAULT '',
    parser_rev   INT NOT NULL DEFAULT 0,
    ipaddress    TEXT NOT NULL DEFAULT '',
    includesig   BOOLEAN NOT NULL DEFAULT TRUE,
    smilieoff    BOOLEAN NOT NULL DEFAULT FALSE,
    edituid      INT NOT NULL DEFAULT 0,
    edittime     BIGINT NOT NULL DEFAULT 0,
    editreason   TEXT NOT NULL DEFAULT '',
    visible      SMALLINT NOT NULL DEFAULT 1,
    search_tsv   TSVECTOR GENERATED ALWAYS AS (
                     setweight(to_tsvector('english'::regconfig, coalesce(subject, '')), 'A') ||
                     setweight(to_tsvector('english'::regconfig, left(message, 100000)), 'B')
                 ) STORED
);
CREATE INDEX posts_tid_dateline ON posts (tid, dateline, pid);
CREATE INDEX posts_uid_dateline ON posts (uid, dateline DESC);
CREATE INDEX posts_fid_dateline ON posts (fid, dateline DESC);
CREATE INDEX posts_dateline ON posts (dateline DESC);
CREATE INDEX posts_visible_mod ON posts (visible, fid) WHERE visible <> 1;
CREATE INDEX posts_search ON posts USING gin (search_tsv);
CREATE INDEX posts_ip ON posts (ipaddress);

CREATE TABLE post_edits (
    peid      SERIAL PRIMARY KEY,
    pid       INT NOT NULL REFERENCES posts(pid) ON DELETE CASCADE,
    uid       INT NOT NULL,
    dateline  BIGINT NOT NULL,
    subject   TEXT NOT NULL DEFAULT '',
    message   TEXT NOT NULL,
    reason    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX post_edits_pid ON post_edits (pid, dateline DESC);

CREATE TABLE drafts (
    did       SERIAL PRIMARY KEY,
    uid       INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    fid       INT NOT NULL DEFAULT 0,
    tid       INT NOT NULL DEFAULT 0,
    subject   TEXT NOT NULL DEFAULT '',
    message   TEXT NOT NULL DEFAULT '',
    dateline  BIGINT NOT NULL
);
CREATE INDEX drafts_uid ON drafts (uid, dateline DESC);

CREATE TABLE threadsread (
    tid       INT NOT NULL,
    uid       INT NOT NULL,
    dateline  BIGINT NOT NULL,
    PRIMARY KEY (uid, tid)
);
CREATE TABLE forumsread (
    fid       INT NOT NULL,
    uid       INT NOT NULL,
    dateline  BIGINT NOT NULL,
    PRIMARY KEY (uid, fid)
);

CREATE TABLE threadsubscriptions (
    sid          SERIAL PRIMARY KEY,
    uid          INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    tid          INT NOT NULL REFERENCES threads(tid) ON DELETE CASCADE,
    notification SMALLINT NOT NULL DEFAULT 0,  -- 0 none, 1 email, 2 pm
    dateline     BIGINT NOT NULL,
    UNIQUE (uid, tid)
);
CREATE INDEX threadsubscriptions_tid ON threadsubscriptions (tid);

CREATE TABLE forumsubscriptions (
    fsid   SERIAL PRIMARY KEY,
    fid    INT NOT NULL REFERENCES forums(fid) ON DELETE CASCADE,
    uid    INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    UNIQUE (uid, fid)
);
CREATE INDEX forumsubscriptions_fid ON forumsubscriptions (fid);

CREATE TABLE threadratings (
    rid        SERIAL PRIMARY KEY,
    tid        INT NOT NULL REFERENCES threads(tid) ON DELETE CASCADE,
    uid        INT NOT NULL,
    rating     SMALLINT NOT NULL,
    ipaddress  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX threadratings_tid ON threadratings (tid, uid);

CREATE TABLE polls (
    pid        SERIAL PRIMARY KEY,
    tid        INT NOT NULL REFERENCES threads(tid) ON DELETE CASCADE,
    question   TEXT NOT NULL,
    dateline   BIGINT NOT NULL,
    options    TEXT[] NOT NULL,
    votes      INT[] NOT NULL,
    numvotes   INT NOT NULL DEFAULT 0,
    timeout    BIGINT NOT NULL DEFAULT 0,
    closed     BOOLEAN NOT NULL DEFAULT FALSE,
    multiple   BOOLEAN NOT NULL DEFAULT FALSE,
    public     BOOLEAN NOT NULL DEFAULT FALSE,
    maxoptions INT NOT NULL DEFAULT 0
);
CREATE TABLE pollvotes (
    vid        SERIAL PRIMARY KEY,
    pid        INT NOT NULL REFERENCES polls(pid) ON DELETE CASCADE,
    uid        INT NOT NULL,
    voteoption INT NOT NULL,
    dateline   BIGINT NOT NULL,
    ipaddress  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX pollvotes_pid ON pollvotes (pid, uid);

CREATE TABLE attachtypes (
    atid       SERIAL PRIMARY KEY,
    name       TEXT NOT NULL,
    mimetype   TEXT NOT NULL,
    extension  TEXT NOT NULL,
    maxsize    INT NOT NULL DEFAULT 0,         -- KB
    icon       TEXT NOT NULL DEFAULT '',
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    groups     INT[] NOT NULL DEFAULT '{}',
    forums     INT[] NOT NULL DEFAULT '{}',
    avatarfile BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE attachments (
    aid          SERIAL PRIMARY KEY,
    pid          INT NOT NULL DEFAULT 0,
    posthash     TEXT NOT NULL DEFAULT '',
    uid          INT NOT NULL,
    filename     TEXT NOT NULL,
    filetype     TEXT NOT NULL,
    filesize     BIGINT NOT NULL,
    attachname   TEXT NOT NULL,
    downloads    INT NOT NULL DEFAULT 0,
    dateuploaded BIGINT NOT NULL,
    visible      BOOLEAN NOT NULL DEFAULT TRUE,
    thumbnail    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX attachments_pid ON attachments (pid);
CREATE INDEX attachments_posthash ON attachments (posthash) WHERE posthash <> '';
CREATE INDEX attachments_uid ON attachments (uid);

CREATE TABLE privatemessages (
    pmid       SERIAL PRIMARY KEY,
    uid        INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,  -- owner of this copy
    toid       INT NOT NULL DEFAULT 0,
    fromid     INT NOT NULL DEFAULT 0,
    recipients JSONB NOT NULL DEFAULT '{}',
    folder     INT NOT NULL DEFAULT 1,        -- 1 inbox, 2 sent, 3 drafts, 4 trash, 5+ custom
    subject    TEXT NOT NULL,
    icon       INT NOT NULL DEFAULT 0,
    message    TEXT NOT NULL,
    dateline   BIGINT NOT NULL,
    deletetime BIGINT NOT NULL DEFAULT 0,
    status     SMALLINT NOT NULL DEFAULT 0,   -- 0 unread, 1 read, 3 replied, 4 forwarded
    statustime BIGINT NOT NULL DEFAULT 0,
    includesig BOOLEAN NOT NULL DEFAULT TRUE,
    smilieoff  BOOLEAN NOT NULL DEFAULT FALSE,
    receipt    SMALLINT NOT NULL DEFAULT 0,   -- 0 none, 1 requested, 2 read
    readtime   BIGINT NOT NULL DEFAULT 0,
    ipaddress  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX pm_uid_folder ON privatemessages (uid, folder, dateline DESC);
CREATE INDEX pm_fromid ON privatemessages (fromid, dateline DESC);
CREATE INDEX pm_tracking ON privatemessages (fromid, receipt) WHERE receipt > 0;

CREATE TABLE reputation (
    rid        SERIAL PRIMARY KEY,
    uid        INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    adduid     INT NOT NULL,
    pid        INT NOT NULL DEFAULT 0,
    reputation INT NOT NULL,
    dateline   BIGINT NOT NULL,
    comments   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX reputation_uid ON reputation (uid, dateline DESC);
CREATE INDEX reputation_adduid ON reputation (adduid, dateline DESC);

CREATE TABLE warningtypes (
    tid            SERIAL PRIMARY KEY,
    title          TEXT NOT NULL,
    points         INT NOT NULL,
    expirationtime BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE warninglevels (
    lid        SERIAL PRIMARY KEY,
    percentage INT NOT NULL,
    action     JSONB NOT NULL          -- {"type":"ban"|"moderate"|"suspend","length":secs,"usergroup":gid}
);
CREATE TABLE warnings (
    wid          SERIAL PRIMARY KEY,
    uid          INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    tid          INT NOT NULL DEFAULT 0,
    pid          INT NOT NULL DEFAULT 0,
    title        TEXT NOT NULL,
    points       INT NOT NULL,
    dateline     BIGINT NOT NULL,
    issuedby     INT NOT NULL,
    expires      BIGINT NOT NULL DEFAULT 0,
    expired      BOOLEAN NOT NULL DEFAULT FALSE,
    daterevoked  BIGINT NOT NULL DEFAULT 0,
    revokedby    INT NOT NULL DEFAULT 0,
    revokereason TEXT NOT NULL DEFAULT '',
    notes        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX warnings_uid ON warnings (uid, dateline DESC);
CREATE INDEX warnings_expiry ON warnings (expires) WHERE expired = FALSE AND expires > 0;

CREATE TABLE banned (
    uid        INT PRIMARY KEY REFERENCES users(uid) ON DELETE CASCADE,
    gid        INT NOT NULL,
    oldgroup   INT NOT NULL,
    oldadditionalgroups INT[] NOT NULL DEFAULT '{}',
    olddisplaygroup INT NOT NULL DEFAULT 0,
    admin      INT NOT NULL,
    dateline   BIGINT NOT NULL,
    bantime    TEXT NOT NULL DEFAULT '---',
    lifted     BIGINT NOT NULL DEFAULT 0,
    reason     TEXT NOT NULL DEFAULT ''
);

CREATE TABLE banfilters (
    fid       SERIAL PRIMARY KEY,
    filter    TEXT NOT NULL,
    type      SMALLINT NOT NULL,      -- 1 ip, 2 username, 3 email
    lastuse   BIGINT NOT NULL DEFAULT 0,
    dateline  BIGINT NOT NULL
);

CREATE TABLE badwords (
    bid         SERIAL PRIMARY KEY,
    badword     TEXT NOT NULL,
    regex       BOOLEAN NOT NULL DEFAULT FALSE,
    replacement TEXT NOT NULL DEFAULT '****'
);

CREATE TABLE smilies (
    sid           SERIAL PRIMARY KEY,
    name          TEXT NOT NULL,
    find          TEXT NOT NULL,     -- newline separated
    image         TEXT NOT NULL,
    disporder     INT NOT NULL DEFAULT 0,
    showclickable BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE mycode (
    cid         SERIAL PRIMARY KEY,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    regex       TEXT NOT NULL,
    replacement TEXT NOT NULL,
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    parseorder  INT NOT NULL DEFAULT 0
);

CREATE TABLE announcements (
    aid          SERIAL PRIMARY KEY,
    fid          INT NOT NULL DEFAULT -1,     -- -1 = global
    uid          INT NOT NULL,
    subject      TEXT NOT NULL,
    message      TEXT NOT NULL,
    startdate    BIGINT NOT NULL,
    enddate      BIGINT NOT NULL DEFAULT 0,
    allowhtml    BOOLEAN NOT NULL DEFAULT FALSE,
    allowmycode  BOOLEAN NOT NULL DEFAULT TRUE,
    allowsmilies BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE reportreasons (
    rid       SERIAL PRIMARY KEY,
    title     TEXT NOT NULL,
    appliesto TEXT NOT NULL DEFAULT 'all',
    extra     BOOLEAN NOT NULL DEFAULT FALSE,
    disporder INT NOT NULL DEFAULT 0
);

CREATE TABLE reportedcontent (
    rid          SERIAL PRIMARY KEY,
    id           INT NOT NULL,       -- pid / uid / rid
    id2          INT NOT NULL DEFAULT 0,
    id3          INT NOT NULL DEFAULT 0,
    uid          INT NOT NULL,
    reportstatus SMALLINT NOT NULL DEFAULT 0,
    reasonid     INT NOT NULL DEFAULT 0,
    reason       TEXT NOT NULL DEFAULT '',
    type         TEXT NOT NULL,      -- post, profile, reputation
    reports      INT NOT NULL DEFAULT 1,
    reporters    INT[] NOT NULL DEFAULT '{}',
    dateline     BIGINT NOT NULL,
    lastreport   BIGINT NOT NULL
);
CREATE INDEX reportedcontent_status ON reportedcontent (reportstatus, lastreport DESC);
CREATE INDEX reportedcontent_lookup ON reportedcontent (type, id, reportstatus);

CREATE TABLE moderatorlog (
    id        BIGSERIAL PRIMARY KEY,
    uid       INT NOT NULL,
    dateline  BIGINT NOT NULL,
    fid       INT NOT NULL DEFAULT 0,
    tid       INT NOT NULL DEFAULT 0,
    pid       INT NOT NULL DEFAULT 0,
    action    TEXT NOT NULL,
    data      JSONB NOT NULL DEFAULT '{}',
    ipaddress TEXT NOT NULL DEFAULT ''
);
CREATE INDEX moderatorlog_dateline ON moderatorlog (dateline DESC);

CREATE TABLE adminlog (
    id        BIGSERIAL PRIMARY KEY,
    uid       INT NOT NULL,
    ipaddress TEXT NOT NULL DEFAULT '',
    dateline  BIGINT NOT NULL,
    module    TEXT NOT NULL,
    action    TEXT NOT NULL,
    data      JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX adminlog_dateline ON adminlog (dateline DESC);

CREATE TABLE adminoptions (
    uid         INT PRIMARY KEY REFERENCES users(uid) ON DELETE CASCADE,
    notes       TEXT NOT NULL DEFAULT '',
    permissions JSONB NOT NULL DEFAULT '{}'    -- empty = inherit / full for admins
);

CREATE TABLE modtools (
    tid           SERIAL PRIMARY KEY,
    name          TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    forums        INT[] NOT NULL DEFAULT '{}',
    groups        INT[] NOT NULL DEFAULT '{}',
    type          CHAR(1) NOT NULL DEFAULT 't',     -- t thread, p post
    postoptions   JSONB NOT NULL DEFAULT '{}',
    threadoptions JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE delayedmoderation (
    did            SERIAL PRIMARY KEY,
    type           TEXT NOT NULL,
    delaydateline  BIGINT NOT NULL,
    uid            INT NOT NULL,
    fid            INT NOT NULL DEFAULT 0,
    tids           INT[] NOT NULL,
    dateline       BIGINT NOT NULL,
    inputs         JSONB NOT NULL DEFAULT '{}'
);

CREATE TABLE calendars (
    cid          SERIAL PRIMARY KEY,
    name         TEXT NOT NULL,
    disporder    INT NOT NULL DEFAULT 0,
    startofweek  SMALLINT NOT NULL DEFAULT 0,
    showbirthdays BOOLEAN NOT NULL DEFAULT TRUE,
    eventlimit   INT NOT NULL DEFAULT 4,
    moderation   BOOLEAN NOT NULL DEFAULT FALSE,
    allowhtml    BOOLEAN NOT NULL DEFAULT FALSE,
    allowmycode  BOOLEAN NOT NULL DEFAULT TRUE,
    allowimgcode BOOLEAN NOT NULL DEFAULT TRUE,
    allowvideocode BOOLEAN NOT NULL DEFAULT TRUE,
    allowsmilies BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE TABLE calendarpermissions (
    cid   INT NOT NULL REFERENCES calendars(cid) ON DELETE CASCADE,
    gid   INT NOT NULL REFERENCES usergroups(gid) ON DELETE CASCADE,
    perms JSONB NOT NULL DEFAULT '{}',
    PRIMARY KEY (cid, gid)
);
CREATE TABLE events (
    eid          SERIAL PRIMARY KEY,
    cid          INT NOT NULL REFERENCES calendars(cid) ON DELETE CASCADE,
    uid          INT NOT NULL,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL,
    visible      BOOLEAN NOT NULL DEFAULT TRUE,
    private      BOOLEAN NOT NULL DEFAULT FALSE,
    dateline     BIGINT NOT NULL,
    starttime    BIGINT NOT NULL,
    endtime      BIGINT NOT NULL DEFAULT 0,
    timezone     TEXT NOT NULL DEFAULT '',
    ignoretimezone BOOLEAN NOT NULL DEFAULT TRUE,
    usingtime    BOOLEAN NOT NULL DEFAULT FALSE,
    repeats      JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX events_range ON events (cid, starttime, endtime);

CREATE TABLE usertitles (
    utid      SERIAL PRIMARY KEY,
    posts     INT NOT NULL,
    title     TEXT NOT NULL,
    stars     SMALLINT NOT NULL DEFAULT 0,
    starimage TEXT NOT NULL DEFAULT ''
);

CREATE TABLE promotions (
    pid             SERIAL PRIMARY KEY,
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    logging         BOOLEAN NOT NULL DEFAULT TRUE,
    requirements    JSONB NOT NULL DEFAULT '{}',   -- {"posts":[">=",100],"registered":[">=",days],...}
    originalusergroup INT[] NOT NULL DEFAULT '{}',
    newusergroup    INT NOT NULL,
    usergrouptype   TEXT NOT NULL DEFAULT 'primary',
    lastrun         BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE promotionlogs (
    plid       SERIAL PRIMARY KEY,
    pid        INT NOT NULL,
    uid        INT NOT NULL,
    oldusergroup TEXT NOT NULL,
    newusergroup INT NOT NULL,
    dateline   BIGINT NOT NULL,
    type       TEXT NOT NULL
);

CREATE TABLE joinrequests (
    rid       SERIAL PRIMARY KEY,
    uid       INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    gid       INT NOT NULL REFERENCES usergroups(gid) ON DELETE CASCADE,
    reason    TEXT NOT NULL DEFAULT '',
    dateline  BIGINT NOT NULL,
    invite    BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (uid, gid)
);
CREATE TABLE groupleaders (
    lid       SERIAL PRIMARY KEY,
    gid       INT NOT NULL REFERENCES usergroups(gid) ON DELETE CASCADE,
    uid       INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    canmanagemembers  BOOLEAN NOT NULL DEFAULT TRUE,
    canmanagerequests BOOLEAN NOT NULL DEFAULT TRUE,
    caninvitemembers  BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE (gid, uid)
);

CREATE TABLE buddyrequests (
    id      SERIAL PRIMARY KEY,
    uid     INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    touid   INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    date    BIGINT NOT NULL,
    UNIQUE (uid, touid)
);

CREATE TABLE helpsections (
    sid         SERIAL PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    disporder   INT NOT NULL DEFAULT 0,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE TABLE helpdocs (
    hid         SERIAL PRIMARY KEY,
    sid         INT NOT NULL REFERENCES helpsections(sid) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    document    TEXT NOT NULL,
    disporder   INT NOT NULL DEFAULT 0,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE mailqueue (
    mid      BIGSERIAL PRIMARY KEY,
    mailto   TEXT NOT NULL,
    subject  TEXT NOT NULL,
    message  TEXT NOT NULL,
    dateline BIGINT NOT NULL,
    attempts INT NOT NULL DEFAULT 0,
    lasterror TEXT NOT NULL DEFAULT ''
);
CREATE TABLE maillogs (
    mid       BIGSERIAL PRIMARY KEY,
    subject   TEXT NOT NULL,
    message   TEXT NOT NULL,
    dateline  BIGINT NOT NULL,
    fromuid   INT NOT NULL DEFAULT 0,
    fromemail TEXT NOT NULL DEFAULT '',
    touid     INT NOT NULL DEFAULT 0,
    toemail   TEXT NOT NULL DEFAULT '',
    tid       INT NOT NULL DEFAULT 0,
    ipaddress TEXT NOT NULL DEFAULT '',
    type      SMALLINT NOT NULL DEFAULT 1    -- 1 email user, 2 send thread, 3 contact
);

CREATE TABLE massemails (
    mid        SERIAL PRIMARY KEY,
    uid        INT NOT NULL,
    subject    TEXT NOT NULL,
    message    TEXT NOT NULL,
    type       SMALLINT NOT NULL DEFAULT 0,   -- 0 email, 1 pm
    format     TEXT NOT NULL DEFAULT 'text',
    dateline   BIGINT NOT NULL,
    senddate   BIGINT NOT NULL,
    status     SMALLINT NOT NULL DEFAULT 0,    -- 0 draft, 1 queued, 2 sending, 3 done
    sentcount  INT NOT NULL DEFAULT 0,
    totalcount INT NOT NULL DEFAULT 0,
    conditions JSONB NOT NULL DEFAULT '{}',
    lastuid    INT NOT NULL DEFAULT 0
);

CREATE TABLE tasks (
    tid        SERIAL PRIMARY KEY,
    key        TEXT NOT NULL UNIQUE,
    title      TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    interval_secs INT NOT NULL,
    nextrun    BIGINT NOT NULL DEFAULT 0,
    lastrun    BIGINT NOT NULL DEFAULT 0,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    logging    BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE TABLE tasklog (
    lid      BIGSERIAL PRIMARY KEY,
    tid      INT NOT NULL,
    dateline BIGINT NOT NULL,
    data     TEXT NOT NULL
);

CREATE TABLE themes (
    tid          SERIAL PRIMARY KEY,
    name         TEXT NOT NULL,
    pid          INT NOT NULL DEFAULT 0,
    def          BOOLEAN NOT NULL DEFAULT FALSE,
    properties   JSONB NOT NULL DEFAULT '{}',
    stylesheet   TEXT NOT NULL DEFAULT '',
    allowedgroups INT[] NOT NULL DEFAULT '{}'
);

CREATE TABLE templates (
    tid      SERIAL PRIMARY KEY,
    title    TEXT NOT NULL,
    theme    INT NOT NULL REFERENCES themes(tid) ON DELETE CASCADE,
    template TEXT NOT NULL,
    dateline BIGINT NOT NULL,
    UNIQUE (theme, title)
);

CREATE TABLE captcha (
    imagehash   TEXT PRIMARY KEY,
    imagestring TEXT NOT NULL,
    dateline    BIGINT NOT NULL
);

CREATE TABLE questions (
    qid       SERIAL PRIMARY KEY,
    question  TEXT NOT NULL,
    answer    TEXT NOT NULL,           -- newline separated acceptable answers
    shown     INT NOT NULL DEFAULT 0,
    correct   INT NOT NULL DEFAULT 0,
    incorrect INT NOT NULL DEFAULT 0,
    active    BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE spamlog (
    sid       BIGSERIAL PRIMARY KEY,
    username  TEXT NOT NULL DEFAULT '',
    email     TEXT NOT NULL DEFAULT '',
    ipaddress TEXT NOT NULL DEFAULT '',
    dateline  BIGINT NOT NULL,
    data      TEXT NOT NULL DEFAULT ''
);

CREATE TABLE searchlog (
    sid        TEXT PRIMARY KEY,
    uid        INT NOT NULL DEFAULT 0,
    dateline   BIGINT NOT NULL,
    ipaddress  TEXT NOT NULL DEFAULT '',
    resulttype TEXT NOT NULL,         -- threads / posts
    ids        INT[] NOT NULL,
    keywords   TEXT NOT NULL DEFAULT '',
    params     JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX searchlog_dateline ON searchlog (dateline);
CREATE INDEX searchlog_flood ON searchlog (ipaddress, dateline DESC);

CREATE TABLE stats (
    dateline   BIGINT PRIMARY KEY,
    numusers   INT NOT NULL,
    numthreads INT NOT NULL,
    numposts   INT NOT NULL
);

-- Board-wide denormalized counters (single row).
CREATE TABLE counters (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    numusers        INT NOT NULL DEFAULT 0,
    numthreads      INT NOT NULL DEFAULT 0,
    numposts        INT NOT NULL DEFAULT 0,
    numunapprovedthreads INT NOT NULL DEFAULT 0,
    numunapprovedposts   INT NOT NULL DEFAULT 0,
    numdeletedthreads    INT NOT NULL DEFAULT 0,
    numdeletedposts      INT NOT NULL DEFAULT 0,
    lastuid         INT NOT NULL DEFAULT 0,
    lastusername    TEXT NOT NULL DEFAULT '',
    mostonline      INT NOT NULL DEFAULT 0,
    mostonlinetime  BIGINT NOT NULL DEFAULT 0
);
INSERT INTO counters (id) VALUES (1);

-- Alerts / notifications (built-in, MyAlerts style).
CREATE TABLE alerts (
    id        BIGSERIAL PRIMARY KEY,
    uid       INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    from_uid  INT NOT NULL DEFAULT 0,
    kind      TEXT NOT NULL,
    object_id INT NOT NULL DEFAULT 0,
    extra     JSONB NOT NULL DEFAULT '{}',
    dateline  BIGINT NOT NULL,
    unread    BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE INDEX alerts_uid ON alerts (uid, id DESC);

-- Post reactions ("likes") — beyond stock MyBB.
CREATE TABLE reactions (
    pid      INT NOT NULL REFERENCES posts(pid) ON DELETE CASCADE,
    uid      INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    dateline BIGINT NOT NULL,
    PRIMARY KEY (pid, uid, kind)
);
CREATE INDEX reactions_uid ON reactions (uid);

-- Rate limiting / flood control buckets that must be shared across nodes.
CREATE UNLOGGED TABLE ratelimits (
    key      TEXT PRIMARY KEY,
    hits     INT NOT NULL,
    reset_at BIGINT NOT NULL
);
