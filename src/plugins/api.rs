//! The plugin API: the calls a plugin makes on its API stream (`Plugin.GetUser`, `Plugin.KVSet`...), answered with
//! this server's functions. Arguments and results are converted by the method table (`schema_gen::API`); a method
//! this server does not implement answers with an error in its `*AppError` (or `error`) result, logged once.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use goplugin::gob::Value;
use goplugin::host::error_value;
use goplugin::rpc::Reply;
use serde_json::{Map, Value as J, json};

use super::kv;
use super::schema::{self, Args, Out};
use crate::app::{ApiResult, AppError, State};
use crate::model::Session;

/// The plugin a call comes from.
pub struct Ctx {
    pub app: State,
    pub id: String,
    pub dir: PathBuf,
    pub manifest: J,
}

pub fn call(ctx: Arc<Ctx>, method: String, a: Value) -> Reply {
    Box::pin(async move {
        let name = method.strip_prefix("Plugin.").unwrap_or(&method).to_string();
        let Some((args, rets)) = schema::api(&name) else {
            anyhow::bail!("rpc: can't find method {method}");
        };
        let r = dispatch(&ctx, &name, &Args::new(args, &a)).await;
        Ok(match r {
            Ok(outs) => schema::pack(rets, outs),
            Err(e) => {
                let mut outs: Vec<Out> = rets.iter().map(|_| Out::J(J::Null)).collect();
                match (rets.last().map(|(_, t)| *t), outs.last_mut()) {
                    (Some("*AppError"), Some(last)) => *last = Out::J(serde_json::to_value(&e).unwrap_or_default()),
                    (Some("error"), Some(last)) => *last = Out::Raw(error_value(&e.message)),
                    _ => {}
                }
                if e.id == "api.not_implemented.app_error" {
                    missing(&ctx.id, &name);
                }
                schema::pack(rets, outs)
            }
        })
    })
}

/// Logs an API method a plugin called that this server does not implement, once per plugin and method.
fn missing(plugin_id: &str, method: &str) {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    if SEEN.lock().unwrap_or_else(PoisonError::into_inner).insert(format!("{plugin_id}.{method}")) {
        crate::logs::warn(
            "Plugin called an API method this server does not implement",
            &[("plugin_id", plugin_id.into()), ("method", method.into())],
        );
    }
}

/// A session with every permission, for the calls a plugin makes as the server.
fn system_session() -> Session {
    Session { roles: "system_admin system_user".into(), ..Default::default() }
}

fn json<T: serde::Serialize>(v: &T) -> Out {
    Out::J(serde_json::to_value(v).unwrap_or_default())
}

fn from<T: serde::de::DeserializeOwned>(j: J, what: &str) -> ApiResult<T> {
    serde_json::from_value(j).map_err(|e| AppError::invalid_param(what).detail(e.to_string()))
}

/// A `*Post` argument; nil file ids stay `null`.
fn post_arg(a: &Args<'_>, i: usize) -> ApiResult<crate::model::Post> {
    let j = a.json(i);
    let nil_files = j.get("file_ids").is_none_or(J::is_null);
    let mut p: crate::model::Post = from(j, "post")?;
    if nil_files && p.file_ids.0.is_empty() {
        p.file_ids.1 = true;
    }
    Ok(p)
}

/// A post as a plugin gets it: without metadata.
fn for_plugin(p: &crate::model::Post) -> Out {
    let mut p = p.clone();
    p.metadata = None;
    json(&p)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn dispatch(c: &Ctx, m: &str, a: &Args<'_>) -> ApiResult<Vec<Out>> {
    let app = &c.app;
    Ok(match m {
        // ---------- logs ----------
        "LogDebug" | "LogInfo" | "LogWarn" | "LogError" => {
            let mut fields: Vec<(String, J)> = vec![("plugin_id".into(), c.id.clone().into())];
            if let J::Array(kv) = a.json(1) {
                for pair in kv.chunks(2) {
                    let k = match pair.first() {
                        Some(J::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => continue,
                    };
                    fields.push((k, pair.get(1).cloned().unwrap_or(J::Null)));
                }
            }
            let f: Vec<(&str, J)> = fields.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            let level = match m {
                "LogError" => crate::logs::Level::Error,
                "LogWarn" => crate::logs::Level::Warn,
                "LogInfo" => crate::logs::Level::Info,
                _ => crate::logs::Level::Debug,
            };
            crate::logs::log(level, &a.str(0), &f);
            vec![]
        }

        // ---------- the server and its configuration ----------
        "GetServerVersion" => vec![J::from(crate::SERVER_VERSION).into()],
        "GetDiagnosticId" | "GetTelemetryId" => vec![J::from(app.cfg.diagnostic_id.clone()).into()],
        "GetSystemInstallDate" => {
            let v: Option<String> = sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = 'InstallationDate'")
                .fetch_optional(&app.db)
                .await?;
            vec![J::from(v.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0)).into()]
        }
        "GetLicense" => vec![crate::license::current().map(|l| json(&l)).unwrap_or(Out::J(J::Null))],
        "IsEnterpriseReady" => vec![J::Bool(false).into()],
        "GetBundlePath" => {
            let p = std::fs::canonicalize(&c.dir).unwrap_or_else(|_| c.dir.clone());
            vec![J::from(p.display().to_string()).into()]
        }
        "GetConfig" => vec![Out::J(app.cfg.sanitized())],
        "GetUnsanitizedConfig" => vec![Out::J(app.cfg.value().clone())],
        "GetPluginConfig" => {
            let cfg = app.cfg.sanitized();
            vec![Out::J(
                cfg.pointer(&format!("/PluginSettings/Plugins/{}", pointer_key(&c.id))).cloned().unwrap_or(json!({})),
            )]
        }
        "LoadPluginConfiguration" => {
            let mut out = Map::new();
            if let Some(settings) = c.manifest.pointer("/settings_schema/settings").and_then(J::as_array) {
                for s in settings {
                    if let Some(k) = s.get("key").and_then(J::as_str) {
                        out.insert(k.to_lowercase(), s.get("default").cloned().unwrap_or(J::Null));
                    }
                }
            }
            if let Some(J::Object(set)) = app.cfg.get("PluginSettings.Plugins").get(&c.id) {
                for (k, v) in set {
                    out.insert(k.to_lowercase(), v.clone());
                }
            }
            vec![Out::Raw(Value::Bytes(serde_json::to_vec(&J::Object(out)).unwrap_or_default()))]
        }
        "SavePluginConfig" => {
            let mut cfg = app.cfg.sanitized();
            let settings = match a.json(0) {
                J::Null => json!({}),
                j => j,
            };
            if let Some(plugins) = cfg.pointer_mut("/PluginSettings/Plugins").and_then(J::as_object_mut) {
                plugins.insert(c.id.clone(), settings);
            } else if let Some(ps) = cfg.pointer_mut("/PluginSettings").and_then(J::as_object_mut) {
                ps.insert("Plugins".into(), J::Object(Map::from_iter([(c.id.clone(), settings)])));
            }
            let (old, new) = app.cfg.save(cfg).await?;
            app.config_changed(old, new).await;
            vec![Out::J(J::Null)]
        }
        "GetPluginStatus" => {
            let id = a.str(0);
            let s = super::statuses(app).into_iter().find(|s| s["plugin_id"] == id.as_str());
            match s {
                Some(s) => vec![Out::J(s)],
                None => {
                    return Err(AppError::not_found("app.plugin.not_installed.app_error", "Plugin is not installed."));
                }
            }
        }
        "GetPlugins" => vec![Out::J(J::Array(super::available(app).into_iter().map(|(_, m)| m).collect()))],
        "PublishPluginClusterEvent" => vec![Out::J(J::Null)],

        // ---------- key-value store ----------
        "KVGet" => vec![bytes_out(kv::get(app, &c.id, &a.str(0)).await?)],
        "KVSet" => {
            kv::set(app, &c.id, &a.str(0), a.bytes(1), Default::default()).await?;
            vec![Out::J(J::Null)]
        }
        "KVSetWithExpiry" => {
            let o = kv::SetOptions { expire_in_seconds: a.i64(2), ..Default::default() };
            kv::set(app, &c.id, &a.str(0), a.bytes(1), o).await?;
            vec![Out::J(J::Null)]
        }
        "KVCompareAndSet" => {
            let o = kv::SetOptions { atomic: true, old_value: a.bytes(1), ..Default::default() };
            vec![J::Bool(kv::set(app, &c.id, &a.str(0), a.bytes(2), o).await?).into()]
        }
        "KVCompareAndDelete" => {
            vec![J::Bool(kv::compare_and_delete(app, &c.id, &a.str(0), a.bytes(1).as_deref()).await?).into()]
        }
        "KVSetWithOptions" => {
            let opts = a.raw(2);
            let field = |n: &str| opts.and_then(|o| o.get(n));
            let o = kv::SetOptions {
                atomic: field("Atomic").and_then(Value::as_bool).unwrap_or(false),
                old_value: field("OldValue").and_then(Value::as_bytes).map(<[u8]>::to_vec),
                expire_in_seconds: field("ExpireInSeconds").and_then(Value::as_i64).unwrap_or(0),
            };
            vec![J::Bool(kv::set(app, &c.id, &a.str(0), a.bytes(1), o).await?).into()]
        }
        "KVDelete" => {
            kv::delete(app, &c.id, &a.str(0)).await?;
            vec![Out::J(J::Null)]
        }
        "KVDeleteAll" => {
            kv::delete_all(app, &c.id).await?;
            vec![Out::J(J::Null)]
        }
        "KVList" => {
            let (page, per_page) = (a.i64(0), a.i64(1));
            vec![json(&kv::list(app, &c.id, page.saturating_mul(per_page), per_page).await?)]
        }

        // ---------- users ----------
        "GetUser" => vec![user_out(app.user(&a.str(0)).await?.as_deref())?],
        "GetUserByUsername" => {
            let u = crate::commands::user_by_username(app, &a.str(0)).await?;
            match u {
                Some(u) => vec![json(&*u)],
                None => {
                    return Err(AppError::not_found(
                        "app.user.get_by_username.app_error",
                        "Unable to find an existing account matching your username for this team. This team may \
                         require an invite from the team owner to join.",
                    ));
                }
            }
        }
        "GetUserByEmail" => {
            let u = crate::users::load_users(app, "u.email = ANY($1)", &[a.str(0).to_lowercase()]).await?;
            vec![user_out(u.first())?]
        }
        "GetUsersByIds" => vec![json(&crate::users::load_users(app, "u.id = ANY($1)", &a.strings(0)).await?)],
        "GetUsersByUsernames" => {
            let names: Vec<String> = a.strings(0).iter().map(|n| n.to_lowercase()).collect();
            vec![json(&crate::users::load_users(app, "u.username = ANY($1)", &names).await?)]
        }
        "HasPermissionTo" => vec![J::Bool(app.user_has_perm(&a.str(0), &perm_id(a, 1)).await).into()],
        "HasPermissionToTeam" => vec![J::Bool(app.user_team_perm(&a.str(0), &a.str(1), &perm_id(a, 2)).await).into()],
        "HasPermissionToChannel" => {
            vec![J::Bool(app.user_channel_perm(&a.str(0), &a.str(1), &perm_id(a, 2)).await.0).into()]
        }
        "GetUserStatus" => match app.status(&a.str(0)).await? {
            Some(s) => vec![json(&s)],
            None => return Err(AppError::not_found("app.status.get.missing.app_error", "Unable to find the status.")),
        },
        "GetUserStatusesByIds" => vec![json(&app.statuses(&a.strings(0)).await?)],
        "UpdateUserStatus" => {
            let (user_id, status) = (a.str(0), a.str(1));
            match status.as_str() {
                "online" => app.set_status_online(&user_id, true).await,
                "offline" => app.set_status_offline(&user_id, true, false).await,
                "away" => app.set_status_away_if_needed(&user_id, true).await,
                "dnd" => app.set_status_dnd(&user_id, None).await,
                _ => {
                    return Err(AppError::bad_request(
                        "plugin.api.update_user_status.bad_status",
                        "Unable to set the user status. Unknown user status.",
                    ));
                }
            }
            match app.status(&user_id).await? {
                Some(s) => vec![json(&s)],
                None => vec![Out::J(J::Null)],
            }
        }
        "SetProfileImage" => {
            app.set_profile_image_from_bytes(&a.str(0), &a.bytes(1).unwrap_or_default()).await?;
            vec![Out::J(J::Null)]
        }
        "GetPreferencesForUser" => vec![json(&app.preferences(&a.str(0)).await?)],
        "UpdatePreferencesForUser" => {
            app.update_preferences(&a.str(0), from(a.json(1), "preferences")?).await?;
            vec![Out::J(J::Null)]
        }
        "DeletePreferencesForUser" => {
            app.delete_preferences(&a.str(0), from(a.json(1), "preferences")?).await?;
            vec![Out::J(J::Null)]
        }
        "GetSession" => vec![json(&*app.session(&a.str(0)).await?)],

        // ---------- bots ----------
        "EnsureBotUser" => vec![J::from(ensure_bot(c, a.json(0)).await?).into()],
        "CreateBot" => vec![json(&app.create_bot(&system_session(), bot_from(c, &a.json(0))).await?)],
        "GetBot" => vec![json(&app.bot(&a.str(0), a.bool(1)).await?)],
        "PatchBot" => {
            let p = a.json(1);
            let s = |k: &str| p.get(k).and_then(J::as_str).map(str::to_string);
            let patch = crate::bots::BotPatch {
                username: s("username"),
                display_name: s("display_name"),
                description: s("description"),
            };
            vec![json(&app.patch_bot(&a.str(0), patch).await?)]
        }
        "UpdateBotActive" => vec![json(&app.update_bot_active(&a.str(0), a.bool(1)).await?)],

        // ---------- teams and channels ----------
        "GetTeam" => vec![json(&*app.team(&a.str(0)).await?)],
        "GetTeamByName" => vec![json(&app.team_by_name(&a.str(0)).await?)],
        "GetTeamsForUser" => vec![json(&app.teams_of_user(&a.str(0)).await?)],
        "GetTeamMember" => match app.team_member(&a.str(0), &a.str(1)).await? {
            Some(m) => vec![json(&m)],
            None => {
                return Err(AppError::not_found(
                    "app.team.get_member.missing.app_error",
                    "No team member found for that user ID and team ID.",
                ));
            }
        },
        "GetChannel" => vec![json(&*app.get_channel(&a.str(0)).await?)],
        "GetChannelByName" => vec![channel_out(app.channel_by_name(&a.str(0), &a.str(1), a.bool(2), true).await?)?],
        "GetChannelByNameForTeamName" => {
            let team = app.team_by_name(&a.str(0)).await?;
            vec![channel_out(app.channel_by_name(&team.id, &a.str(1), a.bool(2), true).await?)?]
        }
        "GetDirectChannel" => vec![json(&app.get_or_create_direct(&system_session(), &a.str(0), &a.str(1)).await?)],
        "GetGroupChannel" => vec![json(&app.create_group_channel(&a.strings(0), "").await?)],
        "GetChannelMember" => match app.channel_member(&a.str(0), &a.str(1)).await? {
            Some(m) => vec![json(&m)],
            None => {
                return Err(AppError::not_found(
                    "app.channel.get_member.missing.app_error",
                    "No channel member found for that user ID and channel ID.",
                ));
            }
        },
        "AddChannelMember" => {
            let ch = app.get_channel(&a.str(0)).await?;
            vec![json(&app.add_channel_member(&ch, &a.str(1), "", "").await?)]
        }
        "AddUserToChannel" => {
            let ch = app.get_channel(&a.str(0)).await?;
            vec![json(&app.add_channel_member(&ch, &a.str(1), &a.str(2), "").await?)]
        }
        "DeleteChannelMember" => {
            let ch = app.get_channel(&a.str(0)).await?;
            app.remove_user_from_channel(&a.str(1), "", &ch).await?;
            vec![Out::J(J::Null)]
        }

        // ---------- posts ----------
        "CreatePost" => {
            let post = post_arg(a, 0)?;
            let ch = app.get_channel(&post.channel_id).await?;
            let flags = crate::posts::CreateFlags {
                trigger_webhooks: true,
                set_online: true,
                from_plugin: true,
                silent: post.prop("silent_notification").is_some_and(|v| v.as_bool() == Some(true)),
                ..Default::default()
            };
            vec![for_plugin(&app.create_post_in(post, &ch, flags, None).await?)]
        }
        "GetPost" => vec![for_plugin(&app.single_post(&a.str(0), false).await?)],
        "UpdatePost" => {
            let post = post_arg(a, 0)?;
            vec![for_plugin(&app.update_post(&system_session(), post, false).await?)]
        }
        "DeletePost" => {
            app.delete_post(&a.str(0), &c.id).await?;
            vec![Out::J(J::Null)]
        }
        "SendEphemeralPost" => {
            let post = post_arg(a, 1)?;
            vec![for_plugin(&app.send_ephemeral(&a.str(0), post).await)]
        }
        "UpdateEphemeralPost" => {
            let mut post = post_arg(a, 1)?;
            post.update_at = now();
            app.update_ephemeral(&a.str(0), post.clone()).await;
            vec![for_plugin(&post)]
        }
        "DeleteEphemeralPost" => {
            let (user_id, post_id) = (a.str(0), a.str(1));
            let t = now();
            let post =
                json!({"id": post_id, "user_id": user_id, "type": "system_ephemeral", "delete_at": t, "update_at": t});
            let b = crate::hub::Broadcast { user_id, ..Default::default() };
            app.hub.publish("post_deleted", json!({"post": post.to_string()}), b);
            vec![]
        }
        "AddReaction" => vec![json(&app.save_reaction(from(a.json(0), "reaction")?).await?)],
        "RemoveReaction" => {
            app.delete_reaction(from(a.json(0), "reaction")?).await?;
            vec![Out::J(J::Null)]
        }
        "GetReactions" => vec![json(&*app.reactions_for_post(&a.str(0)).await?)],

        // ---------- files ----------
        "GetFileInfo" => match app.file_info(&a.str(0)).await? {
            Some(f) => vec![json(&f)],
            None => return Err(AppError::not_found("app.file_info.get.app_error", "Unable to get the file info.")),
        },
        "GetFile" => {
            let f = app
                .file_info(&a.str(0))
                .await?
                .ok_or_else(|| AppError::not_found("app.file_info.get.app_error", "Unable to get the file info."))?;
            let data = app
                .files
                .read(&f.path)
                .await
                .map_err(|e| AppError::internal("api.file.read_file.app_error", e.to_string()))?;
            vec![Out::Raw(Value::Bytes(data.to_vec()))]
        }
        "ReadFile" => {
            let data = app
                .files
                .read(&a.str(0))
                .await
                .map_err(|e| AppError::internal("api.file.read_file.app_error", e.to_string()))?;
            vec![Out::Raw(Value::Bytes(data.to_vec()))]
        }

        // ---------- slash commands and the webapp ----------
        "RegisterCommand" => {
            let j = a.json(0);
            let s = |k: &str| j.get(k).and_then(J::as_str).unwrap_or("").to_string();
            let cmd = crate::commands::Command {
                trigger: s("trigger"),
                team_id: s("team_id"),
                auto_complete: j.get("auto_complete").and_then(J::as_bool).unwrap_or(false),
                auto_complete_desc: s("auto_complete_desc"),
                auto_complete_hint: s("auto_complete_hint"),
                display_name: s("display_name"),
                autocomplete_data: j.get("autocomplete_data").cloned().filter(|d| !d.is_null()),
                autocomplete_icon_data: s("autocomplete_icon_data"),
                ..Default::default()
            };
            match super::PLUGINS.register_command(&c.id, cmd) {
                Ok(()) => vec![Out::J(J::Null)],
                Err(e) => return Err(AppError::bad_request("app.plugin.register_command.app_error", &e.to_string())),
            }
        }
        "UnregisterCommand" => {
            super::PLUGINS.unregister_command(&c.id, &a.str(0), &a.str(1));
            vec![Out::J(J::Null)]
        }
        "PublishWebSocketEvent" => {
            let b = a.json(2);
            let s = |k: &str| b.get(k).and_then(J::as_str).unwrap_or("").to_string();
            let omit = b
                .get("omit_users")
                .and_then(J::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false))).collect());
            let broadcast = crate::hub::Broadcast {
                omit_users: omit,
                user_id: s("user_id"),
                channel_id: s("channel_id"),
                team_id: s("team_id"),
                connection_id: s("connection_id"),
                omit_connection_id: s("omit_connection_id"),
                admin_only: b.get("contains_sensitive_data").and_then(J::as_bool).unwrap_or(false),
                ..Default::default()
            };
            let data = match a.json(1) {
                J::Null => json!({}),
                d => d,
            };
            app.hub.publish(&format!("custom_{}_{}", c.id, a.str(0)), data, broadcast);
            vec![]
        }
        "OpenInteractiveDialog" => {
            app.open_interactive_dialog(from(a.json(0), "dialog")?).await?;
            vec![Out::J(J::Null)]
        }

        _ => return Err(AppError::not_implemented(m)),
    })
}

/// A JSON pointer token: `~` and `/` escaped.
fn pointer_key(k: &str) -> String {
    k.replace('~', "~0").replace('/', "~1")
}

fn bytes_out(b: Option<Vec<u8>>) -> Out {
    b.map(|b| Out::Raw(Value::Bytes(b))).unwrap_or(Out::J(J::Null))
}

/// A `*Permission` argument's id.
fn perm_id(a: &Args<'_>, i: usize) -> String {
    a.json(i).get("id").and_then(J::as_str).unwrap_or("").to_string()
}

fn user_out(u: Option<&crate::model::User>) -> ApiResult<Out> {
    u.map(json).ok_or_else(crate::users::missing_account)
}

fn channel_out(c: Option<Arc<crate::model::Channel>>) -> ApiResult<Out> {
    match c {
        Some(c) => Ok(json(&*c)),
        None => Err(AppError::not_found("app.channel.get_by_name.missing.app_error", "Channel does not exist.")),
    }
}

/// The plugin's bot (its id kept in the plugin's `mmi_botid` key), patched to the given name and
/// description, else an existing bot of that username, else a new bot owned by the plugin.
async fn ensure_bot(c: &Ctx, bot: J) -> ApiResult<String> {
    const KEY: &str = "mmi_botid";
    let app = &c.app;
    let s = |k: &str| bot.get(k).and_then(J::as_str).unwrap_or("").to_string();
    let (username, display_name, description) = (s("username"), s("display_name"), s("description"));
    if username.is_empty() {
        return Err(AppError::bad_request("plugin.api.ensure_bot_user", "passed a bot with no username"));
    }
    if let Some(id) = kv::get(app, &c.id, KEY).await?.and_then(|v| String::from_utf8(v).ok())
        && app.bot(&id, true).await.is_ok()
    {
        let patch = crate::bots::BotPatch {
            username: Some(username),
            display_name: Some(display_name),
            description: Some(description),
        };
        app.patch_bot(&id, patch).await?;
        return Ok(id);
    }
    let save = |id: String| async move {
        kv::set(app, &c.id, KEY, Some(id.clone().into_bytes()), Default::default()).await?;
        Ok::<String, AppError>(id)
    };
    if let Some(u) = crate::commands::user_by_username(app, &username).await? {
        if !u.is_bot {
            return Err(AppError::bad_request(
                "plugin.api.ensure_bot_user",
                &format!("username {username:?} is already taken by a non-bot user"),
            ));
        }
        return save(u.id.clone()).await;
    }
    let created = app.create_bot(&system_session(), bot_from(c, &bot)).await?;
    save(created.user_id).await
}

/// A `*Bot` argument to create, owned by the plugin unless it names an owner.
fn bot_from(c: &Ctx, j: &J) -> crate::bots::Bot {
    let s = |k: &str| j.get(k).and_then(J::as_str).unwrap_or("").to_string();
    let owner = match s("owner_id") {
        o if o.is_empty() => c.id.clone(),
        o => o,
    };
    crate::bots::Bot {
        username: s("username"),
        display_name: s("display_name"),
        description: s("description"),
        owner_id: owner,
        ..Default::default()
    }
}
