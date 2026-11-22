//! Server plugins: the Mattermost plugin bundles in `PluginSettings.Directory`, each run as its own process that
//! talks to this server over go-plugin's net/rpc protocol (the `goplugin` crate). The plugins enabled in
//! `PluginSettings.PluginStates` are started, activated with the plugin API (`api.rs`), checked every 30 seconds
//! and restarted when they die (three times an hour at most), and stopped when disabled. Their webapp bundles,
//! public files, HTTP routes and slash commands are served from here (`http.rs`, `commands.rs`).

pub mod api;
pub mod http;
mod kv;
pub mod schema;
mod schema_gen;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use goplugin::host::{ApiHandler, Plugin};
use serde_json::{Value as J, json};

use crate::app::State;
use crate::commands::Command;

/// Plugins this server does not run, whatever the configuration says: the AI agents need their bridge in the
/// server, Calls its RTC service, Playbooks and Boards product APIs of their own.
const UNSUPPORTED: &[&str] = &["mattermost-ai", "com.mattermost.calls", "playbooks", "focalboard"];

/// Plugin states.
pub const NOT_RUNNING: i64 = 0;
pub const RUNNING: i64 = 2;
pub const FAILED_TO_START: i64 = 3;
pub const FAILED_TO_STAY_RUNNING: i64 = 4;

const HEALTH_INTERVAL: Duration = Duration::from_secs(30);
const RESTARTS_PER_HOUR: usize = 3;

pub static PLUGINS: LazyLock<Plugins> = LazyLock::new(Plugins::default);

#[derive(Default)]
pub struct Plugins {
    active: RwLock<HashMap<String, Arc<Active>>>,
    /// Plugins that failed: their state and error.
    failed: Mutex<HashMap<String, (i64, String)>>,
    /// When each plugin failed its health check, within the last hour.
    failures: Mutex<HashMap<String, Vec<Instant>>>,
    /// Slash commands the plugins registered (`plugin_id` set).
    commands: RwLock<Vec<Command>>,
    sync: tokio::sync::Mutex<()>,
}

/// An activated plugin.
pub struct Active {
    pub id: String,
    pub manifest: J,
    pub dir: PathBuf,
    /// The server part; none for a webapp-only plugin.
    pub process: Option<Plugin>,
    /// The webapp bundle: the name it is served as (`{id}_{hash}_bundle.js`) and its file.
    pub bundle: Option<(String, PathBuf)>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Plugins {
    pub fn get(&self, id: &str) -> Option<Arc<Active>> {
        self.active.read().unwrap_or_else(PoisonError::into_inner).get(id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Active>> {
        let mut v: Vec<_> = self.active.read().unwrap_or_else(PoisonError::into_inner).values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// The registered slash commands of active plugins for a team (or for every team).
    pub fn commands(&self, team_id: &str) -> Vec<Command> {
        let cmds = self.commands.read().unwrap_or_else(PoisonError::into_inner);
        cmds.iter().filter(|c| c.team_id.is_empty() || c.team_id == team_id).cloned().collect()
    }

    /// A plugin's slash command, its trigger lowercased: one per trigger and team for a plugin.
    pub fn register_command(&self, plugin_id: &str, mut c: Command) -> Result<()> {
        if c.trigger.is_empty() {
            bail!("invalid command");
        }
        c.autocomplete_data = Some(match c.autocomplete_data.take() {
            None | Some(J::Null) => json!({
                "Trigger": c.trigger, "Hint": c.auto_complete_hint, "HelpText": c.auto_complete_desc,
                "RoleID": "system_user", "Arguments": [], "SubCommands": [],
            }),
            Some(mut d) => {
                plugin_urls(&mut d, &format!("/plugins/{plugin_id}"), 0);
                d
            }
        });
        let c = Command {
            trigger: c.trigger.to_lowercase(),
            team_id: c.team_id,
            auto_complete: c.auto_complete,
            auto_complete_desc: c.auto_complete_desc,
            auto_complete_hint: c.auto_complete_hint,
            display_name: c.display_name,
            autocomplete_data: c.autocomplete_data,
            autocomplete_icon_data: c.autocomplete_icon_data,
            plugin_id: plugin_id.to_string(),
            ..Default::default()
        };
        let mut cmds = self.commands.write().unwrap_or_else(PoisonError::into_inner);
        match cmds.iter_mut().find(|x| x.trigger == c.trigger && x.team_id == c.team_id && x.plugin_id == plugin_id) {
            Some(x) => *x = c,
            None => cmds.push(c),
        }
        Ok(())
    }

    pub fn unregister_command(&self, plugin_id: &str, team_id: &str, trigger: &str) {
        let trigger = trigger.to_lowercase();
        self.commands
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|c| !(c.plugin_id == plugin_id && c.team_id == team_id && c.trigger == trigger));
    }

    fn remove_commands(&self, plugin_id: &str) {
        self.commands.write().unwrap_or_else(PoisonError::into_inner).retain(|c| c.plugin_id != plugin_id);
    }
}

/// A dynamic list's relative `FetchURL` made a path under the plugin's routes.
fn plugin_urls(d: &mut J, base: &str, depth: usize) {
    if depth > 32 {
        return;
    }
    if let Some(args) = d.get_mut("Arguments").and_then(J::as_array_mut) {
        for a in args {
            if a.get("Type").and_then(J::as_str) != Some("DynamicList") {
                continue;
            }
            if let Some(J::String(u)) = a.get_mut("Data").and_then(|d| d.get_mut("FetchURL"))
                && !u.contains("://")
            {
                *u = crate::gourl::clean_path(&format!("{base}/{u}"));
            }
        }
    }
    if let Some(subs) = d.get_mut("SubCommands").and_then(J::as_array_mut) {
        for s in subs {
            plugin_urls(s, base, depth + 1);
        }
    }
}

/// Starts the enabled plugins and checks them from then on.
pub fn start(app: State) {
    tokio::spawn(async move {
        sync(&app).await;
        loop {
            tokio::time::sleep(HEALTH_INTERVAL).await;
            health(&app).await;
        }
    });
}

/// The configuration changed: plugins enabled or disabled since are started or stopped, and the active ones get
/// `OnConfigurationChange`.
pub fn config_changed(app: State) {
    tokio::spawn(async move {
        sync(&app).await;
        for a in PLUGINS.all() {
            if let Some(p) = &a.process
                && let Err(e) = p.configuration_changed().await
            {
                crate::logs::error(
                    "Plugin OnConfigurationChange hook failed",
                    &[("plugin_id", a.id.clone().into()), ("error", format!("{e:#}").into())],
                );
            }
        }
    });
}

/// The plugin bundles in `PluginSettings.Directory`: (directory, manifest).
pub fn available(app: &State) -> Vec<(PathBuf, J)> {
    manifests_in(app.cfg.str("PluginSettings.Directory"))
}

/// The plugin bundles in a plugins directory (`./plugins` when empty).
pub fn manifests_in(dir: &str) -> Vec<(PathBuf, J)> {
    let dir = if dir.is_empty() { "./plugins" } else { dir };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path.join("plugin.json")) else { continue };
        match serde_json::from_slice::<J>(&bytes) {
            Ok(m) if m.get("id").and_then(J::as_str).is_some_and(|id| !id.is_empty()) => out.push((path, m)),
            _ => crate::logs::warn("Invalid plugin manifest", &[("path", path.display().to_string().into())]),
        }
    }
    out.sort_by(|a, b| manifest_id(&a.1).cmp(manifest_id(&b.1)));
    out
}

/// The installed plugins of a plugins directory and the keys of their secret settings.
pub fn secret_settings(dir: &str) -> HashMap<String, Vec<String>> {
    manifests_in(dir)
        .into_iter()
        .map(|(_, m)| {
            let secret = m
                .pointer("/settings_schema/settings")
                .and_then(J::as_array)
                .map(|list| {
                    list.iter()
                        .filter(|s| s.get("secret").and_then(J::as_bool) == Some(true))
                        .filter_map(|s| s.get("key").and_then(J::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (manifest_id(&m).to_string(), secret)
        })
        .collect()
}

pub fn manifest_id(m: &J) -> &str {
    m.get("id").and_then(J::as_str).unwrap_or("")
}

fn enabled(app: &State, id: &str) -> bool {
    app.cfg.bool("PluginSettings.Enable")
        && !UNSUPPORTED.contains(&id)
        && app.cfg.get("PluginSettings.PluginStates").get(id).and_then(|s| s.get("Enable")).and_then(J::as_bool)
            == Some(true)
}

/// Starts the enabled plugins and stops the others.
pub async fn sync(app: &State) {
    let _one = PLUGINS.sync.lock().await;
    let available = available(app);
    for a in PLUGINS.all() {
        let still = available.iter().any(|(_, m)| manifest_id(m) == a.id);
        if !still || !enabled(app, &a.id) {
            deactivate(app, &a.id).await;
            lock(&PLUGINS.failed).remove(&a.id);
        }
    }
    let mut changed = false;
    for (dir, m) in &available {
        let id = manifest_id(m);
        if !enabled(app, id) {
            lock(&PLUGINS.failed).remove(id);
            continue;
        }
        if PLUGINS.get(id).is_some() {
            continue;
        }
        changed = true;
        match activate(app, dir, m).await {
            Ok(a) => {
                lock(&PLUGINS.failed).remove(id);
                if a.bundle.is_some() {
                    app.hub.publish("plugin_enabled", json!({"manifest": client_manifest(&a)}), Default::default());
                }
            }
            Err(e) => {
                crate::logs::error(
                    "Unable to activate plugin",
                    &[("plugin_id", id.into()), ("error", format!("{e:#}").into())],
                );
                lock(&PLUGINS.failed).insert(id.to_string(), (FAILED_TO_START, format!("{e:#}")));
            }
        }
    }
    if changed {
        statuses_changed(app);
    }
}

async fn activate(app: &State, dir: &Path, manifest: &J) -> Result<Arc<Active>> {
    let id = manifest_id(manifest).to_string();
    let bundle = match manifest.pointer("/webapp/bundle_path").and_then(J::as_str) {
        Some(p) if !p.is_empty() => {
            let file = inside(dir, p)?;
            let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            Some((format!("{id}_{:016x}_bundle.js", fnv64a(&bytes)), file))
        }
        _ => None,
    };
    let process = match executable(manifest) {
        Some(exe) => {
            let exe = inside(dir, &exe)?;
            let pid = id.clone();
            let log: goplugin::host::LogLine = Arc::new(move |l: &str| plugin_log(&pid, l));
            let p = Plugin::start(&exe, dir, log).await?;
            let handler: ApiHandler = {
                let ctx = Arc::new(api::Ctx {
                    app: app.clone(),
                    id: id.clone(),
                    dir: dir.to_path_buf(),
                    manifest: manifest.clone(),
                });
                Arc::new(move |m, a| api::call(ctx.clone(), m, a))
            };
            if let Err(e) = p.activate(handler).await {
                p.stop().await;
                PLUGINS.remove_commands(&id);
                return Err(e);
            }
            Some(p)
        }
        None if bundle.is_some() => None,
        None => bail!("plugin has neither a server nor a webapp part"),
    };
    let a = Arc::new(Active { id: id.clone(), manifest: manifest.clone(), dir: dir.to_path_buf(), process, bundle });
    PLUGINS.active.write().unwrap_or_else(PoisonError::into_inner).insert(id.clone(), a.clone());
    crate::logs::info("Activated plugin", &[("plugin_id", id.into())]);
    Ok(a)
}

/// Stops an active plugin; true when it was active.
async fn deactivate(app: &State, id: &str) -> bool {
    let Some(a) = PLUGINS.active.write().unwrap_or_else(PoisonError::into_inner).remove(id) else { return false };
    PLUGINS.remove_commands(id);
    if let Some(p) = &a.process {
        p.stop().await;
    }
    if a.bundle.is_some() {
        app.hub.publish("plugin_disabled", json!({"manifest": client_manifest(&a)}), Default::default());
    }
    statuses_changed(app);
    true
}

/// The health check: a plugin whose process is gone or does not answer is restarted, and deactivated for good
/// (`FailedToStayRunning`) once it failed three times within an hour.
async fn health(app: &State) {
    for a in PLUGINS.all() {
        let Some(p) = &a.process else { continue };
        if !p.exited().await && p.ping().await.is_ok() {
            continue;
        }
        let failures = {
            let mut f = lock(&PLUGINS.failures);
            let list = f.entry(a.id.clone()).or_default();
            list.retain(|t| t.elapsed() < Duration::from_secs(3600));
            list.push(Instant::now());
            list.len()
        };
        let _one = PLUGINS.sync.lock().await;
        deactivate(app, &a.id).await;
        if failures >= RESTARTS_PER_HOUR {
            crate::logs::error("Plugin failed to stay running", &[("plugin_id", a.id.clone().into())]);
            lock(&PLUGINS.failed)
                .insert(a.id.clone(), (FAILED_TO_STAY_RUNNING, "plugin failed to stay running".into()));
            continue;
        }
        crate::logs::warn("Plugin health check failed, restarting", &[("plugin_id", a.id.clone().into())]);
        if let Err(e) = activate(app, &a.dir, &a.manifest).await {
            crate::logs::error(
                "Unable to restart plugin",
                &[("plugin_id", a.id.clone().into()), ("error", format!("{e:#}").into())],
            );
            lock(&PLUGINS.failed).insert(a.id.clone(), (FAILED_TO_START, format!("{e:#}")));
        }
        statuses_changed(app);
    }
}

/// The plugin's `ExecuteCommand`: its response and its error. Fails when the call does (the plugin crashed).
pub async fn execute_command(
    p: &Plugin,
    s: &crate::model::Session,
    args: &crate::commands::CommandArgs,
    users: Vec<(String, String)>,
    channels: Vec<(String, String)>,
) -> Result<(Option<crate::commands::CommandResponse>, Option<crate::app::AppError>)> {
    use goplugin::gob::Value;
    let st = |v: &str| Value::String(v.to_string());
    let map = |m: Vec<(String, String)>| {
        Value::Map(m.into_iter().map(|(k, v)| (Value::String(k), Value::String(v))).collect())
    };
    let ctx = Value::strukt(vec![("SessionId", st(&s.id)), ("RequestId", st(&crate::app::new_id()))]);
    let a = Value::strukt(vec![
        ("UserId", st(&args.user_id)),
        ("ChannelId", st(&args.channel_id)),
        ("TeamId", st(&args.team_id)),
        ("RootId", st(&args.root_id)),
        ("TriggerId", st(&args.trigger_id)),
        ("Command", st(&args.command)),
        ("SiteURL", st(&args.site_url)),
        ("UserMentions", map(users)),
        ("ChannelMentions", map(channels)),
    ]);
    let (resp, err) = p.execute_command(ctx, a).await?;
    // A nil response is absent; an empty one is an empty struct.
    let resp = resp.map(|v| serde_json::from_value(schema::from_gob("CommandResponse", &v)).unwrap_or_default());
    let err = err.filter(|v| matches!(v, Value::Struct(_))).map(|v| {
        let j = schema::from_gob("AppError", &v);
        let s = |k: &str| j.get(k).and_then(J::as_str).unwrap_or("").to_string();
        let status = j.get("status_code").and_then(J::as_i64).filter(|c| (100..=999).contains(c)).unwrap_or(500);
        crate::app::AppError {
            id: s("id"),
            message: s("message"),
            detailed_error: s("detailed_error"),
            request_id: s("request_id"),
            status_code: u16::try_from(status).unwrap_or(500),
            set_cookie: None,
        }
    });
    Ok((resp, err))
}

/// The status of every installed plugin.
pub fn statuses(app: &State) -> Vec<J> {
    let failed = lock(&PLUGINS.failed).clone();
    available(app)
        .into_iter()
        .map(|(dir, m)| {
            let id = manifest_id(&m).to_string();
            let (state, error) = if PLUGINS.get(&id).is_some() {
                (RUNNING, String::new())
            } else {
                failed.get(&id).cloned().unwrap_or((NOT_RUNNING, String::new()))
            };
            let s = |k: &str| m.get(k).and_then(J::as_str).unwrap_or("").to_string();
            json!({
                "plugin_id": id, "cluster_id": "", "plugin_path": dir.display().to_string(), "state": state,
                "error": error, "name": s("name"), "description": s("description"), "version": s("version"),
            })
        })
        .collect()
}

/// The statuses, to system admins.
fn statuses_changed(app: &State) {
    let b = crate::hub::Broadcast { admin_only: true, ..Default::default() };
    app.hub.publish("plugin_statuses_changed", json!({"plugin_statuses": statuses(app)}), b);
}

/// What the webapp gets of an active plugin's manifest.
pub fn client_manifest(a: &Active) -> J {
    let mut m = a.manifest.clone();
    if let Some(o) = m.as_object_mut() {
        o.remove("description");
        o.remove("server");
        o.remove("settings_schema");
        if let (Some((name, _)), Some(J::Object(w))) = (&a.bundle, o.get_mut("webapp")) {
            w.insert("bundle_path".into(), J::String(format!("/static/{}/{name}", a.id)));
        }
    }
    m
}

/// The server executable for this OS and architecture.
fn executable(m: &J) -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    let server = m.get("server")?;
    server
        .pointer(&format!("/executables/{os}-{arch}"))
        .or_else(|| server.get("executable"))
        .and_then(J::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A path of the manifest inside the bundle directory, refusing one that leaves it.
fn inside(dir: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(anyhow!("{rel} is outside the plugin directory"));
    }
    Ok(dir.join(p))
}

/// FNV-1a, 64 bits: the bundle's hash in its file name.
fn fnv64a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, x| (h ^ *x as u64).wrapping_mul(0x100000001b3))
}

/// A line the plugin wrote: hclog's JSON (`@level`, `@message` and fields), or plain text at debug.
fn plugin_log(id: &str, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(J::Object(o)) = serde_json::from_str::<J>(line) else {
        crate::logs::debug(line, &[("plugin_id", id.into())]);
        return;
    };
    let msg = o.get("@message").and_then(J::as_str).unwrap_or("").to_string();
    let mut fields: Vec<(&str, J)> = vec![("plugin_id", id.into())];
    fields.extend(o.iter().filter(|(k, _)| !k.starts_with('@')).map(|(k, v)| (k.as_str(), v.clone())));
    let level = match o.get("@level").and_then(J::as_str).unwrap_or("") {
        "error" => crate::logs::Level::Error,
        "warn" => crate::logs::Level::Warn,
        "info" => crate::logs::Level::Info,
        _ => crate::logs::Level::Debug,
    };
    crate::logs::log(level, &msg, &fields);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_hash_is_fnv1a() {
        assert_eq!(fnv64a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv64a(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn plugin_command_urls() {
        let mut d = json!({"Trigger": "jira", "Arguments": [], "SubCommands": [{"Trigger": "assign", "Arguments": [
            {"Type": "DynamicList", "Data": {"FetchURL": "/autocomplete/issues"}},
            {"Type": "DynamicList", "Data": {"FetchURL": "https://x.org/a"}},
            {"Type": "TextInput", "Data": {"Hint": "h"}}]}]});
        plugin_urls(&mut d, "/plugins/jira", 0);
        let args = &d["SubCommands"][0]["Arguments"];
        assert_eq!(args[0]["Data"]["FetchURL"], "/plugins/jira/autocomplete/issues");
        assert_eq!(args[1]["Data"]["FetchURL"], "https://x.org/a");
    }

    #[test]
    fn paths_stay_inside() {
        assert!(inside(Path::new("/p/jira"), "server/dist/plugin-linux-amd64").is_ok());
        assert!(inside(Path::new("/p/jira"), "../x").is_err());
        assert!(inside(Path::new("/p/jira"), "/bin/sh").is_err());
    }
}
