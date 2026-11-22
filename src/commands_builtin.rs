//! The built-in slash commands, each as its Go provider's `DoCommand`.

// Based on Mattermost server/channels/app/slashcommands (command_*.go), Copyright (c) 2015-present Mattermost,
// Inc. Licensed under AGPL-3.0. data/emoji_codes.txt is model.SystemEmojis (emoji_data.go).

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Map, Value, json};

use crate::app::{App, State};
use crate::commands::{CommandArgs, CommandResponse, EPHEMERAL, IN_CHANNEL, ephemeral, tr, trp, user_by_username};
use crate::hub::Broadcast;
use crate::model::{Channel, Post, Preference, Session, channel};
use crate::perm;

const TOWN_SQUARE: &str = "town-square";

/// The response of built-in `trigger`, or None when there is no such built-in (or it is off, as `/status`
/// without custom statuses).
pub async fn run(
    app: &State,
    s: &Session,
    args: &CommandArgs,
    trigger: &str,
    message: &str,
) -> Option<CommandResponse> {
    let r = match trigger {
        "away" => {
            app.set_status_away_if_needed(&args.user_id, true).await;
            ephemeral(tr("api.command_away.success"))
        }
        "online" => {
            app.set_status_online(&args.user_id, true).await;
            ephemeral(tr("api.command_online.success"))
        }
        "offline" => {
            app.set_status_offline(&args.user_id, true, false).await;
            ephemeral(tr("api.command_offline.success"))
        }
        "dnd" => {
            app.set_status_dnd(&args.user_id, None).await;
            ephemeral(tr("api.command_dnd.success"))
        }
        "me" => CommandResponse {
            response_type: IN_CHANNEL.into(),
            kind: "me".into(),
            text: format!("*{message}*"),
            ..Default::default()
        },
        "shrug" => {
            let shrug = r"¯\\\_(ツ)\_/¯";
            let text = if message.is_empty() { shrug.to_string() } else { format!("{message} {shrug}") };
            CommandResponse { response_type: IN_CHANNEL.into(), text, ..Default::default() }
        }
        "code" if message.is_empty() => ephemeral(tr("api.command_code.message.app_error")),
        "code" => CommandResponse {
            response_type: IN_CHANNEL.into(),
            text: format!("    {}", message.split('\n').collect::<Vec<_>>().join("\n    ")),
            skip_slack_parsing: true,
            ..Default::default()
        },
        "echo" => echo(app, args, message),
        "join" | "open" => join(app, args, message).await,
        "leave" => leave(app, args).await,
        "msg" => msg(app, s, args, message).await,
        "groupmsg" => groupmsg(app, s, args, message).await,
        "mute" => mute(app, args, message).await,
        "header" => header(app, args, message).await,
        "purpose" => purpose(app, args, message).await,
        "rename" => rename(app, args, message).await,
        "invite" => ephemeral(invite(app, args, message).await),
        "kick" | "remove" => remove(app, args, message).await,
        "invite_people" => invite_people(app, args, message).await,
        "search" => ephemeral(tr("api.command_search.unsupported.app_error")),
        "settings" => ephemeral(tr("api.command_settings.unsupported.app_error")),
        "shortcuts" => ephemeral(tr("api.command_shortcuts.unsupported.app_error")),
        "marketplace" => ephemeral(tr("api.command_marketplace.unsupported.app_error")),
        "expand" => collapse(app, args, false).await,
        "collapse" => collapse(app, args, true).await,
        "logout" => CommandResponse { goto_location: "/login".into(), ..Default::default() },
        "help" => {
            let link = app.cfg.str("SupportSettings.HelpLink");
            let link = if link.is_empty() { "https://mattermost.com/pl/help/" } else { link };
            ephemeral(trp("api.command_help.success", &[("HelpLink", link)]))
        }
        "status" => return custom_status(app, args, message).await,
        "mobile-logs" => mobile_logs(app, s, args, message).await,
        "share-channel" => {
            if !app.has_perm(s, "manage_shared_channels").await {
                ephemeral(trp("api.command_share.permission_required", &[("Permission", "manage_shared_channels")]))
            } else {
                ephemeral(tr("api.command_share.service_disabled"))
            }
        }
        "secure-connection" => {
            if !app.has_perm(s, "manage_secure_connections").await {
                ephemeral(trp("api.command_remote.permission_required", &[("Permission", "manage_secure_connections")]))
            } else {
                ephemeral(tr("api.command_remote.service_disabled"))
            }
        }
        _ => return None,
    };
    Some(r)
}

fn empty() -> CommandResponse {
    CommandResponse::default()
}

fn goto(location: String) -> CommandResponse {
    CommandResponse { goto_location: location, ..Default::default() }
}

// ---------- posts ----------

/// `/echo "message" [delay]`: posted later, at most 100 waiting.
fn echo(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    static WAITING: AtomicUsize = AtomicUsize::new(0);
    if message.is_empty() {
        return ephemeral(tr("api.command_echo.message.app_error"));
    }
    let mut message = message.to_string();
    let mut delay: i64 = 0;
    let end = message.rfind('"');
    if message.starts_with('"') && end.is_some_and(|e| e > 1) {
        let end = end.unwrap();
        if let Ok(d) = message[end..].trim_matches([' ', '"']).parse::<i64>() {
            delay = d;
        }
        message = message[1..end].to_string();
    } else if message.contains(' ') {
        let i = message.rfind(' ').unwrap();
        if let Ok(d) = message[i..].trim_matches(' ').parse::<i64>() {
            delay = d;
            message.truncate(i);
        }
    }
    if delay > 10000 {
        return ephemeral(tr("api.command_echo.delay.app_error"));
    }
    if WAITING.load(Ordering::Relaxed) >= 100 {
        return ephemeral(tr("api.command_echo.high_volume.app_error"));
    }
    WAITING.fetch_add(1, Ordering::Relaxed);
    let app = app.clone();
    let post = Post {
        channel_id: args.channel_id.clone(),
        root_id: args.root_id.clone(),
        message,
        user_id: args.user_id.clone(),
        ..Default::default()
    };
    tokio::spawn(async move {
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay as u64)).await;
        }
        if let Err(e) = app.create_post(post, true).await {
            eprintln!("unable to create /echo post: {}", e.message);
        }
        WAITING.fetch_sub(1, Ordering::Relaxed);
    });
    empty()
}

// ---------- channels ----------

/// `/join ~channel` (and `/open`).
async fn join(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    let fail = || ephemeral(tr("api.command_join.fail.app_error"));
    let name = match message.strip_prefix('~') {
        Some(n) => n.to_string(),
        None => message.to_lowercase(),
    };
    let Ok(Some(c)) = app.channel_by_name(&args.team_id, &name, false, true).await else {
        return ephemeral(tr("api.command_join.list.app_error"));
    };
    if c.name != name {
        return ephemeral(tr("api.command_join.missing.app_error"));
    }
    let needed = match c.r#type.as_str() {
        channel::OPEN => perm::JOIN_PUBLIC_CHANNELS,
        channel::PRIVATE => perm::READ_CHANNEL,
        _ => return fail(),
    };
    if !app.user_channel_perm(&args.user_id, &c.id, needed).await.0 {
        return fail();
    }
    if app.join_channel(&c, &args.user_id).await.is_err() {
        return fail();
    }
    let Ok(team) = app.team(&c.team_id).await else { return fail() };
    goto(format!("{}/{}/channels/{}", args.site_url, team.name, c.name))
}

/// `/leave`.
async fn leave(app: &State, args: &CommandArgs) -> CommandResponse {
    let fail = || ephemeral(tr("api.command_leave.fail.app_error"));
    let Ok(c) = app.get_channel(&args.channel_id).await else { return fail() };
    let Ok(team) = app.team(&args.team_id).await else { return fail() };
    if app.leave_channel(&args.channel_id, &args.user_id).await.is_err() {
        if c.name == TOWN_SQUARE {
            return ephemeral(trp("api.channel.leave.default.app_error", &[("Channel", TOWN_SQUARE)]));
        }
        return fail();
    }
    match app.team_member(&team.id, &args.user_id).await {
        Ok(Some(m)) if m.delete_at == 0 => {}
        _ => return goto(format!("{}/", args.site_url)),
    }
    let Ok(Some(user)) = app.user(&args.user_id).await else { return fail() };
    if user.is_guest() {
        let first: Option<String> = sqlx::query_scalar(
            "SELECT cm.ChannelId FROM ChannelMembers cm JOIN Channels c ON c.Id = cm.ChannelId
             WHERE cm.UserId = $1 AND (c.TeamId = $2 OR c.TeamId = '') AND c.DeleteAt = 0 LIMIT 1",
        )
        .bind(&args.user_id)
        .bind(&team.id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten();
        let Some(first) = first else { return fail() };
        let Ok(c) = app.get_channel(&first).await else { return fail() };
        return goto(format!("{}/{}/channels/{}", args.site_url, team.name, c.name));
    }
    goto(format!("{}/{}/channels/{TOWN_SQUARE}", args.site_url, team.name))
}

/// `/msg @user message`.
async fn msg(app: &State, s: &Session, args: &CommandArgs, message: &str) -> CommandResponse {
    let missing = || ephemeral(tr("api.command_msg.missing.app_error"));
    let fail = || ephemeral(tr("api.command_msg.fail.app_error"));
    let (target, text) = match message.split_once(' ') {
        Some((t, m)) => (t, m),
        None => (message, ""),
    };
    let target = target.strip_prefix('@').unwrap_or(target);
    let Ok(Some(other)) = user_by_username(app, target).await else { return missing() };
    if other.id == args.user_id {
        return missing();
    }
    match app.can_see_user(s, &other.id).await {
        Err(_) => return fail(),
        Ok(false) => return missing(),
        Ok(true) => {}
    }
    let name = channel::direct_name(&args.user_id, &other.id);
    let target_id = match app.channel_by_name(&args.team_id, &name, false, true).await {
        Ok(Some(c)) => c.id.clone(),
        Ok(None) => {
            if !app.user_has_perm(&args.user_id, perm::CREATE_DIRECT_CHANNEL).await {
                return ephemeral(tr("api.command_msg.permission.app_error"));
            }
            match app.get_or_create_direct(s, &args.user_id, &other.id).await {
                Ok(c) => c.id,
                Err(e) => return ephemeral(tr(&e.id)),
            }
        }
        Err(_) => return ephemeral(tr("api.command_msg.dm_fail.app_error")),
    };
    if !text.is_empty() {
        let post =
            Post { message: text.into(), channel_id: target_id, user_id: args.user_id.clone(), ..Default::default() };
        if app.create_post(post, true).await.is_err() {
            return fail();
        }
    }
    let Ok(team) = app.team(&args.team_id).await else { return fail() };
    CommandResponse {
        goto_location: format!("{}/{}/channels/{name}", args.site_url, team.name),
        response_type: EPHEMERAL.into(),
        ..Default::default()
    }
}

/// `groupMsgUsernames`: the names before the message, comma separated.
fn group_msg_usernames(message: &str) -> (Vec<String>, String) {
    let mut names = vec![];
    let mut text = String::new();
    for (idx, part) in message.split(',').enumerate() {
        let clean = part.trim();
        let clean = clean.strip_prefix('@').unwrap_or(clean);
        let words: Vec<&str> = clean.split_whitespace().collect();
        if let Some(first) = words.first() {
            names.push(first.to_string());
        }
        if words.len() > 1 {
            let pieces: Vec<&str> = message.splitn(idx + 1, ',').collect();
            let last = pieces.last().unwrap_or(&"").trim();
            let last = last.strip_prefix('@').unwrap_or(last);
            text = last.strip_prefix(words[0]).unwrap_or(last).trim().to_string();
            break;
        }
    }
    (names, text)
}

/// `/groupmsg @a,@b message`.
async fn groupmsg(app: &State, s: &Session, args: &CommandArgs, message: &str) -> CommandResponse {
    let fail = || ephemeral(tr("api.command_groupmsg.fail.app_error"));
    let (names, text) = group_msg_usernames(message);
    let mut ids = vec![args.user_id.clone()];
    let mut usernames: HashMap<String, String> = HashMap::new();
    let mut invalid = vec![];
    for name in names {
        let name = name.trim();
        let name = name.strip_prefix('@').unwrap_or(name).to_string();
        let Ok(Some(u)) = user_by_username(app, &name).await else {
            invalid.push(name);
            continue;
        };
        match app.can_see_user(s, &u.id).await {
            Err(_) => return fail(),
            Ok(false) => {
                invalid.push(name);
                continue;
            }
            Ok(true) => {}
        }
        if !usernames.contains_key(&u.id) && u.id != args.user_id {
            usernames.insert(u.id.clone(), u.username.clone());
            ids.push(u.id.clone());
        }
    }
    if !invalid.is_empty() {
        let users = format!("@{}", invalid.join(", @"));
        let t = crate::i18n::tr_n("api.command_groupmsg.invalid_user.app_error", invalid.len(), &[("Users", &users)])
            .unwrap_or_default();
        return ephemeral(t);
    }
    if ids.len() == 2 {
        let other = &usernames[&ids[1]];
        return Box::pin(msg(app, s, args, &format!("{other} {text}"))).await;
    }
    if ids.len() < 3 {
        return ephemeral(trp("api.command_groupmsg.min_users.app_error", &[("MinUsers", "2")]));
    }
    if ids.len() > 8 {
        return ephemeral(trp("api.command_groupmsg.max_users.app_error", &[("MaxUsers", "7")]));
    }
    let group = if app.user_has_perm(&args.user_id, perm::CREATE_GROUP_CHANNEL).await {
        match app.create_group_channel(&ids, &args.user_id).await {
            Ok(c) => c,
            Err(_) => return ephemeral(tr("api.command_groupmsg.group_fail.app_error")),
        }
    } else {
        match app.channel_by_name("", &crate::channels::group_name(&ids), false, true).await {
            Ok(Some(c)) => (*c).clone(),
            _ => return ephemeral(tr("api.command_groupmsg.permission.app_error")),
        }
    };
    if !text.is_empty() {
        let post =
            Post { message: text, channel_id: group.id.clone(), user_id: args.user_id.clone(), ..Default::default() };
        if app.create_post(post, true).await.is_err() {
            return fail();
        }
    }
    let Ok(team) = app.team(&args.team_id).await else { return fail() };
    CommandResponse {
        goto_location: format!("{}/{}/channels/{}", args.site_url, team.name, group.name),
        response_type: EPHEMERAL.into(),
        ..Default::default()
    }
}

/// `/mute [~channel]`: `ToggleMuteChannel`.
async fn mute(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    let Ok(mut c) = app.get_channel(&args.channel_id).await else {
        return ephemeral(tr("api.command_mute.no_channel.error"));
    };
    let first = message.split(' ').next().unwrap_or("");
    let name = first.strip_prefix('~').unwrap_or(first).to_string();
    let not_found = |n: &str| ephemeral(trp("api.command_mute.error", &[("Channel", n)]));
    if !name.is_empty() && !message.is_empty() {
        match app.channel_by_name(&c.team_id, &name, false, true).await {
            Ok(Some(found)) => c = found,
            _ => return not_found(&name),
        }
    }
    let Ok(Some(m)) = app.channel_member(&c.id, &args.user_id).await else { return not_found(&name) };
    let muted = m.notify_props.0.get("mark_unread").map(String::as_str) == Some("mention");
    let mut props = Map::new();
    props.insert("mark_unread".into(), Value::String(if muted { "all" } else { "mention" }.into()));
    let Ok(m) = app.update_member_notify_props(&props, &c.id, &args.user_id).await else { return not_found(&name) };
    let now_muted = m.notify_props.0.get("mark_unread").map(String::as_str) == Some("mention");
    if c.is_group_or_direct() {
        return ephemeral(tr(if now_muted {
            "api.command_mute.success_mute_direct_msg"
        } else {
            "api.command_mute.success_unmute_direct_msg"
        }));
    }
    let id = if now_muted { "api.command_mute.success_mute" } else { "api.command_mute.success_unmute" };
    ephemeral(trp(id, &[("Channel", &c.display_name)]))
}

/// The channel's `manage_*_channel_properties` for header, purpose and rename: (allowed, a DM or GM).
async fn may_manage_properties(app: &App, user_id: &str, c: &Channel) -> Option<bool> {
    let needed = match c.r#type.as_str() {
        channel::OPEN => perm::MANAGE_PUBLIC_CHANNEL_PROPERTIES,
        channel::PRIVATE => perm::MANAGE_PRIVATE_CHANNEL_PROPERTIES,
        _ => return None,
    };
    Some(app.user_channel_perm(user_id, &c.id, needed).await.0)
}

async fn patch(app: &State, c: &Channel, key: &str, value: &str, user_id: &str) -> Result<(), String> {
    let mut p = Map::new();
    p.insert(key.into(), Value::String(value.into()));
    app.patch_channel(c.clone(), &p, user_id).await.map(drop).map_err(|e| e.id)
}

/// `/header text`.
async fn header(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    let denied = || ephemeral(tr("api.command_channel_header.permission.app_error"));
    let Ok(c) = app.get_channel(&args.channel_id).await else {
        return ephemeral(tr("api.command_channel_header.channel.app_error"));
    };
    match may_manage_properties(app, &args.user_id, &c).await {
        Some(false) => return denied(),
        Some(true) => {}
        None if c.is_group_or_direct() => {
            if !matches!(app.channel_member(&c.id, &args.user_id).await, Ok(Some(_))) {
                return denied();
            }
        }
        None => return denied(),
    }
    if message.is_empty() {
        return ephemeral(tr("api.command_channel_header.message.app_error"));
    }
    match patch(app, &c, "header", message, &args.user_id).await {
        Ok(()) => empty(),
        Err(id) if id == "model.channel.is_valid.header.app_error" => {
            ephemeral(trp("api.command_channel_header.update_channel.max_length", &[("MaxLength", "1024")]))
        }
        Err(_) => ephemeral(tr("api.command_channel_header.update_channel.app_error")),
    }
}

/// `/purpose text`.
async fn purpose(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    let Ok(c) = app.get_channel(&args.channel_id).await else {
        return ephemeral(tr("api.command_channel_purpose.channel.app_error"));
    };
    match may_manage_properties(app, &args.user_id, &c).await {
        Some(true) => {}
        Some(false) => return ephemeral(tr("api.command_channel_purpose.permission.app_error")),
        None => return ephemeral(tr("api.command_channel_purpose.direct_group.app_error")),
    }
    if message.is_empty() {
        return ephemeral(tr("api.command_channel_purpose.message.app_error"));
    }
    match patch(app, &c, "purpose", message, &args.user_id).await {
        Ok(()) => empty(),
        Err(id) if id == "model.channel.is_valid.purpose.app_error" => {
            ephemeral(trp("api.command_channel_purpose.update_channel.max_length", &[("MaxLength", "250")]))
        }
        Err(_) => ephemeral(tr("api.command_channel_purpose.update_channel.app_error")),
    }
}

/// `/rename text`.
async fn rename(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    let Ok(c) = app.get_channel(&args.channel_id).await else {
        return ephemeral(tr("api.command_channel_rename.channel.app_error"));
    };
    match may_manage_properties(app, &args.user_id, &c).await {
        Some(true) => {}
        Some(false) => return ephemeral(tr("api.command_channel_rename.permission.app_error")),
        None => return ephemeral(tr("api.command_channel_rename.direct_group.app_error")),
    }
    if message.is_empty() {
        return ephemeral(tr("api.command_channel_rename.message.app_error"));
    }
    if message.len() > 64 {
        return ephemeral(trp("api.command_channel_rename.too_long.app_error", &[("Length", "64")]));
    }
    match patch(app, &c, "display_name", message, &args.user_id).await {
        Ok(()) => empty(),
        Err(_) => ephemeral(tr("api.command_channel_rename.update_channel.app_error")),
    }
}

// ---------- members ----------

enum Added {
    Ok,
    InChannel,
    NotInTeam,
    Constrained,
    Failed,
}

/// `/invite @user... ~channel...`: the text of the answer.
async fn invite(app: &State, args: &CommandArgs, message: &str) -> String {
    if message.is_empty() {
        return tr("api.command_invite.missing_message.app_error");
    }
    let mut resps: Vec<String> = vec![];
    // parseMessage
    let mut users = vec![];
    let mut channels: Vec<std::sync::Arc<Channel>> = vec![];
    for (j, word) in message.split(' ').enumerate() {
        if word.is_empty() {
            continue;
        }
        if word.starts_with('@') || (!word.starts_with('~') && j == 0) {
            let name = word.strip_prefix('@').unwrap_or(word);
            match user_by_username(app, name).await {
                Ok(Some(u)) if u.delete_at == 0 => users.push(u),
                _ => resps.push(trp("api.command_invite.missing_user.app_error", &[("User", name)])),
            }
        } else {
            let name = word.strip_prefix('~').unwrap_or(word);
            match app.channel_by_name(&args.team_id, name, false, true).await {
                Ok(Some(c)) => channels.push(c),
                _ => resps.push(trp("api.command_invite.channel.error", &[("Channel", name)])),
            }
        }
    }
    if users.is_empty() {
        return if resps.is_empty() { tr("api.command_invite.missing_message.app_error") } else { resps.join("\n") };
    }
    if channels.is_empty() {
        if !resps.is_empty() {
            return resps.join("\n");
        }
        match app.get_channel(&args.channel_id).await {
            Ok(c) => channels.push(c),
            Err(_) => return tr("api.command_invite.channel.app_error"),
        }
    }
    // checkPermissions
    let first = users[0].username.clone();
    let mut valid = vec![];
    for c in channels {
        let needed = match c.r#type.as_str() {
            channel::OPEN => perm::MANAGE_PUBLIC_CHANNEL_MEMBERS,
            channel::PRIVATE => perm::MANAGE_PRIVATE_CHANNEL_MEMBERS,
            _ => {
                resps.push(tr("api.command_invite.directchannel.app_error"));
                continue;
            }
        };
        if !app.user_channel_perm(&args.user_id, &c.id, needed).await.0 {
            if c.r#type == channel::OPEN || matches!(app.channel_member(&c.id, &args.user_id).await, Ok(Some(_))) {
                resps.push(trp("api.command_invite.permission.app_error", &[("User", &first), ("Channel", &c.name)]));
            } else {
                resps.push(trp("api.command_invite.private_channel.app_error", &[("Channel", &c.name)]));
            }
            continue;
        }
        valid.push(c);
    }
    let mut different: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut not_in_team: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut constrained, mut in_channel, mut errors) = (vec![], vec![], vec![]);
    for c in &valid {
        let mut team_display = String::new();
        for u in &users {
            let added = match app.channel_member(&c.id, &u.id).await {
                Ok(Some(_)) => Added::InChannel,
                _ => match app.add_channel_member(c, &u.id, &args.user_id, "").await {
                    Ok(_) => Added::Ok,
                    Err(e) if e.id == "api.channel.add_members.user_denied" => Added::Constrained,
                    Err(e)
                        if e.id == "app.team.get_member.missing.app_error"
                            || e.id == "api.channel.add_user.to.channel.failed.deleted.app_error" =>
                    {
                        Added::NotInTeam
                    }
                    Err(_) => Added::Failed,
                },
            };
            match added {
                Added::Ok if args.channel_id != c.id => {
                    different.entry(c.name.clone()).or_default().push(u.username.clone())
                }
                Added::Ok => {}
                Added::NotInTeam => {
                    if team_display.is_empty() {
                        team_display =
                            app.team(&c.team_id).await.map(|t| t.display_name.clone()).unwrap_or("unknown".into());
                    }
                    not_in_team.entry(team_display.clone()).or_default().push(u.username.clone());
                }
                Added::Constrained => constrained.push(u.username.clone()),
                Added::InChannel => in_channel.push(u.username.clone()),
                Added::Failed => errors.push(u.username.clone()),
            }
        }
    }
    if !in_channel.is_empty() {
        if in_channel.len() > 10 {
            let others = (in_channel.len() - 1).to_string();
            resps.push(trp(
                "api.command_invite.user_already_in_channel.overflow",
                &[("FirstUser", &format!("@{}", in_channel[0])), ("Others", &others)],
            ));
        } else {
            let user = format!("@{}", in_channel.join(", @"));
            resps.push(
                crate::i18n::tr_n(
                    "api.command_invite.user_already_in_channel.app_error",
                    in_channel.len(),
                    &[("User", &user)],
                )
                .unwrap_or_default(),
            );
        }
    }
    for (channel_name, names) in &different {
        if names.len() > 10 {
            let others = (names.len() - 1).to_string();
            resps.push(trp(
                "api.command_invite.successOverflow",
                &[("FirstUser", &format!("@{}", names[0])), ("Others", &others), ("Channel", channel_name)],
            ));
        } else {
            let user = format!("@{}", names.join(", @"));
            resps.push(trp("api.command_invite.success", &[("User", &user), ("Channel", channel_name)]));
        }
    }
    for (team, names) in &not_in_team {
        if names.len() > 10 {
            let others = (names.len() - 1).to_string();
            resps.push(trp(
                "api.command_invite.user_not_in_team.messageOverflow",
                &[("FirstUser", &format!("@{}", names[0])), ("Others", &others), ("Team", team)],
            ));
        } else {
            let users = format!("@{}", names.join(", @"));
            resps.push(trp("api.command_invite.user_not_in_team.app_error", &[("Users", &users), ("Team", team)]));
        }
    }
    if !constrained.is_empty() {
        resps.push(tr("api.command_invite.channel_constrained_user_denied"));
    }
    if !errors.is_empty() {
        resps.push(tr("api.command_invite.fail.app_error"));
    }
    resps.join("\n")
}

/// `/kick @user` and `/remove @user`.
async fn remove(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    let Ok(c) = app.get_channel(&args.channel_id).await else {
        return ephemeral(tr("api.command_channel_remove.channel.app_error"));
    };
    let needed = match c.r#type.as_str() {
        channel::OPEN => perm::MANAGE_PUBLIC_CHANNEL_MEMBERS,
        channel::PRIVATE => perm::MANAGE_PRIVATE_CHANNEL_MEMBERS,
        _ => return ephemeral(tr("api.command_remove.direct_group.app_error")),
    };
    if !app.user_channel_perm(&args.user_id, &args.channel_id, needed).await.0 {
        return ephemeral(tr("api.command_remove.permission.app_error"));
    }
    if message.is_empty() {
        return ephemeral(tr("api.command_remove.message.app_error"));
    }
    let name = message.split(' ').next().unwrap_or("");
    let name = name.strip_prefix('@').unwrap_or(name);
    let user = match user_by_username(app, name).await {
        Ok(Some(u)) if u.delete_at == 0 => u,
        _ => return ephemeral(tr("api.command_remove.missing.app_error")),
    };
    if !matches!(app.channel_member(&args.channel_id, &user.id).await, Ok(Some(_))) {
        let display = user.display_name(app.cfg.str("TeamSettings.TeammateNameDisplay"));
        return ephemeral(trp("api.command_remove.user_not_in_channel", &[("Username", &display)]));
    }
    match app.remove_user_from_channel(&user.id, &args.user_id, &c).await {
        Ok(()) => empty(),
        Err(e) if e.id == "api.channel.remove_members.denied" => {
            ephemeral(tr("api.command_remove.group_constrained_user_denied"))
        }
        Err(e) => ephemeral(trp(&e.id, &[("Channel", TOWN_SQUARE)])),
    }
}

/// `/invite_people email...`: `InviteNewUsersToTeam`.
async fn invite_people(app: &State, args: &CommandArgs, message: &str) -> CommandResponse {
    if !app.user_team_perm(&args.user_id, &args.team_id, perm::INVITE_USER).await
        || !app.user_team_perm(&args.user_id, &args.team_id, perm::ADD_USER_TO_TEAM).await
    {
        return ephemeral(tr("api.command_invite_people.permission.app_error"));
    }
    if !app.cfg.bool("EmailSettings.SendEmailNotifications") {
        return ephemeral(tr("api.command.invite_people.email_off"));
    }
    if !app.cfg.bool("TeamSettings.EnableUserCreation") {
        return ephemeral(tr("api.command.invite_people.invite_off"));
    }
    if !app.cfg.bool("ServiceSettings.EnableEmailInvitations") {
        return ephemeral(tr("api.command.invite_people.email_invitations_off"));
    }
    let emails: Vec<String> = message
        .split_whitespace()
        .map(|t| t.trim_matches(','))
        .filter(|t| t.contains('@'))
        .map(str::to_string)
        .collect();
    if emails.is_empty() {
        return ephemeral(tr("api.command.invite_people.no_email"));
    }
    match crate::teams::invite_new_users_to_team(app, emails, &args.team_id, &args.user_id).await {
        Ok(()) => ephemeral(tr("api.command.invite_people.sent")),
        Err(_) => ephemeral(tr("api.command.invite_people.fail")),
    }
}

// ---------- settings ----------

/// `/expand` and `/collapse`: the `collapse_previews` display setting.
async fn collapse(app: &State, args: &CommandArgs, collapse: bool) -> CommandResponse {
    let pref = Preference {
        user_id: args.user_id.clone(),
        category: "display_settings".into(),
        name: "collapse_previews".into(),
        value: collapse.to_string(),
    };
    let r = sqlx::query(
        "INSERT INTO Preferences (UserId, Category, Name, Value) VALUES ($1, $2, $3, $4)
         ON CONFLICT (UserId, Category, Name) DO UPDATE SET Value = EXCLUDED.Value",
    )
    .bind(&pref.user_id)
    .bind(&pref.category)
    .bind(&pref.name)
    .bind(&pref.value)
    .execute(&app.db)
    .await;
    if let Err(e) = r {
        return ephemeral(format!("{}{e}", tr("api.command_expand_collapse.fail.app_error")));
    }
    app.hub.publish(
        "preference_changed",
        json!({ "preference": serde_json::to_string(&pref).unwrap() }),
        Broadcast { user_id: args.user_id.clone(), ..Default::default() },
    );
    ephemeral(tr(if collapse { "api.command_collapse.success" } else { "api.command_expand.success" }))
}

/// `model.SystemEmojis` by unicode: the names of each code point sequence, sorted.
fn emoji_by_unicode() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static M: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m: HashMap<&str, Vec<&str>> = HashMap::new();
        for line in include_str!("../data/emoji_codes.txt").lines() {
            if let Some((name, code)) = line.split_once(' ') {
                m.entry(code).or_default().push(name);
            }
        }
        for v in m.values_mut() {
            v.sort();
        }
        m
    })
}

fn emoji_name_from_unicode(code: &str) -> Option<&'static str> {
    emoji_by_unicode().get(code).and_then(|v| v.first().copied())
}

/// `removeUnicodeSkinTone`.
fn remove_skin_tone(code: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new("-(1f3fb|1f3fc|1f3fd|1f3fe|1f3ff)").unwrap());
    let Some(m) = re.find(code) else { return code.to_string() };
    if emoji_name_from_unicode(code).is_some() {
        return code.to_string();
    }
    let removed = format!("{}{}", &code[..m.start()], &code[m.end()..]);
    let variation = format!("{}-fe0f{}", &code[..m.start()], &code[m.end()..]);
    if emoji_name_from_unicode(&removed).is_some() {
        removed
    } else if emoji_name_from_unicode(&variation).is_some() {
        variation
    } else {
        code.to_string()
    }
}

/// `GetCustomStatus`: a leading `:emoji:` or unicode emoji, then the text.
fn parse_custom_status(message: &str) -> crate::user_edit::CustomStatus {
    let mut cs: crate::user_edit::CustomStatus =
        serde_json::from_value(json!({"emoji": "speech_balloon", "text": message})).unwrap();
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(":[a-zA-Z0-9_+-]+:").unwrap());
    if let Some(m) = re.find(message)
        && m.start() == 0
    {
        cs.emoji = message[1..m.end() - 1].to_string();
        cs.text = message[m.end()..].trim().to_string();
        return cs;
    }
    let words: Vec<&str> = message.split_whitespace().collect();
    let Some(first) = words.first() else { return cs };
    let code: Vec<String> = first.chars().map(|c| format!("{:x}", c as u32)).collect();
    let code = remove_skin_tone(&code.join("-"));
    if let Some(name) = emoji_name_from_unicode(&code) {
        cs.emoji = name.to_string();
        cs.text = words[1..].join(" ").trim().to_string();
    }
    cs
}

/// `/status [:emoji:] [text]` or `/status clear`.
async fn custom_status(app: &State, args: &CommandArgs, message: &str) -> Option<CommandResponse> {
    if !app.cfg.bool("TeamSettings.EnableCustomUserStatuses") {
        return None;
    }
    let message = message.trim();
    if message == "clear" {
        return Some(match app.remove_custom_status(&args.user_id).await {
            Ok(()) => ephemeral(tr("api.command_custom_status.clear.success")),
            Err(_) => ephemeral(tr("api.command_custom_status.clear.app_error")),
        });
    }
    let mut cs = parse_custom_status(message);
    cs.pre_save();
    let (emoji, text) = (cs.emoji.clone(), cs.text.clone());
    Some(match app.set_custom_status(&args.user_id, cs).await {
        Ok(()) => ephemeral(trp(
            "api.command_custom_status.success",
            &[("EmojiName", &format!(":{emoji}:")), ("StatusMessage", &text)],
        )),
        Err(_) => ephemeral(tr("api.command_custom_status.app_error")),
    })
}

/// `/mobile-logs on|off|status [@user]`.
async fn mobile_logs(app: &State, s: &Session, args: &CommandArgs, message: &str) -> CommandResponse {
    let usage = || ephemeral(tr("api.command_mobile_logs.usage"));
    let unavailable = || ephemeral(tr("api.command_mobile_logs.cross_user_unavailable.app_error"));
    let fields: Vec<&str> = message.split_whitespace().collect();
    if fields.is_empty() || fields.len() > 2 {
        return usage();
    }
    let action = fields[0].to_lowercase();
    if !["on", "off", "status"].contains(&action.as_str()) {
        return usage();
    }
    let mut target = args.user_id.clone();
    let mut display = tr("api.command_mobile_logs.you");
    if let Some(name) = fields.get(1) {
        let name = name.strip_prefix('@').unwrap_or(name).to_lowercase();
        let not_found = || ephemeral(trp("api.command_mobile_logs.user_not_found.app_error", &[("Username", &name)]));
        if !crate::user_edit::valid_username(&name) {
            return not_found();
        }
        let Ok(Some(caller)) = app.user(&args.user_id).await else {
            return ephemeral(tr("api.command_mobile_logs.update_error.app_error"));
        };
        if name == caller.username.to_lowercase() {
            target = caller.id.clone();
            display = format!("@{}", caller.username);
        } else {
            let sysadmin = app.has_perm(s, perm::MANAGE_SYSTEM).await;
            if !sysadmin && !app.has_perm(s, perm::EDIT_OTHER_USERS).await {
                return unavailable();
            }
            let other = match user_by_username(app, &name).await {
                Ok(Some(u)) if u.delete_at == 0 => u,
                _ => return if sysadmin { not_found() } else { unavailable() },
            };
            if !sysadmin && other.is_system_admin() {
                return unavailable();
            }
            target = other.id.clone();
            display = format!("@{}", other.username);
        }
    }
    let user = [("User", display.as_str())];
    match action.as_str() {
        "on" | "off" => {
            let value = if action == "on" { "true" } else { "false" };
            let pref = Preference {
                user_id: target.clone(),
                category: "advanced_settings".into(),
                name: "attach_app_logs".into(),
                value: value.into(),
            };
            if app.update_preferences(&target, vec![pref]).await.is_err() {
                return ephemeral(tr("api.command_mobile_logs.update_error.app_error"));
            }
            let id =
                if action == "on" { "api.command_mobile_logs.enabled" } else { "api.command_mobile_logs.disabled" };
            ephemeral(trp(id, &user))
        }
        _ => match app.preference(&target, "advanced_settings", "attach_app_logs").await {
            Ok(Some(p)) if p.value == "true" => ephemeral(trp("api.command_mobile_logs.status_on", &user)),
            _ => ephemeral(trp("api.command_mobile_logs.status_off", &user)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groupmsg_names_and_message() {
        assert_eq!(group_msg_usernames("@a,@b hello there"), (vec!["a".into(), "b".into()], "hello there".into()));
        assert_eq!(group_msg_usernames("@a, @b"), (vec!["a".into(), "b".into()], String::new()));
    }

    #[test]
    fn custom_status_emoji() {
        let cs = parse_custom_status(":taco: lunch");
        assert_eq!((cs.emoji.as_str(), cs.text.as_str()), ("taco", "lunch"));
        let cs = parse_custom_status("🌮 lunch");
        assert_eq!((cs.emoji.as_str(), cs.text.as_str()), ("taco", "lunch"));
        let cs = parse_custom_status("just text");
        assert_eq!((cs.emoji.as_str(), cs.text.as_str()), ("speech_balloon", "just text"));
    }
}
