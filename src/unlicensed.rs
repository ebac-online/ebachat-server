//! What a server without a license, plugins, LDAP, SAML, a cluster, cloud billing or AI answers on those
//! features' routes: Go's checks in Go's order (session, URL ids, permissions, body, settings), then the error
//! (or the empty answer) a Team Edition server gives. The routes and their steps are data
//! (`data/unlicensed_routes.txt`), one line per route:
//!
//! `METHOD PATH | steps | answer`, where the answer is `STATUS error.id [| message]`, `ok` or `json VALUE`, and
//! the steps, in order:
//! - `anon`: no session needed (otherwise a request without one is 401);
//! - `id:param[:name]`: the URL parameter must be an id (`invalid_url_param` for `name`);
//! - `perm:a|b`: one of these system permissions; `perm:sysconsole_read_*` any system console read permission;
//!   `rperm:a`: the permission, and not a restricted system admin;
//! - `on:Setting:STATUS:id` / `off:Setting:STATUS:id`: that error when the boolean setting is on (off);
//! - `obj:name`: the body must be a JSON object (`invalid_body_param` for `name`);
//! - `arr`: the body must be a JSON array of strings (`api.payload.parse.error`);
//! - `prop:key:name`: the body's `key` must be a non-empty string;
//! - `need:a|b:name`: one of the body's arrays `a`, `b` must be non-empty;
//! - `nonempty:key:STATUS:id`: that error when the body's array `key` is not empty;
//! - `cfgnil:Section:STATUS:id`: the body is a config (the server's when it is not one), whose section must have
//!   every setting;
//! - `qbool:name`: the query parameter must be `true` or `false`;
//! - `audit:extra`: an `Audits` row (`c.LogAudit(extra)`), `_` for a space.

// Based on Mattermost server/channels/api4 (access_control.go, cloud.go, compliance.go, content_flagging.go,
// custom_profile_attributes.go, data_retention.go, elasticsearch.go, group.go, ip_filtering.go, ldap.go,
// license.go, outgoing_oauth_connection.go, plugin.go, properties.go, recap.go, remote_cluster.go, saml.go,
// shared_channel.go, system.go, ...), Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::app::Query;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawPathParams, State as St};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use serde_json::Value;

use crate::app::{AppError, MaybeAuth, State};

static TABLE: &str = include_str!("../data/unlicensed_routes.txt");

enum Answer {
    Error(StatusCode, String, Option<String>),
    Json(Value),
}

struct Spec {
    path: &'static str,
    steps: Vec<Vec<&'static str>>,
    answer: Answer,
}

fn status(s: &str) -> StatusCode {
    s.parse::<u16>().ok().and_then(|c| StatusCode::from_u16(c).ok()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn parse(line: &'static str) -> (Method, Spec) {
    let mut parts = line.splitn(3, " | ");
    let head = parts.next().unwrap_or_default();
    let (method, path) = head.split_once(' ').unwrap_or_default();
    let steps = parts.next().unwrap_or_default().split_whitespace().map(|s| s.split(':').collect()).collect();
    let ans = parts.next().unwrap_or_default().trim();
    let answer = if ans == "ok" {
        Answer::Json(serde_json::json!({"status": "OK"}))
    } else if let Some(j) = ans.strip_prefix("json ") {
        Answer::Json(serde_json::from_str(j).expect("table json"))
    } else {
        let (code_id, msg) = match ans.split_once(" | ") {
            Some((a, m)) => (a, Some(m.to_string())),
            None => (ans, None),
        };
        let (code, id) = code_id.split_once(' ').unwrap_or((code_id, ""));
        Answer::Error(status(code), id.to_string(), msg)
    };
    (method.trim().parse().expect("table method"), Spec { path: path.trim(), steps, answer })
}

/// Go's error: its English text, or the message the table gives (for texts with parameters).
fn error(code: StatusCode, id: &str, msg: Option<&str>) -> AppError {
    let mut e = AppError::new(code, id, msg.unwrap_or(id));
    if let Some(m) = msg {
        e.message = crate::brand::apply(m).into_owned();
    }
    e
}

fn step_error(s: &[&str], from: usize) -> AppError {
    error(status(s.get(from).copied().unwrap_or("500")), s.get(from + 1).copied().unwrap_or(""), None)
}

/// `json.Unmarshal` of the body; `None` for an empty or bad one.
fn json(body: &Bytes) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

async fn run(
    app: State,
    spec: &'static Spec,
    session: Option<Arc<crate::model::Session>>,
    params: HashMap<String, String>,
    query: HashMap<String, String>,
    body: Bytes,
) -> Response {
    let anon = spec.steps.iter().any(|s| s[0] == "anon");
    if !anon && session.is_none() {
        return AppError::new(StatusCode::UNAUTHORIZED, "api.context.session_expired.app_error", "").into_response();
    }
    for s in &spec.steps {
        let arg = |i: usize| s.get(i).copied().unwrap_or("");
        let err = match s[0] {
            "id" => {
                let v = params.get(arg(1)).map(String::as_str).unwrap_or("");
                (!crate::app::is_valid_id(v))
                    .then(|| AppError::invalid_url_param(if s.len() > 2 { arg(2) } else { arg(1) }))
            }
            "perm" | "rperm" => {
                let ok = match &session {
                    None => false,
                    Some(se) => {
                        let perms = app.session_permissions(se).await;
                        let any = arg(1).split('|').any(|p| match p.strip_suffix('*') {
                            Some(prefix) => perms.iter().any(|x| x.starts_with(prefix)),
                            None => perms.contains(p),
                        });
                        any && !(s[0] == "rperm" && app.cfg.bool("ExperimentalSettings.RestrictSystemAdmin"))
                    }
                };
                (!ok).then(|| AppError::forbidden_perm(arg(1)))
            }
            "on" | "off" => (app.cfg.bool(arg(1)) == (s[0] == "on")).then(|| step_error(s, 2)),
            "obj" => match json(&body) {
                Some(Value::Object(_)) | Some(Value::Null) => None,
                _ => Some(AppError::invalid_param(arg(1))),
            },
            "arr" => match json(&body) {
                Some(Value::Array(a)) if a.iter().all(Value::is_string) => None,
                _ => Some(AppError::bad_request("api.payload.parse.error", "")),
            },
            "prop" => {
                let v = json(&body).unwrap_or(Value::Null);
                v.get(arg(1)).and_then(Value::as_str).is_none_or(str::is_empty).then(|| AppError::invalid_param(arg(2)))
            }
            "need" => {
                let v = json(&body).unwrap_or(Value::Null);
                let any = arg(1).split('|').any(|k| v.get(k).and_then(Value::as_array).is_some_and(|a| !a.is_empty()));
                (!any).then(|| AppError::invalid_param(arg(2)))
            }
            "nonempty" => {
                let v = json(&body).unwrap_or(Value::Null);
                v.get(arg(1)).and_then(Value::as_array).is_some_and(|a| !a.is_empty()).then(|| step_error(s, 2))
            }
            "cfgnil" => match json(&body) {
                // checkHasNilFields over the section the request sends.
                Some(cfg @ Value::Object(_)) => {
                    let schema = crate::config::defaults().get(arg(1));
                    let sec = cfg.get(arg(1));
                    let missing = match (schema.and_then(Value::as_object), sec.and_then(Value::as_object)) {
                        (Some(sm), Some(m)) => sm.keys().any(|k| m.get(k).is_none_or(Value::is_null)),
                        _ => true,
                    };
                    missing.then(|| step_error(s, 2))
                }
                _ => None,
            },
            "audit" => {
                crate::audit::log(&app, session.as_deref(), &arg(1).replace('_', " ")).await;
                None
            }
            "qbool" => {
                let v = query.get(arg(1)).map(String::as_str).unwrap_or("");
                (v != "true" && v != "false").then(|| AppError::invalid_param(arg(1)))
            }
            _ => None,
        };
        if let Some(e) = err {
            return e.into_response();
        }
    }
    match &spec.answer {
        Answer::Error(code, id, msg) => error(*code, id, msg.as_deref()).into_response(),
        Answer::Json(v) => ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response(),
    }
}

fn filter(m: &Method) -> MethodFilter {
    match *m {
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::PATCH => MethodFilter::PATCH,
        Method::DELETE => MethodFilter::DELETE,
        Method::HEAD => MethodFilter::HEAD,
        _ => MethodFilter::GET,
    }
}

pub fn routes() -> Router<State> {
    let mut by_path: BTreeMap<&'static str, MethodRouter<State>> = BTreeMap::new();
    for line in TABLE.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let (method, spec) = parse(line);
        let spec: &'static Spec = Box::leak(Box::new(spec));
        let h = move |St(app): St<State>,
                      MaybeAuth(s): MaybeAuth,
                      params: RawPathParams,
                      Query(q): Query<HashMap<String, String>>,
                      body: Bytes| async move {
            let p = params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            run(app, spec, s, p, q, body).await
        };
        let mr = by_path.remove(spec.path).unwrap_or_default();
        by_path.insert(spec.path, mr.on(filter(&method), h));
    }
    by_path.into_iter().fold(Router::new(), |r, (p, mr)| r.route(p, mr))
}
