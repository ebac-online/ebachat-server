//! The server log as Go writes it (`LogSettings`): a console target on standard error and, with `EnableFile`,
//! `mattermost.log` in `FileLocation` (by default the `logs` directory found as Go finds it), JSON lines or plain
//! text. `GET /logs`, `POST /logs/query` and `GET /logs/download` read that file back, as Go does.

// Based on Mattermost server/config/logger.go, channels/app/platform/log.go, channels/app/admin.go (GetLogs,
// QueryLogs), channels/api4/system.go (getLogs, queryLogs, downloadLogs, postLog), and the JSON and plain formats
// of github.com/mattermost/logr/v2 (MIT), Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

/// The file Go names its log.
pub const LOG_FILENAME: &str = "mattermost.log";

/// mlog's standard levels, by id.
const LEVELS: &[&str] = &["panic", "fatal", "error", "warn", "info", "debug", "trace", "stdlog"];

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Level {
    Error = 2,
    Warn = 3,
    Info = 4,
    Debug = 5,
}

impl Level {
    fn name(self) -> &'static str {
        LEVELS[self as usize]
    }
}

struct Target {
    max: usize,
    json: bool,
}

struct Logger {
    console: Option<Target>,
    file: Option<(Target, PathBuf, Option<std::fs::File>)>,
    max_field: usize,
}

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);

/// `stringToStdLevel`: the highest level id a target takes.
fn level_id(s: &str) -> Option<usize> {
    LEVELS.iter().position(|l| l.eq_ignore_ascii_case(s))
}

/// `fileutils.FindDir`: `dir` in the working directory and its ancestors, then next to the program; `./` if
/// nowhere.
pub fn find_dir(dir: &str) -> PathBuf {
    let mut bases: Vec<PathBuf> = vec![".".into(), "..".into(), "../..".into(), "../../..".into()];
    if let Ok(exe) = std::env::current_exe()
        && let Some(d) = exe.parent()
    {
        bases.extend([d.to_path_buf(), d.join(".."), d.join("../.."), d.join("../../..")]);
    }
    for b in bases {
        let p = b.join(dir);
        if p.is_dir() {
            return std::path::absolute(&p).unwrap_or(p);
        }
    }
    PathBuf::from("./")
}

/// `config.GetLogFileLocation`.
pub fn file_location(location: &str) -> PathBuf {
    let dir = if location.is_empty() { find_dir("logs") } else { PathBuf::from(location) };
    dir.join(LOG_FILENAME)
}

/// `config.GetLogRootPath`: `MM_LOG_PATH`, or the `logs` directory.
pub fn root_path() -> PathBuf {
    if let Ok(p) = std::env::var("MM_LOG_PATH")
        && !p.is_empty()
    {
        return std::path::absolute(&p).unwrap_or(PathBuf::from(p));
    }
    let d = find_dir("logs");
    std::path::absolute(&d).unwrap_or(d)
}

/// Symlinks resolved as far as the path exists (`evalSymlinksLenient`).
fn resolve_lenient(p: &Path) -> PathBuf {
    let mut missing = vec![];
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(r) = cur.canonicalize() {
            let mut r = r;
            for m in missing.iter().rev() {
                r.push(m);
            }
            return r;
        }
        let (Some(parent), Some(name)) = (cur.parent(), cur.file_name()) else { return p.to_path_buf() };
        missing.push(name.to_os_string());
        cur = parent.to_path_buf();
    }
}

/// `config.ValidateLogFilePath`: the file must be inside the logging root.
pub fn validate_path(file: &Path, root: &Path) -> Result<(), String> {
    let abs = resolve_lenient(&std::path::absolute(file).unwrap_or(file.to_path_buf()));
    let root = resolve_lenient(&std::path::absolute(root).unwrap_or(root.to_path_buf()));
    if abs == root || abs.starts_with(&root) {
        Ok(())
    } else {
        Err(format!("path {} is outside logging root {}", file.display(), root.display()))
    }
}

/// `initLogging`: the targets of `LogSettings`. Called at start and on every config change.
pub fn configure(cfg: &Value) {
    let ls = &cfg["LogSettings"];
    let b = |k: &str| ls[k].as_bool().unwrap_or(false);
    let s = |k: &str| ls[k].as_str().unwrap_or("");
    let console = b("EnableConsole")
        .then(|| level_id(s("ConsoleLevel")).map(|max| Target { max, json: b("ConsoleJson") }))
        .flatten();
    let file = b("EnableFile")
        .then(|| {
            level_id(s("FileLevel"))
                .map(|max| (Target { max, json: b("FileJson") }, file_location(s("FileLocation")), None))
        })
        .flatten();
    let max_field = ls["MaxFieldSize"].as_i64().unwrap_or(2048).max(0) as usize;
    *LOGGER.lock().unwrap() = Some(Logger { console, file, max_field });
}

/// The log file's path, when `EnableFile` is on.
pub fn file_path() -> Option<PathBuf> {
    LOGGER.lock().unwrap().as_ref()?.file.as_ref().map(|f| f.1.clone())
}

/// Go's default timestamp, `2006-01-02 15:04:05.000 Z07:00`, in local time.
fn timestamp() -> String {
    let now = chrono::Local::now();
    let off = now.offset().local_minus_utc();
    let zone = if off == 0 {
        "Z".to_string()
    } else {
        let a = off.abs();
        format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 3600, a % 3600 / 60)
    };
    format!("{} {zone}", now.format("%Y-%m-%d %H:%M:%S%.3f"))
}

fn truncate(v: &Value, max: usize) -> Value {
    match v {
        Value::String(s) if max > 0 && s.len() > max => {
            let mut cut = max;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            Value::String(s[..cut].to_string())
        }
        v => v.clone(),
    }
}

fn json_line(ts: &str, level: Level, msg: &str, caller: &str, fields: &[(&str, Value)], max: usize) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap();
    let mut out =
        format!("{{\"timestamp\":{},\"level\":{},\"msg\":{},\"caller\":{}", q(ts), q(level.name()), q(msg), q(caller));
    for (k, v) in fields {
        let mut k = k.to_string();
        while ["timestamp", "level", "msg", "stacktrace"].contains(&k.as_str()) {
            k = format!("_{k}");
        }
        out.push(',');
        out.push_str(&q(&k));
        out.push(':');
        out.push_str(&truncate(v, max).to_string());
    }
    out.push('}');
    out
}

fn plain_line(ts: &str, level: Level, msg: &str, caller: &str, fields: &[(&str, Value)], max: usize) -> String {
    let mut out = format!("{:<5} [{ts}] {msg:<45} caller=\"{caller}\"", level.name());
    for (k, v) in fields {
        let v = match truncate(v, max) {
            Value::String(s) if s.contains([' ', '=', '"']) || s.is_empty() => format!("{s:?}"),
            Value::String(s) => s,
            v => v.to_string(),
        };
        out.push_str(&format!(" {k}={v}"));
    }
    out
}

/// One record to every target that takes its level.
#[track_caller]
pub fn log(level: Level, msg: &str, fields: &[(&str, Value)]) {
    let loc = std::panic::Location::caller();
    let caller = format!("{}:{}", loc.file().trim_start_matches("src/"), loc.line());
    let mut guard = LOGGER.lock().unwrap();
    let Some(l) = guard.as_mut() else {
        if level <= Level::Warn {
            eprintln!("{} {msg} {fields:?}", level.name());
        }
        return;
    };
    let ts = timestamp();
    let max = l.max_field;
    if let Some(t) = &l.console
        && level as usize <= t.max
    {
        let line = if t.json {
            json_line(&ts, level, msg, &caller, fields, max)
        } else {
            plain_line(&ts, level, msg, &caller, fields, max)
        };
        eprintln!("{line}");
    }
    if let Some((t, path, file)) = &mut l.file
        && level as usize <= t.max
    {
        let line = if t.json {
            json_line(&ts, level, msg, &caller, fields, max)
        } else {
            plain_line(&ts, level, msg, &caller, fields, max)
        };
        if file.is_none() {
            if let Some(d) = path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            *file = std::fs::OpenOptions::new().create(true).append(true).open(&*path).ok();
        }
        if let Some(f) = file {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[track_caller]
pub fn error(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Error, msg, fields)
}
#[track_caller]
pub fn warn(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Warn, msg, fields)
}
#[track_caller]
pub fn info(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Info, msg, fields)
}
#[track_caller]
pub fn debug(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Debug, msg, fields)
}

/// What Go logs while it starts (`NewServer`, `Server.Start`) that an admin sees in `/logs`: a site URL that is not
/// one, the working directory and the config's source, the SMTP connection test (with email notifications on),
/// the file storage test, a push server over plain HTTP, then "Starting Server...".
/// What `platform.New` logs once logging is set up: the runtime, the version and build, the edition. This server
/// reports the API version it implements (12.1.0, as `X-Version-Id` and the client config do), the build hash of
/// that release, no build date or enterprise hash, and the Rust compiler where Go names its own.
pub fn initializing() {
    info("Server is initializing...", &[("go_version", env!("EBACHAT_RUSTC_VERSION").into())]);
    let (version, number, date, hash, hash_ee) =
        (crate::SERVER_VERSION, crate::SERVER_VERSION, "", crate::BUILD_HASH, "");
    info(
        &format!("Current version is {version} ({number}/{date}/{hash}/{hash_ee})"),
        &[
            ("current_version", version.into()),
            ("build_number", number.into()),
            ("build_date", date.into()),
            ("build_hash", hash.into()),
            ("build_hash_enterprise", hash_ee.into()),
            ("service_environment", crate::license::service_environment().into()),
        ],
    );
    info("Team Edition Build", &[("enterprise_build", false.into())]);
}

pub async fn startup(app: &crate::app::App) {
    if reqwest::Url::parse(app.cfg.str("ServiceSettings.SiteURL")).is_err()
        && std::env::var("IS_CI").as_deref() != Ok("true")
        && std::env::var("IS_LOCAL_TESTING").as_deref() != Ok("true")
    {
        error(
            "SiteURL must be set. Some features will operate incorrectly if the SiteURL is not set. See documentation for details: https://mattermost.com/pl/configure-site-url",
            &[],
        );
    }
    let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    info("Printing current working", &[("directory", pwd.into())]);
    info("Loaded config", &[("source", app.cfg.describe().into())]);
    if app.cfg.bool("EmailSettings.SendEmailNotifications")
        && let Err(e) = crate::mail::test_connection(&crate::email::smtp_config(app, "")).await
    {
        error("Mail server connection test failed", &[("error", e.into())]);
    }
    let test = app.files.write("testfile", bytes::Bytes::from_static(b"testingwrite")).await;
    match test {
        Ok(()) => {
            let _ = app.files.remove("testfile").await;
        }
        Err(e) => error("Problem with file storage settings", &[("error", e.to_string().into())]),
    }
    if app.cfg.str("EmailSettings.PushNotificationServer").starts_with("http://") {
        warn(
            "Your push notification server is configured with HTTP. For improved security, update to HTTPS in your configuration.",
            &[],
        );
    }
    info("Starting Server...", &[]);
}

/// The fields Go's handler logger carries for a request (`path`, `request_id`, `ip_addr`, `method`).
pub fn request_fields(request_id: &str) -> Vec<(&'static str, Value)> {
    let r = crate::audit::req();
    vec![
        ("path", r.path.into()),
        ("request_id", request_id.into()),
        ("ip_addr", r.ip.into()),
        ("user_id", r.user_id.into()),
        ("method", r.method.into()),
    ]
}

/// `model.LogFilter`.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct Filter {
    pub server_names: Option<Vec<String>>,
    pub log_levels: Option<Vec<String>>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
}

/// `LogFilterDateLayout` (`2006-01-02 15:04:05.999 -07:00`) and Go's default log timestamp: unix milliseconds.
/// Go parses both with the layout: the offset must be numeric, so a `Z` timestamp does not parse.
pub fn parse_date(s: &str) -> Option<i64> {
    let dt = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f %:z").ok()?;
    Some(dt.timestamp_millis())
}

impl Filter {
    /// `LogFilter.IsValid`.
    pub fn check(&self) -> Result<(), crate::app::AppError> {
        let layout = "2006-01-02 15:04:05.999 -07:00";
        for (v, id) in [
            (&self.date_from, "model.log_filter.is_valid.date_from.app_error"),
            (&self.date_to, "model.log_filter.is_valid.date_to.app_error"),
        ] {
            if let Some(d) = v.as_deref().filter(|d| !d.is_empty())
                && parse_date(d).is_none()
            {
                return Err(crate::app::AppError::bad_request(id, "").with_param("Layout", layout));
            }
        }
        Ok(())
    }

    /// `isLogFilteredByLevel` || `isLogFilteredByDate`.
    fn filtered(&self, entry: &Value) -> bool {
        let level = entry.get("Level").or_else(|| entry.get("level")).and_then(Value::as_str).unwrap_or("");
        let by_level = self.log_levels.as_ref().is_some_and(|l| !l.is_empty() && !l.iter().any(|x| x == level));
        let (from, to) = (self.date_from.as_deref().unwrap_or(""), self.date_to.as_deref().unwrap_or(""));
        let by_date = if from.is_empty() && to.is_empty() {
            false
        } else {
            let from = parse_date(from).unwrap_or(i64::MIN);
            let to = parse_date(to).unwrap_or_else(crate::app::now_ms);
            let ts = entry.get("Timestamp").or_else(|| entry.get("timestamp")).and_then(Value::as_str).unwrap_or("");
            match parse_date(ts) {
                None => false,
                Some(t) => !(t == from || t == to || (t > from && t < to)),
            }
        };
        by_level || by_date
    }
}

/// `GetLogsSkipSend`: a page of lines from the end of the file, oldest first; `[""]` without a log file.
pub fn read_lines(page: i64, per_page: i64, filter: &Filter) -> Result<Vec<String>, crate::app::AppError> {
    let Some(path) = file_path() else { return Ok(vec![String::new()]) };
    let read_err = |status: u16| {
        let mut e = crate::app::AppError::internal("api.admin.file_read_error", "");
        e.status_code = status;
        e
    };
    if let Err(e) = validate_path(&path, &root_path()) {
        error(
            "Blocked attempt to read log file outside allowed root",
            &[
                ("path", path.display().to_string().into()),
                ("config_section", "LogSettings.FileLocation".into()),
                ("error", e.into()),
            ],
        );
        return Err(read_err(403));
    }
    let mut f = std::fs::File::open(&path).map_err(|_| read_err(500))?;
    let mut data = vec![];
    f.seek(SeekFrom::Start(0)).map_err(|_| read_err(500))?;
    f.read_to_end(&mut data).map_err(|_| read_err(500))?;
    if data.ends_with(b"\n") {
        data.pop();
    }
    // Go walks back from the end byte by byte: each line it takes runs from its preceding newline (kept) to
    // the next line's newline.
    let mut lines: Vec<String> = vec![];
    let (mut count, mut end) = (0i64, data.len());
    if data.is_empty() {
        return Err(read_err(500));
    }
    let mut pos = data.len();
    loop {
        pos -= 1;
        if data[pos] == b'\n' || pos == 0 {
            count += 1;
            if count > page * per_page {
                let line = String::from_utf8_lossy(&data[pos..end]).into_owned();
                match serde_json::from_str::<Value>(line.trim_start_matches('\n')) {
                    Ok(v) if v.is_object() && filter.filtered(&v) => count -= 1,
                    _ => lines.push(line),
                }
            }
            if pos == 0 {
                break;
            }
            end = pos;
        }
        if lines.len() as i64 == per_page {
            break;
        }
    }
    lines.reverse();
    Ok(lines)
}

// ---------- routes ----------

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::State as St;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::app::{ApiResult, AppError, Auth, MaybeAuth, State};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/logs", get(get_logs).post(post_log))
        .route("/api/v4/logs/query", post(query_logs))
        .route("/api/v4/logs/download", get(download_logs))
}

type Q = Query<std::collections::HashMap<String, String>>;

/// `Params.Page` and `Params.LogsPerPage` (default and maximum 10000).
fn log_page(q: &std::collections::HashMap<String, String>) -> (i64, i64) {
    let page = q.get("page").and_then(|v| v.parse::<i64>().ok()).filter(|v| *v >= 0).unwrap_or(0);
    let per = match q.get("logs_per_page").and_then(|v| v.parse::<i64>().ok()) {
        Some(v) if v >= 0 => v.min(10000),
        _ => 10000,
    };
    (page, per)
}

fn json_raw(v: &Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

/// `getLogs`.
async fn get_logs(St(app): St<State>, Auth(s): Auth, Query(q): Q) -> ApiResult<Response> {
    app.require_unrestricted(&s, "get_logs").await?;
    let (page, per) = log_page(&q);
    let lines = read_lines(page, per, &Filter::default())?;
    Ok(json_raw(&serde_json::to_value(lines).unwrap()))
}

/// `queryLogs`: the lines of this node ("default") that pass the filter, each parsed as JSON.
async fn query_logs(St(app): St<State>, Auth(s): Auth, Query(q): Q, body: Bytes) -> ApiResult<Response> {
    app.require_unrestricted(&s, "get_logs").await?;
    let invalid = || AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "api.system.logs.invalidFilter", "");
    let v: Value = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok)
        .ok_or_else(invalid)?;
    if !v.is_object() {
        return Err(invalid());
    }
    let filter: Filter = serde_json::from_value(v).map_err(|_| invalid())?;
    filter.check()?;
    let (page, per) = log_page(&q);
    let names = filter.server_names.clone().unwrap_or_default();
    let mut out = serde_json::Map::new();
    if names.is_empty() || names.iter().any(|n| n == "default") {
        let lines = read_lines(page, per, &filter)?;
        let parsed: Vec<Value> = lines
            .iter()
            .filter_map(|l| match serde_json::from_str::<Value>(l) {
                Ok(v) => Some(v),
                Err(e) => {
                    warn(
                        "Error parsing log line in Server Logs",
                        &[("from_node", "default".into()), ("error", e.to_string().into())],
                    );
                    None
                }
            })
            .collect();
        if !parsed.is_empty() {
            out.insert("default".into(), Value::Array(parsed));
        }
    }
    Ok(json_raw(&Value::Object(out)))
}

/// `downloadLogs`: the whole log file as an attachment.
async fn download_logs(St(app): St<State>, Auth(s): Auth) -> ApiResult<Response> {
    app.require_unrestricted(&s, "get_logs").await?;
    let fail = |d: String| {
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "api.system.logs.download_bytes_buffer.app_error", "")
            .detail(d)
    };
    let Some(path) = file_path() else {
        return Err(fail("Unable to retrieve mattermost logs because LogSettings.EnableFile is set to false".into()));
    };
    if let Err(e) = validate_path(&path, &root_path()) {
        error(
            "Blocked attempt to read log file outside allowed root",
            &[
                ("path", path.display().to_string().into()),
                ("config_section", "LogSettings.FileLocation".into()),
                ("error", e.clone().into()),
            ],
        );
        return Err(fail(format!("log file path {} is outside allowed logging directory: {e}", path.display())));
    }
    let body = std::fs::read(&path)
        .map_err(|e| fail(format!("failed read mattermost log file at path {}: {e}", path.display())))?;
    let mut h = HeaderMap::new();
    crate::file_api::file_headers(&mut h, "text/plain", true, LOG_FILENAME);
    if !body.is_empty() && app.cfg.str("ServiceSettings.WebserverMode") == "gzip" {
        h.insert("X-Uncompressed-Content-Length", HeaderValue::from(body.len()));
    }
    h.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&httpdate::fmt_http_date(std::time::SystemTime::now())).unwrap(),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    Ok((StatusCode::OK, h, body).into_response())
}

/// `postLog`: a client's message in the server log, at error level for `ERROR` from an admin (or in developer
/// mode), at debug otherwise.
async fn post_log(
    St(app): St<State>,
    MaybeAuth(s): MaybeAuth,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let mut force_debug = false;
    if !app.cfg.bool("ServiceSettings.EnableDeveloper") {
        let Some(s) = &s else { return Err(AppError::forbidden()) };
        if !app.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
            force_debug = true;
        }
    }
    let mut m: serde_json::Map<String, Value> = match serde_json::Deserializer::from_slice(&body)
        .into_iter::<std::collections::BTreeMap<String, String>>()
        .next()
    {
        Some(Ok(m)) => m.into_iter().map(|(k, v)| (k, Value::String(v))).collect(),
        _ => {
            warn("Error decoding request.", &[]);
            serde_json::Map::new()
        }
    };
    let level = m.get("level").and_then(Value::as_str).unwrap_or("").to_string();
    let mut msg = m.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    if msg.len() > 400 {
        // Go cuts at byte 399, which can split a character; the cut is made at a character boundary here.
        let mut cut = 399;
        while !msg.is_char_boundary(cut) {
            cut -= 1;
        }
        msg.truncate(cut);
    }
    let msg = format!("Client Logs API Endpoint Message: {msg}");
    let (sid, uid) = s.as_ref().map_or((String::new(), String::new()), |s| (s.id.clone(), s.user_id.clone()));
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let fields: [(&str, Value); 4] = [
        ("type", "client_message".into()),
        ("user_agent", ua.into()),
        ("session_id", sid.into()),
        ("user_id", uid.into()),
    ];
    if !force_debug && level == "ERROR" {
        error(&msg, &fields);
    } else {
        debug(&msg, &fields);
    }
    m.insert("message".into(), Value::String(msg));
    Ok(Json(Value::Object(m)))
}
