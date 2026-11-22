//! The server config over the API: read (sanitized, filtered by the session's system console permissions),
//! replaced, patched, reloaded, and which settings the environment sets.

// Based on Mattermost server/channels/api4/config.go, app/platform/config.go (SaveConfig, ReloadConfig),
// app/platform/service.go (the config listener) and public/model/config.go (FilterConfig), Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashSet;

use crate::app::Query;
use axum::extract::State as St;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State};
use crate::config::{FAKE_SETTING, access_tags, conform, defaults, fill_defaults, merge_cfg, sanitize};
use crate::model::Session;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/config", get(get_config).put(update_config))
        .route("/api/v4/config/patch", put(patch_config))
        .route("/api/v4/config/reload", post(reload_config))
        .route("/api/v4/config/environment", get(environment))
}

const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate");

/// What a session may see and change of the config: `makeFilterConfigByPermission`.
pub struct Access {
    perms: HashSet<String>,
    restrict: bool,
}

impl App {
    /// Every permission the session's roles grant.
    pub async fn session_permissions(&self, s: &Session) -> HashSet<String> {
        let names: Vec<&str> = s.roles.split_whitespace().collect();
        let mut out = HashSet::new();
        if let Ok(rs) = self.roles(&names).await {
            for r in rs.iter().filter(|r| r.delete_at == 0) {
                out.extend(r.permissions.0.iter().cloned());
            }
        }
        out
    }

    pub async fn config_access(&self, s: &Session) -> Access {
        Access {
            perms: self.session_permissions(s).await,
            restrict: self.cfg.bool("ExperimentalSettings.RestrictSystemAdmin"),
        }
    }

    /// The config listener: when the settings changed, every client gets the new client config.
    pub async fn config_changed(&self, old: &Value, new: &Value) {
        crate::job_workers::config_changed(self, old, new).await;
        crate::logs::configure(new);
        if let Some(app) = self.hub.app() {
            crate::plugins::config_changed(app);
        }
        let strip = |v: &Value| {
            let mut v = v.clone();
            if let Some(m) = v.as_object_mut() {
                m.remove("FeatureFlags");
            }
            v
        };
        if strip(old) == strip(new) {
            return;
        }
        if let Ok(c) = self.client_config(false).await {
            self.hub.publish("config_changed", json!({"config": c}), Default::default());
        }
    }
}

impl Access {
    /// `SessionHasPermissionToAny(SysconsoleReadPermissions)` (or write).
    fn any_sysconsole(&self, write: bool) -> bool {
        let prefix = if write { "sysconsole_write_" } else { "sysconsole_read_" };
        self.perms.iter().any(|p| p.starts_with(prefix))
    }

    /// May the session read (write) a setting with this `access` tag?
    fn allowed(&self, tag: &str, write: bool) -> bool {
        let tags: Vec<&str> = tag.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
        let restrictable = |t: &&str| *t == "write_restrictable" || *t == "cloud_restrictable";
        if write && self.restrict && tags.iter().any(restrictable) {
            return false;
        }
        for t in tags.iter().filter(|t| !restrictable(t)) {
            if *t == "*_read" && !write && self.any_sysconsole(false) {
                return true;
            }
            let id = format!("sysconsole_{}_{t}", if write { "write" } else { "read" });
            if self.perms.contains(&id) {
                return true;
            }
        }
        self.perms.contains(crate::perm::MANAGE_SYSTEM)
    }

    /// `config.Merge(&model.Config{}, cfg, readFilter)`: what the session may not read is nil.
    pub fn read_filtered(&self, mut v: Value) -> Value {
        for (path, tag) in access_tags() {
            if !self.allowed(tag, false)
                && let Some(x) = v.pointer_mut(&format!("/{}", path.replace('.', "/")))
            {
                *x = Value::Null;
            }
        }
        v
    }
}

/// `SetPermissionError(perms...)`.
fn permission_error(perms: &str) -> AppError {
    AppError::forbidden_perm(perms)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GetQuery {
    remove_masked: String,
    remove_defaults: String,
}

/// `strconv.ParseBool`, false where it fails.
fn parse_bool(s: &str) -> bool {
    matches!(s, "1" | "t" | "T" | "TRUE" | "true" | "True")
}

/// `getConfig`.
async fn get_config(St(app): St<State>, Auth(s): Auth, Query(q): Query<GetQuery>) -> ApiResult<Response> {
    let access = app.config_access(&s).await;
    if !access.any_sysconsole(false) {
        return Err(permission_error("sysconsole_read_*"));
    }
    let mut m = access.read_filtered(app.cfg.sanitized());
    let (masked, defaults_too) = (parse_bool(&q.remove_masked), parse_bool(&q.remove_defaults));
    // FilterConfig
    if defaults_too {
        m = string_map_diff(&m, defaults());
    }
    if masked {
        remove_fake_settings(&mut m);
    }
    if masked || defaults_too {
        remove_empty(&mut m);
    }
    // FilterConfig answers a map: its keys sorted.
    Ok(([NO_CACHE], Json(m)).into_response())
}

/// `stringMapDiff`: what differs from the defaults.
fn string_map_diff(v: &Value, d: &Value) -> Value {
    let (Some(vm), Some(dm)) = (v.as_object(), d.as_object()) else { return v.clone() };
    let mut out = Map::new();
    for (k, x) in vm {
        match dm.get(k) {
            Some(y) if x == y => {}
            Some(y @ Value::Object(_)) if x.is_object() => {
                let r = string_map_diff(x, y);
                if r.as_object().is_some_and(|m| !m.is_empty()) {
                    out.insert(k.clone(), r);
                }
            }
            _ => {
                out.insert(k.clone(), x.clone());
            }
        }
    }
    Value::Object(out)
}

/// `removeFakeSettings`.
fn remove_fake_settings(v: &mut Value) {
    if let Some(m) = v.as_object_mut() {
        m.retain(|_, x| x.as_str() != Some(FAKE_SETTING));
        m.values_mut().for_each(remove_fake_settings);
    }
}

/// `removeEmptyMapsAndSlices`.
fn remove_empty(v: &mut Value) {
    if let Some(m) = v.as_object_mut() {
        m.values_mut().for_each(remove_empty);
        m.retain(|_, x| match x {
            Value::Null => false,
            Value::Object(o) => !o.is_empty(),
            Value::Array(a) => !a.is_empty(),
            _ => true,
        });
    }
}

/// The request's config, decoded as Go decodes `*model.Config`: `null` or a value of the wrong type is
/// `invalid_body_param` "config".
fn decode_config(body: &[u8]) -> ApiResult<Value> {
    let v: Value = serde_json::from_slice(body).map_err(|e| AppError::invalid_param("config").detail(e.to_string()))?;
    if v.is_null() {
        return Err(AppError::invalid_param("config"));
    }
    if !v.is_object() {
        return Err(
            AppError::invalid_param("config").detail("json: cannot unmarshal into Go value of type model.Config")
        );
    }
    conform(defaults(), &v, false).map_err(|path| {
        AppError::invalid_param("config").detail(format!("json: cannot unmarshal into Go struct field Config.{path}"))
    })
}

fn not_allowed(name: &str) -> AppError {
    let mut e = AppError::new(StatusCode::FORBIDDEN, "api.config.update_config.not_allowed_security.app_error", "");
    if let Some(m) = crate::i18n::tr(&e.id, &[("Name", name)]) {
        e.message = m;
    }
    e
}

fn clear_site_url() -> AppError {
    AppError::bad_request("api.config.update_config.clear_siteurl.app_error", "")
}

fn ptr<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path).unwrap_or(&Value::Null)
}

fn set(v: &mut Value, path: &str, x: Value) {
    if let Some(slot) = v.pointer_mut(path) {
        *slot = x;
    }
}

/// `HandleMessageExportConfig`: export turned on starts from now, unless a start is set.
fn message_export(cfg: &mut Value, app_cfg: &Value) {
    let on = ptr(cfg, "/MessageExportSettings/EnableExport").as_bool() == Some(true);
    let was = ptr(app_cfg, "/MessageExportSettings/EnableExport").as_bool() == Some(true);
    if on && !was && ptr(cfg, "/MessageExportSettings/ExportFromTimestamp").as_i64().unwrap_or(0) == 0 {
        set(cfg, "/MessageExportSettings/ExportFromTimestamp", Value::from(crate::app::now_ms()));
    } else if !on && was {
        set(cfg, "/MessageExportSettings/ExportFromTimestamp", Value::from(0));
    }
}

/// The saved config as the session may read it, and the listener told.
async fn saved(app: &App, access: &Access, old: &Value, new: &Value) -> Response {
    app.config_changed(old, new).await;
    ([NO_CACHE], go_config_json(&access.read_filtered(sanitize(new.clone())))).into_response()
}

/// A config answered as Go's `json.Encoder` encodes its struct: in the struct's field order, with a newline.
fn go_config_json(v: &Value) -> Response {
    let mut s = crate::config_order::to_go_json(v, false);
    s.push('\n');
    ([(axum::http::header::CONTENT_TYPE, "application/json")], s).into_response()
}

/// `updateConfig`: the whole config replaced (defaults where the request has none), as far as the session may
/// write it.
async fn update_config(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Response> {
    let mut cfg = decode_config(&body)?;
    fill_defaults(defaults(), &mut cfg);
    let access = app.config_access(&s).await;
    if !access.any_sysconsole(true) {
        return Err(permission_error("sysconsole_write_*"));
    }
    let app_cfg = app.cfg.value();
    if !app.cfg.str("ServiceSettings.SiteURL").is_empty() && ptr(&cfg, "/ServiceSettings/SiteURL").as_str() == Some("")
    {
        return Err(clear_site_url());
    }
    let mut new = app_cfg.clone();
    let tags = access_tags();
    merge_cfg(defaults(), &mut new, &cfg, &|p| access.allowed(tags.get(p).map_or("", String::as_str), true));
    // What the API may not change.
    for p in ["/PluginSettings/EnableUploads", "/PluginSettings/SignaturePublicKeyFiles", "/ImportSettings/Directory"] {
        set(&mut new, p, ptr(app_cfg, p).clone());
    }
    if !app.cfg.bool("PluginSettings.EnableUploads") {
        set(&mut new, "/PluginSettings/MarketplaceURL", ptr(app_cfg, "/PluginSettings/MarketplaceURL").clone());
    }
    // There is no Elasticsearch to have checked an autocomplete index.
    if !app.cfg.bool("ElasticsearchSettings.EnableAutocomplete")
        && ptr(&new, "/ElasticsearchSettings/EnableAutocomplete").as_bool() == Some(true)
    {
        return Err(AppError::bad_request("api.config.update.elasticsearch.autocomplete_cannot_be_enabled_error", ""));
    }
    message_export(&mut new, app_cfg);
    crate::config_valid::is_valid(&new)?;
    let (old, new) = app.cfg.save(new).await?;
    crate::audit::log(&app, Some(&s), "updateConfig").await;
    Ok(saved(&app, &access, old, new).await)
}

/// `patchConfig`: the settings the request has, as far as the session may write them.
async fn patch_config(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Response> {
    let mut cfg = decode_config(&body)?;
    let access = app.config_access(&s).await;
    if !access.any_sysconsole(true) {
        return Err(permission_error("sysconsole_write_*"));
    }
    let app_cfg = app.cfg.value();
    if !app.cfg.str("ServiceSettings.SiteURL").is_empty() && ptr(&cfg, "/ServiceSettings/SiteURL").as_str() == Some("")
    {
        return Err(clear_site_url());
    }
    let uploads = ptr(&cfg, "/PluginSettings/EnableUploads").clone();
    if !uploads.is_null() && uploads != *ptr(app_cfg, "/PluginSettings/EnableUploads") {
        return Err(not_allowed("PluginSettings.EnableUploads"));
    }
    if !ptr(&cfg, "/PluginSettings/SignaturePublicKeyFiles").is_null() {
        set(
            &mut cfg,
            "/PluginSettings/SignaturePublicKeyFiles",
            ptr(app_cfg, "/PluginSettings/SignaturePublicKeyFiles").clone(),
        );
    }
    let dir = ptr(&cfg, "/ImportSettings/Directory");
    if !dir.is_null() && dir != ptr(app_cfg, "/ImportSettings/Directory") {
        return Err(not_allowed("ImportSettings.Directory"));
    }
    let market = ptr(&cfg, "/PluginSettings/MarketplaceURL");
    if !market.is_null()
        && !uploads.is_null()
        && market != ptr(app_cfg, "/PluginSettings/MarketplaceURL")
        && uploads == false
    {
        return Err(not_allowed("PluginSettings.MarketplaceURL"));
    }
    if !ptr(&cfg, "/MessageExportSettings/EnableExport").is_null() {
        message_export(&mut cfg, app_cfg);
    }
    // The settings of plugins the patch does not name stay.
    if let (Some(Value::Object(p)), Some(Value::Object(stored))) =
        (cfg.pointer_mut("/PluginSettings/Plugins"), app_cfg.pointer("/PluginSettings/Plugins"))
    {
        for (id, v) in stored {
            p.entry(id.clone()).or_insert_with(|| v.clone());
        }
    }
    let mut new = app_cfg.clone();
    let tags = access_tags();
    merge_cfg(defaults(), &mut new, &cfg, &|p| access.allowed(tags.get(p).map_or("", String::as_str), true));
    crate::config_valid::is_valid(&new)?;
    let (old, new) = app.cfg.save(new).await?;
    Ok(saved(&app, &access, old, new).await)
}

/// `configReload`.
async fn reload_config(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    if !app.has_perm(&s, "reload_config").await || app.cfg.bool("ExperimentalSettings.RestrictSystemAdmin") {
        return Err(permission_error("reload_config"));
    }
    let (old, new) =
        app.cfg.reload().await.map_err(|e| AppError::internal("api.config.reload_config.app_error", e.to_string()))?;
    app.config_changed(old, new).await;
    Ok(([NO_CACHE], crate::users::ok()).into_response())
}

/// `getEnvironmentConfig`: the settings the environment sets that the session may read; `null` for none.
async fn environment(St(app): St<State>, Auth(s): Auth) -> Response {
    let access = app.config_access(&s).await;
    let mut env = app.cfg.environment();
    /// Leaves the session may not read go, and sections left empty.
    fn keep(v: &mut Value, path: &str, access: &Access) {
        if let Some(m) = v.as_object_mut() {
            for (k, x) in m.iter_mut() {
                keep(x, &format!("{path}{k}."), access);
            }
            m.retain(|k, x| match x {
                Value::Object(o) => !o.is_empty(),
                _ => access.allowed(access_tags().get(&format!("{path}{k}")).map_or("", String::as_str), false),
            });
        }
    }
    keep(&mut env, "", &access);
    let body = if env.as_object().is_some_and(|m| m.is_empty()) { Value::Null } else { env };
    ([NO_CACHE, (header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}
