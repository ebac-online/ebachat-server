//! What a new post does to others: mentions (`@user`, keywords, `@channel`/`@all`/`@here`), mention counts,
//! thread memberships of collapsed threads, email and push notifications (`email.rs`, `push.rs`), and the
//! `posted` and `thread_updated` events.

// Based on Mattermost server/channels/app/notification.go, mention_keywords.go, mention_parser_standard.go,
// mention_results.go and post.go (isPostMention), Copyright (c) 2015-present Mattermost, Inc. Licensed under
// AGPL-3.0.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Value, json};

use crate::app::{ApiResult, App};
use crate::hub::{Broadcast, PerUser, PostedAck};
use crate::model::post::kind;
use crate::model::{Channel, Post, PostList, StringMap, User, channel, status};
use crate::posts::{all_strings, without_code};
use crate::threads::MembershipOpts;

/// `MentionType`, ordered as Go's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Mention {
    Gm = 1,
    Thread,
    Comment,
    Channel,
    Dm,
    Keyword,
}

/// `MentionResults`.
#[derive(Default, Debug)]
pub struct Mentions {
    pub users: HashMap<String, Mention>,
    pub other_potential: Vec<String>,
    pub here: bool,
    pub all: bool,
    pub channel: bool,
}

impl Mentions {
    fn add(&mut self, id: &str, t: Mention) {
        match self.users.get(id) {
            Some(cur) if *cur >= t => {}
            _ => {
                self.users.insert(id.to_string(), t);
            }
        }
    }
    /// `isUserMentioned`.
    pub fn is_mentioned(&self, id: &str) -> bool {
        self.users.contains_key(id) || self.here || self.all || self.channel
    }
}

/// `MentionKeywords`: keyword → users.
#[derive(Default)]
pub struct Keywords(HashMap<String, Vec<String>>);

impl Keywords {
    fn push(&mut self, k: String, id: &str) {
        self.0.entry(k).or_default().push(id.to_string());
    }

    /// `MentionKeywords.AddUser`.
    pub fn add_user(&mut self, u: &User, channel_props: Option<&StringMap>, status: Option<&str>, allow_channel: bool) {
        self.push(format!("@{}", u.username.to_lowercase()), &u.id);
        let mut total = 0;
        for (n, k) in u
            .notify_prop("mention_keys")
            .split(',')
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty())
            .enumerate()
        {
            if n == 10 {
                break;
            }
            if total > 0 {
                total += 1;
            }
            total += k.len();
            if total > 1024 {
                break;
            }
            self.push(k, &u.id);
        }
        if u.notify_prop("first_name") == "true" && !u.first_name.is_empty() {
            self.push(u.first_name.clone(), &u.id);
        }
        if allow_channel {
            let cp = |k: &str| channel_props.and_then(|p| p.0.get(k)).map(String::as_str).unwrap_or("");
            let ignore = cp("ignore_channel_mentions") == "on"
                || (cp("mark_unread") == "mention" && cp("ignore_channel_mentions") == "default");
            if u.notify_prop("channel") == "true" && !ignore {
                self.push("@channel".into(), &u.id);
                self.push("@all".into(), &u.id);
                if status == Some(status::ONLINE) {
                    self.push("@here".into(), &u.id);
                }
            }
        }
    }
}

static REF_DEFINITION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^ {0,3}\[[^\]]+\]:.*$").unwrap());
static LINK_DESTINATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\([^)]*\)").unwrap());
static URL_AUTOLINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(^|[^a-z0-9])(?:https?|ftp|mailto|tel)://[^\s\p{P}]\S*").unwrap());
static WWW_AUTOLINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(^|[\s*_~)<(>])www[0-9]{0,3}\.\S*").unwrap());

/// Text where mentions count: without code and links.
fn mention_text(s: &str) -> String {
    let s = without_code(s);
    let s = REF_DEFINITION.replace_all(&s, " ");
    let s = LINK_DESTINATION.replace_all(&s, "] ");
    let s = URL_AUTOLINK.replace_all(&s, "${1} ");
    WWW_AUTOLINK.replace_all(&s, "${1} ").into_owned()
}

/// `StandardMentionParser.ProcessText` over a post's texts.
pub fn explicit_mentions(post: &Post, k: &Keywords) -> Mentions {
    let mut r = Mentions::default();
    let multibyte: Vec<&String> = k.0.keys().filter(|w| w.len() != w.chars().count()).collect();
    for s in all_strings(post) {
        let text = mention_text(&s);
        for word in
            text.split(|c: char| !(c == ':' || c == '.' || c == '-' || c == '_' || c == '@' || c.is_alphanumeric()))
        {
            if word.is_empty() {
                continue;
            }
            if word.starts_with(':') && word.ends_with(':') && word.len() > 1 {
                continue;
            }
            let word = word.trim_start_matches([':', '.', '-', '_']);
            if word.is_empty() || check(&mut r, k, word) {
                continue;
            }
            let mut w = word;
            let mut found = false;
            while !w.is_empty() && w.ends_with(['.', '-', ':', '_']) {
                w = &w[..w.len() - 1];
                if check(&mut r, k, w) {
                    found = true;
                    break;
                }
            }
            if found {
                continue;
            }
            let system = ["@here", "@channel", "@all"];
            if !system.contains(&word) && word.starts_with('@') {
                let w = word.trim_end_matches(['.', '-', ':']);
                r.other_potential.push(w[1..].to_string());
            } else if word.contains(['.', '-', ':']) {
                for part in word.split(['.', '-', ':']).filter(|p| !p.is_empty()) {
                    if check(&mut r, k, part) {
                        continue;
                    }
                    if !system.contains(&part) && part.starts_with('@') {
                        r.other_potential.push(part[1..].to_string());
                    }
                }
            }
            if word.len() != word.chars().count() {
                let mut ids: Option<&Vec<String>> = None;
                for key in &multibyte {
                    if word.contains(key.as_str()) {
                        ids = k.0.get(key.as_str());
                    }
                }
                if let Some(ids) = ids {
                    for id in ids.clone() {
                        r.add(&id, Mention::Keyword);
                    }
                }
            }
        }
    }
    r
}

/// `checkForMention`.
fn check(r: &mut Mentions, k: &Keywords, word: &str) -> bool {
    let lower = word.to_lowercase();
    let t = match lower.as_str() {
        "@here" => {
            r.here = true;
            Mention::Channel
        }
        "@channel" => {
            r.channel = true;
            Mention::Channel
        }
        "@all" => {
            r.all = true;
            Mention::Channel
        }
        _ => Mention::Keyword,
    };
    let ids = k.0.get(&lower).or_else(|| k.0.get(word));
    match ids {
        Some(ids) => {
            for id in ids.clone() {
                r.add(&id, t);
            }
            true
        }
        None => false,
    }
}

static CHANNEL_MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\B~[a-zA-Z0-9\-_]+").unwrap());
static AT_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\B@(channel|all|here)\b").unwrap());

/// `ChannelMentionsAllWithOptions`: `~name` mentions.
pub fn channel_mentions(post: &Post) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for s in all_strings(post) {
        if !s.contains('~') {
            continue;
        }
        for m in CHANNEL_MENTION.find_iter(&s) {
            let n = m.as_str()[1..].to_string();
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// `Post.DisableMentionHighlights`: the first `@channel`/`@all`/`@here`, highlighting turned off.
pub fn disable_mention_highlights(post: &mut Post) -> String {
    match AT_CHANNEL.find(&post.message) {
        Some(m) => {
            post.props.0.insert("mentionHighlightDisabled".into(), Value::Bool(true));
            m.as_str().to_lowercase()
        }
        None => String::new(),
    }
}

/// `isCommentMention`.
fn is_comment_mention(
    user: &User,
    post: &Post,
    others: &BTreeMap<String, Post>,
    by_thread: &mut HashMap<String, bool>,
) -> bool {
    if post.root_id.is_empty() {
        return false;
    }
    if let Some(m) = by_thread.get(&post.root_id) {
        return *m;
    }
    let Some(root) = others.get(&post.root_id) else { return false };
    let mut mentioned = root.user_id == user.id;
    if !mentioned && user.notify_prop("comments") == "any" {
        mentioned = others.values().any(|o| {
            o.id != post.id && o.root_id == post.root_id && o.user_id == user.id && o.create_at < post.create_at
        });
    }
    by_thread.insert(post.root_id.clone(), mentioned);
    mentioned
}

/// `isPostMention`.
pub fn is_post_mention(
    user: &User,
    post: &Post,
    k: &Keywords,
    others: &BTreeMap<String, Post>,
    by_thread: &mut HashMap<String, bool>,
    check_comments: bool,
) -> bool {
    if post.user_id == user.id && !post.from_webhook() {
        return false;
    }
    if explicit_mentions(post, k).users.contains_key(&user.id) {
        return true;
    }
    if post.r#type == kind::ADD_TO_CHANNEL && post.prop_str("addedUserId") == user.id {
        return true;
    }
    check_comments && is_comment_mention(user, post, others, by_thread)
}

impl App {
    /// `GetAllProfilesInChannel` through the profiles-in-channel cache: active members.
    pub async fn profiles_in_channel(&self, channel_id: &str) -> ApiResult<Arc<HashMap<String, Arc<User>>>> {
        if let Some(m) = self.caches.profiles_in_channel.get(channel_id) {
            return Ok(m);
        }
        // One load per channel at a time (see cache::Flights): posts racing into a big channel after its entry was
        // dropped would each load every member's profile.
        let _load = self.caches.profiles_in_channel_loads.begin(channel_id).await;
        if let Some(m) = self.caches.profiles_in_channel.get(channel_id) {
            return Ok(m);
        }
        let users = crate::users::load_users(
            self,
            "u.id IN (SELECT UserId FROM ChannelMembers WHERE ChannelId = ANY($1)) AND u.DeleteAt = 0",
            &[channel_id.to_string()],
        )
        .await?;
        let m: HashMap<String, Arc<User>> = users.into_iter().map(|u| (u.id.clone(), Arc::new(u))).collect();
        let m = Arc::new(m);
        self.caches.profiles_in_channel.put(channel_id.to_string(), m.clone());
        Ok(m)
    }

    /// `getExplicitMentionsAndKeywords`.
    async fn mentions_and_keywords(
        &self,
        post: &Post,
        c: &Channel,
        profiles: &HashMap<String, Arc<User>>,
        props: &HashMap<String, StringMap>,
        parent: Option<&PostList>,
    ) -> (Mentions, Keywords) {
        let mut m = Mentions::default();
        let mut k = Keywords::default();
        if c.r#type == channel::DIRECT {
            let (a, b) = c.name.split_once("__").unwrap_or((&c.name, ""));
            for u in [a, b] {
                if !u.is_empty() && (post.user_id != u || post.from_webhook()) && profiles.contains_key(u) {
                    m.add(u, Mention::Dm);
                }
            }
            return (m, k);
        }
        // allowChannelMentions
        let allow = self.user_channel_perm(&post.user_id, &post.channel_id, crate::perm::USE_CHANNEL_MENTIONS).await.0
            && post.r#type != kind::HEADER_CHANGE
            && post.r#type != kind::PURPOSE_CHANGE
            && (profiles.len() as i64) < self.cfg.i64("TeamSettings.MaxNotificationsPerChannel");
        for u in profiles.values() {
            let st = self.caches.status.get(&u.id).map(|s| s.status);
            k.add_user(u, props.get(&u.id), st.as_deref(), allow);
        }
        m = explicit_mentions(post, &k);
        if c.r#type == channel::GROUP {
            for id in props.keys() {
                if profiles.contains_key(id) {
                    m.add(id, Mention::Gm);
                }
            }
        }
        if post.r#type == kind::ADD_TO_CHANNEL {
            let added = post.prop_str("addedUserId");
            if !added.is_empty() {
                m.add(added, Mention::Keyword);
            }
        }
        if !post.root_id.is_empty()
            && let Some(parent) = parent
        {
            let ids: Vec<String> = parent.posts.values().map(|p| p.user_id.clone()).collect();
            let crt = self.crt_for_users(&ids).await;
            let first = parent.order.first().cloned().unwrap_or_default();
            for tp in parent.posts.values() {
                let Some(profile) = profiles.get(&tp.user_id) else { continue };
                if tp.id == first && tp.prop_str("from_oauth_app") == "true" {
                    continue;
                }
                if crt.get(&profile.id).copied().unwrap_or(false) {
                    continue;
                }
                let comments = profile.notify_prop("comments");
                if comments == "any" || (comments == "root" && tp.id == first) {
                    m.add(&tp.user_id, if tp.id == first { Mention::Comment } else { Mention::Thread });
                }
            }
        }
        if !post.from_webhook() {
            m.users.remove(&post.user_id);
        }
        (m, k)
    }
}

/// `addFollowerToNotify`: whether a follower gets a desktop notification for a reply.
fn follower_desktop(u: &User, mentioned: bool, cm: Option<&StringMap>) -> bool {
    let up = |k: &str| u.notify_prop(k).to_string();
    let cp = |k: &str| cm.and_then(|p| p.0.get(k)).cloned().unwrap_or_default();
    let desktop = up("desktop");
    let user = desktop != "none" && (mentioned || up("desktop_threads") == "all" || desktop == "all");
    let cdesktop = cp("desktop");
    let channel = cdesktop != "default"
        && cdesktop != "none"
        && (mentioned || (cp("desktop_threads") == "all" && up("desktop_threads") != "mention") || cdesktop == "all");
    (cdesktop == "default" && user) || channel
}

/// `sendOutOfChannelMentions`: the poster learns which `@names` are not in the channel (and may add them), and
/// which are not in the team.
async fn out_of_channel_mentions(app: &App, sender: &User, post: &Post, c: &Channel, names: Vec<String>) {
    if post.is_system_message()
        || post.is_notification_suppressed()
        || c.team_id.is_empty()
        || c.r#type == channel::GROUP
    {
        return;
    }
    let Ok(all) = crate::users::load_users(app, "u.Username = ANY($1) ORDER BY u.Username ASC", &names).await else {
        return;
    };
    let viewer = crate::model::Session { user_id: sender.id.clone(), ..Default::default() };
    let in_team: Vec<String> =
        sqlx::query_scalar("SELECT UserId FROM TeamMembers WHERE TeamId = $1 AND DeleteAt = 0 AND UserId = ANY($2)")
            .bind(&c.team_id)
            .bind(all.iter().map(|u| u.id.clone()).collect::<Vec<_>>())
            .fetch_all(&app.db)
            .await
            .unwrap_or_default();
    let (mut team_users, mut out_of_team) = (vec![], vec![]);
    for u in all {
        if u.delete_at != 0 || u.is_bot || !app.can_see_user(&viewer, &u.id).await.unwrap_or(false) {
            continue;
        }
        if in_team.contains(&u.id) { team_users.push(u) } else { out_of_team.push(u) }
    }
    let t = |id: &str, names: &[String]| {
        if names.len() == 1 {
            crate::i18n::tl(&sender.locale, &format!("{id}.one"), &[("Username", &names[0])])
        } else {
            let (last, first) = names.split_last().unwrap();
            crate::i18n::tl(
                &sender.locale,
                &format!("{id}.multiple"),
                &[("Usernames", &first.join(", @")), ("LastUsername", last)],
            )
        }
    };
    if !team_users.is_empty() {
        let id = crate::app::new_id();
        let usernames: Vec<String> = team_users.iter().map(|u| u.username.clone()).collect();
        let ids: Vec<String> = team_users.iter().map(|u| u.id.clone()).collect();
        let mut p = Post {
            id: id.clone(),
            root_id: post.root_id.clone(),
            channel_id: post.channel_id.clone(),
            message: t("api.post.check_for_out_of_channel_mentions.message", &usernames),
            create_at: post.create_at + 1,
            ..Default::default()
        };
        p.props.0.insert(
            "add_channel_member".into(),
            json!({"post_id": id, "usernames": usernames, "not_in_channel_usernames": usernames, "user_ids": ids,
                   "not_in_channel_user_ids": ids, "not_in_groups_usernames": [], "not_in_groups_user_ids": []}),
        );
        app.send_ephemeral(&post.user_id, p).await;
    }
    if !out_of_team.is_empty() {
        let usernames: Vec<String> = out_of_team.iter().map(|u| u.username.clone()).collect();
        let p = Post {
            id: crate::app::new_id(),
            root_id: post.root_id.clone(),
            channel_id: post.channel_id.clone(),
            message: t("api.post.check_for_out_of_team_mentions.message", &usernames),
            create_at: post.create_at + 1,
            ..Default::default()
        };
        app.send_ephemeral(&post.user_id, p).await;
    }
}

/// `addFollowerToNotify` for email and push: `shouldUserNotifyCRT` and `shouldChannelMemberNotifyCRT`.
fn follower_email_push(u: &User, mentioned: bool, cm: Option<&StringMap>) -> (bool, bool) {
    let up = |k: &str| u.notify_prop(k).to_string();
    let cp = |k: &str| cm.and_then(|p| p.0.get(k)).cloned().unwrap_or_default();
    let email = up("email") == "true" && (mentioned || up("email_threads") == "all");
    let push = up("push");
    let user_push = push != "none" && (mentioned || up("push_threads") == "all" || push == "all");
    let cpush = cp("push");
    let channel_push =
        cpush != "default" && cpush != "none" && (mentioned || cp("push_threads") == "all" || cpush == "all");
    (email, (cpush == "default" && user_push) || channel_push)
}

/// `userAllowsEmail`.
async fn user_allows_email(app: &App, u: &User, cm: Option<&StringMap>, post: &Post) -> bool {
    if u.is_bot || u.remote_id.as_deref().is_some_and(|r| !r.is_empty()) {
        return false;
    }
    if post.r#type == "system_access_control_team_membership" {
        return false;
    }
    let mut allows = u.notify_prop("email") != "false";
    if let Some(ce) = cm.and_then(|p| p.0.get("email"))
        && !(!post.root_id.is_empty() && app.is_crt_enabled(&u.id).await)
        && ce != "default"
    {
        allows = ce != "false";
    }
    if cm.and_then(|p| p.0.get("mark_unread")).map(String::as_str) == Some("mention") {
        allows = false;
    }
    let st = crate::push::status_or_offline(app, &u.id).await;
    let auto_responder = st.status == status::OUT_OF_OFFICE || post.r#type == "system_auto_responder";
    let status_ok = st.status != status::ONLINE && st.status != status::DND;
    allows && status_ok && u.delete_at == 0 && !auto_responder
}

/// The email and push part of `SendNotifications`: emails to the mentioned and to followers who want them, the
/// "too many members" ephemeral posts, and pushes to the mentioned, to those who want every message and to
/// followers.
#[allow(clippy::too_many_arguments)]
async fn send_email_and_push(
    app: &App,
    post: &Post,
    c: &Channel,
    sender: &User,
    profiles: &Arc<HashMap<String, Arc<User>>>,
    props: &HashMap<String, StringMap>,
    mentions: &Mentions,
    mentioned: &[String],
    crt_email: &[String],
    crt_push: &[String],
    all_activity_push: &[String],
) {
    let n = Arc::new(crate::push::PostNotification {
        post: post.clone(),
        channel: c.clone(),
        profiles: profiles.clone(),
        sender: Arc::new(sender.clone()),
    });
    if app.cfg.bool("EmailSettings.SendEmailNotifications") {
        let mut recipients: Vec<String> = vec![];
        for id in mentioned.iter().chain(crt_email) {
            if !recipients.contains(id) {
                recipients.push(id.clone());
            }
        }
        let team = if c.team_id.is_empty() {
            crate::model::Team::default()
        } else {
            app.team(&c.team_id).await.map(|t| (*t).clone()).unwrap_or_default()
        };
        let verify = app.cfg.bool("EmailSettings.RequireEmailVerification");
        for id in recipients {
            let Some(u) = profiles.get(&id) else { continue };
            if verify && !u.email_verified {
                continue;
            }
            if !user_allows_email(app, u, props.get(&id), post).await {
                continue;
            }
            let image = Some(app.profile_image(sender).await.0);
            let (n, u, team) = (n.clone(), u.clone(), team.clone());
            let Some(state) = app.hub.app() else { continue };
            tokio::spawn(async move {
                if let Err(e) = crate::email::send_notification_email(&state, &n, &u, team, image).await {
                    eprintln!("Unable to send notification email: {e}");
                }
            });
        }
    }
    let max = app.cfg.i64("TeamSettings.MaxNotificationsPerChannel");
    if profiles.len() as i64 > max {
        let max_s = max.to_string();
        for (on, id) in [
            (mentions.here, "api.post.disabled_here"),
            (mentions.channel, "api.post.disabled_channel"),
            (mentions.all, "api.post.disabled_all"),
        ] {
            if on {
                let msg = crate::i18n::tl(&sender.locale, id, &[("Users", &max_s)]);
                let p = Post {
                    channel_id: post.channel_id.clone(),
                    message: msg,
                    create_at: post.create_at + 1,
                    ..Default::default()
                };
                app.send_ephemeral(&post.user_id, p).await;
            }
        }
    }
    if !crate::push::can_send(app) {
        return;
    }
    let is_gm = c.r#type == channel::GROUP;
    for id in mentioned {
        let Some(u) = profiles.get(id) else { continue };
        if crt_push.contains(id) {
            continue;
        }
        let st = crate::push::status_or_offline(app, id).await;
        let mt = mentions.users.get(id).copied();
        let explicit = mt.is_some_and(|m| m > Mention::Gm);
        let suppressed_add = kind::is_add_membership(&post.r#type) && crate::posts::excludes_membership(c);
        if suppressed_add || crate::push::should_send(u, props.get(id), explicit, &st, post, is_gm) {
            let reply = match mt {
                Some(Mention::Thread) => "any",
                Some(Mention::Comment) => "root",
                _ => "",
            };
            let explicit = matches!(mt, Some(Mention::Keyword | Mention::Channel | Mention::Dm));
            crate::push::send_message(app, &n, u, explicit, mt == Some(Mention::Channel), reply).await;
        }
    }
    for id in all_activity_push {
        let Some(u) = profiles.get(id) else { continue };
        if crt_push.contains(id) || mentions.users.contains_key(id) {
            continue;
        }
        let st = crate::push::status_or_offline(app, id).await;
        if crate::push::should_send(u, props.get(id), false, &st, post, is_gm) {
            crate::push::send_message(app, &n, u, false, false, "").await;
        }
    }
    for id in crt_push {
        let Some(u) = profiles.get(id) else { continue };
        let st = crate::push::status_or_offline(app, id).await;
        if crate::push::status_allows(u, &st, &post.channel_id, true) {
            crate::push::send_message(app, &n, u, false, false, "crt").await;
        }
    }
}

/// `SendNotifications`: mention counts, thread memberships, email and push, `posted`, `thread_updated`.
pub async fn send_notifications(
    app: &App,
    post: &Post,
    c: &Channel,
    sender: &User,
    parent: Option<&PostList>,
    set_online: bool,
) -> ApiResult<Vec<String>> {
    if c.delete_at > 0 {
        return Ok(vec![]);
    }
    if kind::is_membership(&post.r#type)
        && !kind::is_add_membership(&post.r#type)
        && crate::posts::excludes_membership(c)
    {
        return Ok(vec![]);
    }
    let suppressed = post.is_notification_suppressed();
    let crt_allowed = app.crt_allowed();
    let profiles = app.profiles_in_channel(&c.id).await?;
    let props = app.channel_notify_props(&c.id).await?;
    let mut followers: HashSet<String> = HashSet::new();
    if !suppressed && crt_allowed && !post.root_id.is_empty() {
        let f: Vec<(String,)> =
            sqlx::query_as("SELECT UserId FROM ThreadMemberships WHERE PostId = $1 AND Following = true")
                .bind(&post.root_id)
                .fetch_all(&app.db)
                .await?;
        followers.extend(f.into_iter().map(|r| r.0));
    }
    let team_id = c.team_id.clone();
    let mut mentioned: Vec<String> = vec![];
    let mut mentions = Mentions::default();
    let mut new_participants: HashSet<String> = HashSet::new();
    let mut memberships: HashMap<String, crate::model::ThreadMembership> = HashMap::new();
    let mut desktop_followers: Vec<String> = vec![];
    let (mut crt_email, mut crt_push, mut all_activity_push): (Vec<String>, Vec<String>, Vec<String>) =
        (vec![], vec![], vec![]);
    if !suppressed {
        let (m, k) = app.mentions_and_keywords(post, c, &profiles, &props, parent).await;
        mentions = m;
        if c.r#type != channel::DIRECT
            && !mentions.other_potential.is_empty()
            && let Some(state) = app.hub.app()
        {
            let (sender, post, c, names) = (sender.clone(), post.clone(), c.clone(), mentions.other_potential.clone());
            tokio::spawn(async move { out_of_channel_mentions(&state, &sender, &post, &c, names).await });
        }
        let auto_follow = app.cfg.bool("ServiceSettings.ThreadAutoFollow");
        if auto_follow && !post.root_id.is_empty() {
            let mut participants: HashSet<String> = HashSet::new();
            participants.insert(post.user_id.clone());
            let skip_channel_mention = |id: &str, t: Mention| {
                t == Mention::Channel
                    && profiles.get(id).is_some_and(|p| p.notify_prop("channel_mention_auto_follow_threads") == "false")
            };
            if let Some(parent) = parent
                && let Some(root) = parent.order.first().and_then(|id| parent.posts.get(id))
            {
                if !root.from_webhook() && profiles.contains_key(&root.user_id) {
                    participants.insert(root.user_id.clone());
                }
                if c.r#type != channel::DIRECT {
                    for (id, t) in explicit_mentions(root, &k).users {
                        if !skip_channel_mention(&id, t) {
                            participants.insert(id);
                        }
                    }
                }
            }
            for (id, t) in &mentions.users {
                if !skip_channel_mention(id, *t) {
                    participants.insert(id.clone());
                }
            }
            if c.r#type != channel::DIRECT {
                for (id, p) in props.iter() {
                    if !followers.contains(id)
                        && p.0.get("channel_auto_follow_threads").map(String::as_str) == Some("on")
                    {
                        participants.insert(id.clone());
                    }
                }
            }
            for id in participants {
                let mt = mentions.users.get(&id).copied();
                let mut increment = mt.is_some();
                if !increment {
                    match app.thread_membership(&id, &post.root_id).await {
                        Ok(Some(m)) if !m.following => continue,
                        Ok(_) => {}
                        Err(_) => continue,
                    }
                }
                let mut update_following = auto_follow;
                if matches!(mt, Some(Mention::Thread) | Some(Mention::Comment)) {
                    increment = false;
                    update_following = false;
                }
                let opts = MembershipOpts {
                    following: true,
                    increment_mentions: increment,
                    update_following,
                    update_viewed: false,
                    update_participants: id == post.user_id,
                };
                if let Ok(m) = app.maintain_membership(&id, &post.root_id, opts).await {
                    if !followers.contains(&id) && m.following {
                        followers.insert(id.clone());
                        new_participants.insert(id.clone());
                    }
                    memberships.insert(id, m);
                }
            }
        }
        mentioned = mentions.users.keys().cloned().collect();
        if !mentioned.is_empty() {
            let _ = sqlx::query(
                "UPDATE ChannelMembers SET MentionCount = MentionCount + 1, MentionCountRoot = MentionCountRoot + $1,
                     UrgentMentionCount = UrgentMentionCount + $2, LastUpdateAt = $3
                 WHERE UserId = ANY($4) AND ChannelId = $5",
            )
            .bind(i64::from(post.root_id.is_empty()))
            .bind(i64::from(post.is_urgent()))
            .bind(crate::app::now_ms())
            .bind(&mentioned)
            .bind(&post.channel_id)
            .execute(&app.db)
            .await;
            for id in &mentioned {
                app.caches.invalidate_members_for_user(id);
            }
        }
        if crt_allowed && !post.root_id.is_empty() {
            let ids: Vec<String> = followers.iter().cloned().collect();
            let crt = app.crt_for_users(&ids).await;
            for uid in &followers {
                let Some(profile) = profiles.get(uid) else { continue };
                if !crt.get(uid).copied().unwrap_or(false) || (!post.from_webhook() && *uid == post.user_id) {
                    continue;
                }
                let was = mentions.users.contains_key(uid);
                if follower_desktop(profile, was, props.get(uid)) {
                    desktop_followers.push(uid.clone());
                }
                let (email, push) = follower_email_push(profile, was, props.get(uid));
                if email {
                    crt_email.push(uid.clone());
                }
                if push {
                    crt_push.push(uid.clone());
                }
            }
        }
        if c.r#type != channel::DIRECT {
            let ids: Vec<String> = profiles.keys().cloned().collect();
            let crt = if post.root_id.is_empty() { HashMap::new() } else { app.crt_for_users(&ids).await };
            for (id, p) in profiles.iter() {
                let cpush = props.get(id).and_then(|m| m.0.get("push")).map(String::as_str);
                if (p.notify_prop("push") == "all" || cpush == Some("all"))
                    && (post.user_id != *id || post.from_webhook())
                    && !(crt.get(id).copied().unwrap_or(false) && !post.root_id.is_empty())
                {
                    all_activity_push.push(id.clone());
                }
            }
        }
    }

    if !suppressed {
        send_email_and_push(
            app,
            post,
            c,
            sender,
            &profiles,
            &props,
            &mentions,
            &mentioned,
            &crt_email,
            &crt_push,
            &all_activity_push,
        )
        .await;
    }

    // The posted event.
    let channel_display_name = match c.r#type.as_str() {
        channel::DIRECT => format!("@{}", sender.username),
        channel::GROUP => {
            let mut names: Vec<String> = profiles.values().map(|u| u.username.clone()).collect();
            names.sort();
            names.join(", ")
        }
        _ => c.display_name.clone(),
    };
    let sender_name = if post.is_system_message() {
        "System".to_string()
    } else if app.cfg.bool("ServiceSettings.EnablePostUsernameOverride")
        && c.r#type != channel::DIRECT
        && post.from_webhook()
        && let Some(Value::String(o)) = post.prop("override_username")
    {
        o.clone()
    } else {
        format!("@{}", sender.username)
    };
    let bpost = app.post_for_broadcast(post).await;
    let post_hook = bpost.hook();
    let mut data = json!({
        "channel_display_name": channel_display_name, "channel_name": c.name, "channel_type": c.r#type,
        "post": serde_json::to_string(&bpost.post).unwrap(), "sender_name": sender_name, "set_online": set_online,
        "team_id": team_id,
    });
    if !post.file_ids.0.is_empty() {
        data["otherFile"] = "true".into();
        if let Ok(infos) = app.file_infos_for_post(&post.id, false).await
            && infos.iter().any(|f| {
                f.mime_type.starts_with("image")
                    || matches!(
                        f.extension.to_lowercase().as_str(),
                        "jpg" | "jpeg" | "gif" | "bmp" | "png" | "tiff" | "tif" | "webp"
                    )
            })
        {
            data["image"] = "true".into();
        }
    }
    let m_set: HashSet<String> = mentioned.iter().cloned().collect();
    let f_set: HashSet<String> = desktop_followers.iter().cloned().collect();
    let per_user: PerUser = Arc::new(move |uid: &str| {
        let mut extra = serde_json::Map::new();
        if m_set.contains(uid) {
            extra.insert("mentions".into(), Value::String(format!("[\"{uid}\"]")));
        }
        if f_set.contains(uid) {
            extra.insert("followers".into(), Value::String(format!("[\"{uid}\"]")));
        }
        (!extra.is_empty()).then_some(extra)
    });
    // shouldAckWebsocketNotification: the members whose desktop notifications fire for every message here.
    let users = profiles
        .keys()
        .filter(|id| {
            let cl = props.get(*id).and_then(|m| m.0.get("desktop")).map(String::as_str).unwrap_or("");
            let ul = profiles.get(*id).map_or("", |u| u.notify_prop("desktop"));
            cl == "all"
                || (cl == "default" && ul == "all")
                || (c.r#type == channel::GROUP && ((cl == "default" && ul == "mention") || cl == "mention"))
        })
        .cloned()
        .collect();
    let posted_ack =
        Some(Arc::new(PostedAck { poster: post.user_id.clone(), direct: c.r#type == channel::DIRECT, users }));
    let b = if kind::is_add_membership(&post.r#type) && crate::posts::excludes_membership(c) {
        Broadcast {
            user_id: post.prop_str("addedUserId").to_string(),
            per_user: Some(per_user),
            post_hook: post_hook.clone(),
            posted_ack,
            ..Default::default()
        }
    } else {
        Broadcast {
            channel_id: post.channel_id.clone(),
            per_user: Some(per_user),
            post_hook: post_hook.clone(),
            posted_ack,
            ..Default::default()
        }
    };
    app.hub.publish("posted", data, b);

    // thread_updated to followers who use collapsed threads.
    if !suppressed && crt_allowed && !post.root_id.is_empty() {
        let ids: Vec<String> = followers.iter().cloned().collect();
        let crt = app.crt_for_users(&ids).await;
        for uid in followers {
            if !profiles.contains_key(&uid) || !crt.get(&uid).copied().unwrap_or(false) {
                continue;
            }
            let m = match memberships.remove(&uid) {
                Some(m) => m,
                None => match app.thread_membership(&uid, &post.root_id).await? {
                    Some(m) => m,
                    None => continue,
                },
            };
            let Some(mut t) = app.thread_for_user(&m, true).await? else { continue };
            let (mut prev_mentions, mut prev_replies) = (0, 0);
            if !new_participants.contains(&uid) {
                prev_mentions = t.unread_mentions;
                prev_replies = (t.unread_replies - 1).max(0);
                if mentions.is_mentioned(&uid) {
                    prev_mentions = (t.unread_mentions - 1).max(0);
                }
            }
            if uid == post.user_id {
                let _ = app
                    .maintain_membership(
                        &uid,
                        &post.root_id,
                        MembershipOpts { update_viewed: true, ..Default::default() },
                    )
                    .await;
                t.unread_mentions = 0;
                t.unread_replies = 0;
            }
            if let Some(p) = t.post.take() {
                t.post = Some(app.sanitize_for_user(p, &uid).await);
            }
            app.hub.publish(
                "thread_updated",
                json!({"thread": serde_json::to_string(&t).unwrap(), "previous_unread_mentions": prev_mentions,
                       "previous_unread_replies": prev_replies}),
                Broadcast { team_id: team_id.clone(), user_id: uid.clone(), ..Default::default() },
            );
        }
    }
    Ok(mentioned)
}

/// `RemoveNotifications`: a deleted reply's mentions taken off the threads of those it mentioned.
pub async fn remove_notifications(app: &App, post: &Post, c: &Channel) -> ApiResult<()> {
    if !app.crt_allowed() || post.root_id.is_empty() {
        return Ok(());
    }
    let profiles = app.profiles_in_channel(&c.id).await?;
    let props = app.channel_notify_props(&c.id).await?;
    let (m, _) = app.mentions_and_keywords(post, c, &profiles, &props, None).await;
    for uid in m.users.keys() {
        let Some(mut tm) = app.thread_membership(uid, &post.root_id).await? else {
            return Ok(());
        };
        if tm.last_viewed > post.create_at || tm.unread_mentions == 0 {
            continue;
        }
        tm.unread_mentions -= 1;
        app.update_membership(&tm).await?;
        if let Some(mut t) = app.thread_for_user(&tm, true).await? {
            if let Some(p) = t.post.take() {
                t.post = Some(app.sanitize_for_user(p, uid).await);
            }
            app.hub.publish(
                "thread_updated",
                json!({"thread": serde_json::to_string(&t).unwrap(), "previous_unread_mentions": 0, "previous_unread_replies": 0}),
                Broadcast { team_id: c.team_id.clone(), user_id: uid.clone(), ..Default::default() },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, name: &str) -> User {
        let mut u = User { id: id.into(), username: name.into(), ..Default::default() };
        u.notify_props.0.insert("channel".into(), "true".into());
        u.notify_props.0.insert("mention_keys".into(), "deploy,Release".into());
        u
    }

    #[test]
    fn mentions_as_go_parses_them() {
        let mut k = Keywords::default();
        k.add_user(&user("a", "alice"), None, Some("online"), true);
        k.add_user(&user("b", "bob"), None, Some("away"), true);
        let p = |m: &str| Post { message: m.into(), ..Default::default() };
        let m = explicit_mentions(&p("hi @alice, and @Bob."), &k);
        assert_eq!(m.users.len(), 2);
        let m = explicit_mentions(&p("`@alice` in code\n```\n@bob\n```"), &k);
        assert!(m.users.is_empty());
        let m = explicit_mentions(&p("@here now"), &k);
        assert!(m.here && m.users.contains_key("a") && !m.users.contains_key("b"));
        let m = explicit_mentions(&p("the deploy is done"), &k);
        assert_eq!(m.users.len(), 2);
        let m = explicit_mentions(&p("ask @carol.smith"), &k);
        assert_eq!(m.other_potential, vec!["carol.smith".to_string()]);
    }

    #[test]
    fn mentions_in_links() {
        let mut k = Keywords::default();
        k.add_user(&user("a", "alice"), None, Some("online"), true);
        let p = |m: &str| Post { message: m.into(), ..Default::default() };
        for (message, mentioned) in [
            ("https://youtube.com/@alice", false),
            ("http://x/a?u=@alice", false),
            ("ftp://x/@alice", false),
            ("<http://x/@alice>", false),
            ("text (http://x/@alice)", false),
            ("see:http://x/@alice", false),
            ("www.x.com/@alice", false),
            ("[link](http://x/@alice)", false),
            ("[x][r]\n\n[r]: http://x/@alice", false),
            ("http://x/@alice and @alice", true),
            ("[@alice](http://x)", true),
            ("![@alice](http://x/y.png)", true),
            ("x.com/@alice", true),
            ("http://@alice", true),
        ] {
            assert_eq!(explicit_mentions(&p(message), &k).users.contains_key("a"), mentioned, "{message}");
        }
    }
}
