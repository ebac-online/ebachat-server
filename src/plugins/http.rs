//! The plugins' HTTP side: requests to `/plugins/{id}/...` handed to the plugin's `ServeHTTP` (with the user of a
//! valid session in `Mattermost-User-Id`), its public files, the webapp bundles, the plugin management API, and
//! requests the server itself makes to a plugin (message actions, dialogs, autocomplete lists).

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{ConnectInfo, Path, Request, State as St};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use goplugin::gob::Value;
use goplugin::host::HttpResponse;
use serde_json::{Value as J, json};

use super::{PLUGINS, manifest_id};
use crate::app::{ApiResult, App, AppError, Auth, State};
use crate::model::Session;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/plugins/{plugin_id}/public/{*file}", any(public_file))
        .route("/plugins/{plugin_id}", any(plugin_request_route))
        .route("/plugins/{plugin_id}/", any(plugin_request_route))
        .route("/plugins/{plugin_id}/{*rest}", any(plugin_request_route))
        .route("/api/v4/plugins", get(list))
        .route("/api/v4/plugins/statuses", get(statuses))
        .route("/api/v4/plugins/webapp", get(webapp))
        .route("/api/v4/plugins/{plugin_id}/enable", post(enable))
        .route("/api/v4/plugins/{plugin_id}/disable", post(disable))
}

fn disabled() -> AppError {
    AppError::new(
        StatusCode::NOT_IMPLEMENTED,
        "app.plugin.disabled.app_error",
        "Plugins have been disabled. Please check your logs for details.",
    )
}

fn plugins_on(app: &App) -> ApiResult<()> {
    if app.cfg.bool("PluginSettings.Enable") { Ok(()) } else { Err(disabled()) }
}

/// A plain-text error, without the API's headers.
fn plain(status: StatusCode, text: &str) -> Response {
    let mut r = (status, format!("{text}\n")).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert("x-content-type-options", header::HeaderValue::from_static("nosniff"));
    r.extensions_mut().insert(crate::web::Raw);
    r
}

// ---------- requests to plugins ----------

/// A request to a plugin's routes.
async fn plugin_request_route(
    St(app): St<State>,
    Path(params): Path<Vec<(String, String)>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let id = params.iter().find(|(k, _)| k == "plugin_id").map(|(_, v)| v.clone()).unwrap_or_default();
    if !app.cfg.bool("PluginSettings.Enable") {
        let mut r = disabled().into_response();
        r.extensions_mut().insert(crate::web::Raw);
        return r;
    }
    let Some(plugin) = PLUGINS.get(&id).filter(|p| p.process.is_some()) else {
        return crate::web::router_not_found();
    };
    let (parts, body) = req.into_parts();
    let limit = usize::try_from(app.cfg.i64("FileSettings.MaxFileSize").max(1 << 20)).unwrap_or(usize::MAX);
    let Ok(body) = axum::body::to_bytes(body, limit).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "Request Entity Too Large");
    };
    let mut headers = go_headers(&parts.headers);
    let mut context = vec![
        ("RequestId", crate::app::new_id()),
        ("IPAddress", crate::audit::ip_address(&app, &parts.headers, Some(peer))),
        ("AcceptLanguage", header_str(&parts.headers, "accept-language")),
        ("UserAgent", header_str(&parts.headers, "user-agent")),
        ("ConnectionId", header_str(&parts.headers, "connection-id")),
    ];
    // The token: `Authorization: Bearer` or `Token`, else the session cookie, else `?access_token=`.
    let auth = header_str(&parts.headers, "authorization");
    let mut cookie_auth = false;
    let token = if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        auth[7..].to_string()
    } else if auth.len() >= 6 && auth[..6].eq_ignore_ascii_case("token ") {
        auth[6..].to_string()
    } else if let Some(c) = cookie(&parts.headers, "MMAUTHTOKEN") {
        cookie_auth = true;
        c
    } else {
        crate::gourl::parse_query(parts.uri.query().unwrap_or("")).0.get("access_token").to_string()
    };
    // Only the server sets these; the session cookie, the referrer and the query's token stay here.
    remove(&mut headers, "Mattermost-Plugin-Id");
    remove(&mut headers, "Mattermost-User-Id");
    remove(&mut headers, "Referer");
    remove(&mut headers, "Cookie");
    let others: Vec<String> = all_cookies(&parts.headers)
        .into_iter()
        .filter(|(n, _)| n != "MMAUTHTOKEN")
        .map(|(n, v)| format!("{n}={v}"))
        .collect();
    if !others.is_empty() {
        headers.push(("Cookie".into(), vec![others.join("; ")]));
    }
    let mut query = crate::gourl::parse_query(parts.uri.query().unwrap_or("")).0;
    query.0.retain(|(k, _)| k != "access_token");
    let path = plugin_path(parts.uri.path(), &id);
    let url = match query.encode() {
        q if q.is_empty() => path,
        q => format!("{path}?{q}"),
    };
    if !token.is_empty() {
        match app.session(&token).await {
            Err(e) if e.status_code == 500 => return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
            Err(_) => {}
            Ok(s) => {
                remove(&mut headers, "Authorization");
                if csrf_ok(&app, &parts.method, &parts.headers, &parts.uri, &body, &s, cookie_auth) {
                    headers.push(("Mattermost-User-Id".into(), vec![s.user_id.clone()]));
                    context.push(("SessionId", s.id.clone()));
                }
            }
        }
    }
    let request_uri = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
    let host = header_str(&parts.headers, "host");
    let r = serve(
        &plugin,
        parts.method.as_str(),
        &url,
        &request_uri,
        &host,
        &peer.to_string(),
        headers,
        &context,
        body.to_vec(),
    )
    .await;
    match r {
        Ok(r) => response(r),
        Err(e) => {
            crate::logs::error(
                "Plugin request failed",
                &[("plugin_id", id.into()), ("error", format!("{e:#}").into())],
            );
            plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

/// The request's path under the plugin (`/plugins/{id}` cut, raw as sent).
fn plugin_path(path: &str, id: &str) -> String {
    let prefix = format!("/plugins/{id}");
    match path.find(&prefix) {
        Some(i) => path.get(i + prefix.len()..).unwrap_or("").to_string(),
        None => path.to_string(),
    }
}

/// A cookie session on anything but GET needs the session's CSRF token in
/// `X-CSRF-Token` (or a `csrf` form field), or `X-Requested-With: XMLHttpRequest` without strict enforcement.
fn csrf_ok(
    app: &App,
    method: &Method,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    body: &[u8],
    s: &Session,
    cookie_auth: bool,
) -> bool {
    if !cookie_auth || method == Method::GET {
        return true;
    }
    let mut token = header_str(headers, "x-csrf-token");
    if token.is_empty() {
        let form = header_str(headers, "content-type").starts_with("application/x-www-form-urlencoded")
            && matches!(*method, Method::POST | Method::PUT | Method::PATCH);
        let from_body = if form {
            crate::gourl::parse_query(&String::from_utf8_lossy(body)).0.get("csrf").to_string()
        } else {
            String::new()
        };
        token = if from_body.is_empty() {
            crate::gourl::parse_query(uri.query().unwrap_or("")).0.get("csrf").to_string()
        } else {
            from_body
        };
    }
    if token == s.prop(crate::model::session::PROP_CSRF) {
        return true;
    }
    header_str(headers, "x-requested-with") == "XMLHttpRequest"
        && !app.cfg.bool("ServiceSettings.ExperimentalStrictCSRFEnforcement")
}

/// A request the server makes to a plugin for a session: `raw_url` is `/plugins/{id}/...`.
pub async fn plugin_request(
    app: &App,
    s: &Session,
    method: &str,
    raw_url: &str,
    body: Vec<u8>,
) -> ApiResult<HttpResponse> {
    let bad = || {
        AppError::bad_request("api.post.do_action.action_integration.app_error", "")
            .detail("err=Unable to find pluginId")
    };
    let rel = raw_url.trim_start_matches('/');
    let (path, query) = rel.split_once('?').unwrap_or((rel, ""));
    let mut segs = path.splitn(3, '/');
    let (Some("plugins"), Some(id)) = (segs.next(), segs.next()) else { return Err(bad()) };
    if !app.cfg.bool("PluginSettings.Enable") {
        return Ok(HttpResponse {
            status: 501,
            headers: vec![("Content-Type".into(), vec!["application/json".into()])],
            body: serde_json::to_vec(&disabled()).unwrap_or_default(),
        });
    }
    let Some(plugin) = PLUGINS.get(id).filter(|p| p.process.is_some()) else {
        return Ok(HttpResponse {
            status: 404,
            headers: vec![("Content-Type".into(), vec!["text/plain; charset=utf-8".into()])],
            body: b"404 page not found\n".to_vec(),
        });
    };
    let sub = path.get("plugins/".len() + id.len()..).unwrap_or("");
    let q = crate::gourl::parse_query(query).0.encode();
    let url = if q.is_empty() { sub.to_string() } else { format!("{sub}?{q}") };
    let headers = vec![("Mattermost-User-Id".into(), vec![s.user_id.clone()])];
    let context = vec![("RequestId", crate::app::new_id()), ("SessionId", s.id.clone())];
    serve(&plugin, method, &url, &url, "", "", headers, &context, body)
        .await
        .map_err(|e| AppError::internal("app.plugin.request.app_error", format!("{e:#}")))
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    plugin: &super::Active,
    method: &str,
    url: &str,
    request_uri: &str,
    host: &str,
    remote: &str,
    headers: Vec<(String, Vec<String>)>,
    context: &[(&str, String)],
    body: Vec<u8>,
) -> anyhow::Result<HttpResponse> {
    let p = plugin.process.as_ref().ok_or_else(|| anyhow::anyhow!("no server part"))?;
    let s = |v: &str| Value::String(v.to_string());
    let ctx = Value::strukt(context.iter().map(|(k, v)| (*k, s(v))).collect());
    let req = Value::strukt(vec![
        ("Method", s(method)),
        ("URL", Value::Marshaled(url.as_bytes().to_vec())),
        ("Proto", s("HTTP/1.1")),
        ("ProtoMajor", Value::Int(1)),
        ("ProtoMinor", Value::Int(1)),
        ("Header", goplugin::host::header_value(&headers)),
        ("Host", s(host)),
        ("RemoteAddr", s(remote)),
        ("RequestURI", s(request_uri)),
    ]);
    p.serve_http(ctx, req, body).await
}

/// What the plugin answered, written as it set it.
fn response(r: HttpResponse) -> Response {
    let mut out = Response::new(axum::body::Body::from(r.body));
    *out.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
    for (k, vs) in r.headers {
        let Ok(name) = header::HeaderName::from_bytes(k.as_bytes()) else { continue };
        for v in vs {
            if let Ok(v) = header::HeaderValue::from_str(&v) {
                out.headers_mut().append(name.clone(), v);
            }
        }
    }
    out.extensions_mut().insert(crate::web::Raw);
    out.extensions_mut().insert(crate::web::NoExpires);
    out
}

/// The request's headers for the plugin: canonical names, values in order (`Host` is not one).
fn go_headers(h: &HeaderMap) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = vec![];
    for (k, v) in h {
        if k == header::HOST {
            continue;
        }
        let name = canonical(k.as_str());
        let v = String::from_utf8_lossy(v.as_bytes()).into_owned();
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some((_, vs)) => vs.push(v),
            None => out.push((name, vec![v])),
        }
    }
    out
}

/// `x-csrf-token` → `X-Csrf-Token`.
fn canonical(k: &str) -> String {
    let mut up = true;
    k.chars()
        .map(|c| {
            let c = if up { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
            up = c == '-';
            c
        })
        .collect()
}

fn remove(h: &mut Vec<(String, Vec<String>)>, name: &str) {
    h.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
}

fn header_str(h: &HeaderMap, name: &str) -> String {
    h.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default()
}

fn all_cookies(h: &HeaderMap) -> Vec<(String, String)> {
    h.get_all(header::COOKIE)
        .iter()
        .flat_map(|v| String::from_utf8_lossy(v.as_bytes()).split(';').map(str::to_string).collect::<Vec<_>>())
        .filter_map(|c| c.trim().split_once('=').map(|(n, v)| (n.trim().to_string(), v.trim().to_string())))
        .filter(|(n, _)| !n.is_empty())
        .collect()
}

fn cookie(h: &HeaderMap, name: &str) -> Option<String> {
    all_cookies(h).into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

// ---------- files ----------

/// A file of an active plugin's `public` directory.
async fn public_file(Path((id, file)): Path<(String, String)>, req: Request) -> Response {
    if req.uri().path().ends_with('/') || file.split('/').any(|s| s == "..") {
        return crate::web::router_not_found();
    }
    let Some(p) = PLUGINS.get(&id) else { return crate::web::router_not_found() };
    let path = file.split('/').filter(|s| !s.is_empty() && *s != ".").fold(p.dir.join("public"), |d, s| d.join(s));
    let mut r = crate::web::serve_file(&path, req.headers(), req.method() == Method::HEAD, None).await;
    r.extensions_mut().insert(crate::web::Raw);
    r
}

/// A file of an active plugin's webapp under `/static/plugins/{id}/`: the bundle by its hashed name, the rest
/// from the bundle's directory. `None` for a plugin that is not active.
pub fn webapp_file(rel: &str) -> Option<(std::path::PathBuf, String)> {
    let (id, file) = rel.split_once('/')?;
    let p = PLUGINS.get(id)?;
    let (name, bundle) = p.bundle.as_ref()?;
    let dir = bundle.parent()?.to_path_buf();
    if file == name {
        return Some((dir, bundle.file_name()?.to_string_lossy().into_owned()));
    }
    Some((dir, file.to_string()))
}

// ---------- the management API ----------

/// Installed plugins, active or not, with their manifests.
async fn list(St(app): St<State>, Auth(s): Auth) -> ApiResult<axum::Json<J>> {
    plugins_on(&app)?;
    app.require(&s, crate::perm::SYSCONSOLE_READ_PLUGINS).await?;
    let (mut active, mut inactive) = (vec![], vec![]);
    for (_, m) in super::available(&app) {
        if PLUGINS.get(manifest_id(&m)).is_some() { active.push(m) } else { inactive.push(m) }
    }
    Ok(axum::Json(json!({"active": active, "inactive": inactive})))
}

async fn statuses(St(app): St<State>, Auth(s): Auth) -> ApiResult<axum::Json<J>> {
    plugins_on(&app)?;
    app.require(&s, crate::perm::SYSCONSOLE_READ_PLUGINS).await?;
    Ok(axum::Json(J::Array(super::statuses(&app))))
}

/// The client manifests of active plugins with a webapp.
async fn webapp(St(app): St<State>) -> ApiResult<axum::Json<J>> {
    plugins_on(&app)?;
    let list: Vec<J> = PLUGINS.all().iter().filter(|a| a.bundle.is_some()).map(|a| super::client_manifest(a)).collect();
    Ok(axum::Json(J::Array(list)))
}

async fn enable(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<axum::Json<J>> {
    set_enabled(&app, &s, &id, true).await
}

async fn disable(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<axum::Json<J>> {
    set_enabled(&app, &s, &id, false).await
}

/// The plugin's state saved in the configuration, which starts or stops it.
async fn set_enabled(app: &State, s: &Session, id: &str, on: bool) -> ApiResult<axum::Json<J>> {
    plugins_on(app)?;
    app.require(s, crate::perm::SYSCONSOLE_WRITE_PLUGINS).await?;
    if !super::available(app).iter().any(|(_, m)| manifest_id(m) == id) {
        return Err(AppError::not_found("app.plugin.not_installed.app_error", "Plugin is not installed."));
    }
    let mut cfg = app.cfg.value().clone();
    if let Some(ps) = cfg.pointer_mut("/PluginSettings").and_then(J::as_object_mut) {
        let states = ps.entry("PluginStates").or_insert_with(|| json!({}));
        if let Some(m) = states.as_object_mut() {
            m.insert(id.to_string(), json!({"Enable": on}));
        }
    }
    let (old, new) =
        app.cfg.save(cfg).await.map_err(|e| AppError::internal("app.plugin.config.app_error", e.detailed_error))?;
    app.config_changed(old, new).await;
    if !on {
        PLUGINS.remove_commands(id);
    }
    Ok(crate::users::ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_names_and_paths() {
        assert_eq!(canonical("mattermost-user-id"), "Mattermost-User-Id");
        assert_eq!(canonical("x-csrf-token"), "X-Csrf-Token");
        assert_eq!(plugin_path("/plugins/jira/api/v2/x", "jira"), "/api/v2/x");
        assert_eq!(plugin_path("/plugins/jira", "jira"), "");
    }
}
