//! The web client as Go's web server hands it out: `/static/*` from the client directory, `/static/plugins/*`
//! from the plugins' client directory, `/robots.txt`, and `root.html` for any other GET outside `/api/`.
//!
//! The client directory is `EBACHAT_CLIENT_DIR` (Go finds `client/` next to its working directory or binary).
//! Static files are compressed as Go's `gzhttp` wrapper does in the default `WebserverMode` "gzip", with the
//! compressed bytes kept in memory per file and modification time.
//!
//! An error of a handler Go registers outside `/api/` (`/files/{id}/public`, `/login/sso/saml`) is its web error
//! page (`utils.RenderWebAppError`): an HTML page, or a redirect for a 3xx, to the client's signed
//! `/error?message=...&s=...`.

// Based on Mattermost server/channels/web/static.go, web.go, saml.go, handlers.go (handleContextError) and
// server/channels/utils/api.go, subpath.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::extract::{Path, Request, State as AxState};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::app::{App, AppError, State};
use crate::compress::Enc;

pub fn client_dir() -> PathBuf {
    PathBuf::from(std::env::var("EBACHAT_CLIENT_DIR").unwrap_or_else(|_| "client".into()))
}

/// Go's `http.NotFound` behind `notFoundNoCacheResponseWriter`.
fn not_found() -> Response {
    let mut r = (StatusCode::NOT_FOUND, "404 page not found\n").into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, public"));
    r
}

/// `path.Clean("/" + p)` as `http.FileServer` does: `.` and `..` resolved, never above the root.
fn clean(p: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out
}

/// Go's built-in extension table first (`mime.builtinTypesLower`), then the wider guess.
fn content_type(name: &str) -> String {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let builtin = match ext.as_str() {
        "avif" => "image/avif",
        "css" => "text/css; charset=utf-8",
        "gif" => "image/gif",
        "htm" | "html" => "text/html; charset=utf-8",
        "jpeg" | "jpg" => "image/jpeg",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "webp" => "image/webp",
        "xml" => "text/xml; charset=utf-8",
        _ => "",
    };
    if !builtin.is_empty() {
        return builtin.into();
    }
    match mime_guess::from_ext(&ext).first() {
        Some(m) if m.type_() == "text" => format!("{m}; charset=utf-8"),
        Some(m) => m.to_string(),
        // Go sniffs the content; the client's extension-less and unknown files (source maps) are text.
        None => "text/plain; charset=utf-8".into(),
    }
}

/// A file's compressed bytes, with the modification time they were made from.
type Gz = (SystemTime, Arc<Vec<u8>>);

#[derive(Default)]
pub struct GzCache(Mutex<HashMap<(PathBuf, Enc), Gz>>);

impl GzCache {
    fn get(&self, path: &std::path::Path, enc: Enc, mtime: SystemTime, data: &[u8]) -> Arc<Vec<u8>> {
        let key = (path.to_path_buf(), enc);
        if let Some((t, gz)) = self.0.lock().unwrap().get(&key)
            && *t == mtime
        {
            return gz.clone();
        }
        let gz = Arc::new(crate::compress::encode(enc, data));
        self.0.lock().unwrap().insert(key, (mtime, gz.clone()));
        gz
    }
}

fn gz_cache() -> &'static GzCache {
    static C: std::sync::OnceLock<GzCache> = std::sync::OnceLock::new();
    C.get_or_init(GzCache::default)
}

/// `staticFilesHandler(http.StripPrefix(..., http.FileServer(http.Dir(dir))))` under `gzhttp.GzipHandler`: Go's
/// `http.ServeContent` (conditional requests, ranges), and a full 200 answer gzip-compressed when the client takes it.
async fn serve(dir: PathBuf, rel: &str, req_path: &str, headers: &HeaderMap, head: bool) -> Response {
    let gzip = gzip_mode();
    let mut r = serve_static(dir, rel, req_path, headers, head, gzip).await;
    if gzip {
        // gzhttp adds it before anything else, whatever the answer.
        r.headers_mut().append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    r.extensions_mut().insert(NoExpires);
    r
}

/// `ServiceSettings.WebserverMode` as read at start (`InitStatic`): "gzip" wraps the static, plugin and docs
/// handlers in gzhttp; "disabled" registers none of the web client's routes.
static WEBSERVER_MODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn gzip_mode() -> bool {
    WEBSERVER_MODE.get().is_none_or(|m| m == "gzip")
}

/// Whether the web client's routes are served (`WebserverMode` is not "disabled").
pub fn static_enabled() -> bool {
    WEBSERVER_MODE.get().is_none_or(|m| m != "disabled")
}

/// `docsStaticExts`: real asset and page suffixes, so `v10.5` is not taken for a file with an extension.
fn has_docs_static_ext(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    let ext = base.rfind('.').map(|i| base[i..].to_ascii_lowercase()).unwrap_or_default();
    matches!(
        ext.as_str(),
        ".html"
            | ".htm"
            | ".css"
            | ".js"
            | ".mjs"
            | ".map"
            | ".json"
            | ".png"
            | ".jpg"
            | ".jpeg"
            | ".gif"
            | ".svg"
            | ".webp"
            | ".ico"
            | ".avif"
            | ".woff"
            | ".woff2"
            | ".ttf"
            | ".eot"
            | ".otf"
            | ".mp4"
            | ".webm"
            | ".txt"
            | ".xml"
            | ".pdf"
    )
}

/// `NewDocsHandler`: the offline docs bundle in `<client>/_documentation`, any method, clean URLs mapped to
/// `<path>.html` or `<path>/index.html` (`docsFileSystem`); HTML pages not cached for a year.
async fn docs(req: Request) -> Response {
    let dir = client_dir().join("_documentation");
    let path = req.uri().path();
    let mut upath = path.strip_prefix("/_documentation").unwrap_or(path).to_string();
    if !upath.starts_with('/') {
        upath.insert(0, '/');
    }
    // `http.Dir.Open`: the name cleaned, under the root.
    let open = |name: &str| -> Option<PathBuf> {
        let file = clean(name).iter().fold(dir.clone(), |p, s| p.join(s));
        std::fs::metadata(&file).ok().map(|_| file)
    };
    let found = if upath.contains("..") {
        None
    } else {
        open(&upath)
            .filter(|f| f.is_file())
            .or_else(|| if has_docs_static_ext(&upath) { None } else { open(&format!("{upath}.html")) })
            .or_else(|| open(&format!("{upath}/index.html")))
    };
    let mut r = match found.filter(|f| f.is_file()) {
        None => not_found(),
        Some(file) => {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut r = serve_file(&file, req.headers(), req.method() == Method::HEAD, None).await;
            let cc = if r.status() == StatusCode::NOT_FOUND || name.to_lowercase().ends_with(".html") {
                "no-cache, public"
            } else {
                "max-age=31556926, public"
            };
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cc));
            r
        }
    };
    r.extensions_mut().insert(NoExpires);
    r
}

/// `/_documentation`: Go's `http.Redirect` (302) to the slashed path, from the main router itself.
async fn docs_redirect(req: Request) -> Response {
    let mut url = format!("{}/", req.uri().path());
    if let Some(q) = req.uri().query() {
        url = format!("{url}?{q}");
    }
    let mut r = Response::new(axum::body::Body::empty());
    *r.status_mut() = StatusCode::FOUND;
    let h = r.headers_mut();
    h.insert(header::LOCATION, HeaderValue::from_str(&url).unwrap_or(HeaderValue::from_static("/_documentation/")));
    if req.method() == Method::GET || req.method() == Method::HEAD {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    }
    if req.method() == Method::GET {
        let body = format!("<a href=\"{}\">Found</a>.\n\n", html_escape(&url));
        r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        *r.body_mut() = axum::body::Body::from(body);
    }
    r.extensions_mut().insert(Raw);
    r
}

async fn serve_static(
    dir: PathBuf,
    rel: &str,
    req_path: &str,
    headers: &HeaderMap,
    head: bool,
    gzip: bool,
) -> Response {
    if req_path.ends_with('/') {
        return not_found();
    }
    let segs = clean(rel);
    let file = segs.iter().fold(dir, |p, s| p.join(s));
    let Ok(meta) = tokio::fs::metadata(&file).await else { return not_found() };
    let last = segs.last().copied().unwrap_or("");
    if meta.is_dir() {
        // `http.FileServer` sends a directory without its slash to the slashed path.
        let loc = format!("{last}/");
        return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, loc)]).into_response();
    }
    if last == "index.html" {
        return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "./")]).into_response();
    }
    let cache_control =
        if last == "remote_entry.js" { "no-cache, max-age=31556926, public" } else { "max-age=31556926, public" };
    let ct = content_type(last);
    let mut r = serve_file(&file, headers, head, Some(&ct)).await;
    if r.status() == StatusCode::NOT_FOUND {
        return not_found();
    }
    // `serveError` drops the cache headers of a 416.
    if r.status() != StatusCode::RANGE_NOT_SATISFIABLE {
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    }
    if r.status() != StatusCode::OK || head || !gzip {
        return r;
    }
    // Go's file server answers every method as GET, so only HEAD stays plain.
    let enc = crate::compress::select(&Method::GET, headers);
    let len =
        r.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok());
    // `gzhttp.DefaultMinSize`: smaller bodies go out as they are.
    if !(enc != Enc::None
        && len.is_some_and(|l| l >= crate::compress::MIN_SIZE)
        && crate::compress::content_type_ok(&ct))
    {
        return r;
    }
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let (mut parts, body) = r.into_parts();
    let data = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let gz = gz_cache().get(&file, enc, mtime, &data);
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc.name()));
    parts.headers.remove(header::ACCEPT_RANGES);
    parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(gz.len()));
    Response::from_parts(parts, axum::body::Body::from(gz.as_ref().clone()))
}

/// `http.ServeFile`/`http.ServeContent` for one file: `Last-Modified`, the preconditions of RFC 7232 (no ETag),
/// and byte ranges (one range as a 206 with `Content-Range`, several as `multipart/byteranges`). `content_type`
/// is what the handler set before; without one, Go's type for the extension.
pub async fn serve_file(
    file: &std::path::Path,
    headers: &HeaderMap,
    head: bool,
    content_type: Option<&str>,
) -> Response {
    let Ok(meta) = tokio::fs::metadata(file).await else { return not_found() };
    if meta.is_dir() {
        return not_found();
    }
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let mut base = HeaderMap::new();
    let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let zero_time = secs(mtime) == 0;
    if !zero_time {
        base.insert(header::LAST_MODIFIED, HeaderValue::from_str(&httpdate::fmt_http_date(mtime)).unwrap());
    }
    let hdr = |n: header::HeaderName| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let date = |v: &str| httpdate::parse_http_date(v).ok().map(secs);
    // checkPreconditions: If-Match / If-Unmodified-Since, If-None-Match / If-Modified-Since, If-Range.
    let if_match = hdr(header::IF_MATCH);
    let precondition_failed = if !if_match.is_empty() {
        !etag_list_has_star(&if_match)
    } else {
        let ius = hdr(header::IF_UNMODIFIED_SINCE);
        !ius.is_empty() && !zero_time && date(&ius).is_some_and(|t| secs(mtime) > t)
    };
    if precondition_failed {
        return (StatusCode::PRECONDITION_FAILED, base).into_response();
    }
    let inm = hdr(header::IF_NONE_MATCH);
    let not_modified = if !inm.is_empty() {
        etag_list_has_star(&inm)
    } else {
        let ims = hdr(header::IF_MODIFIED_SINCE);
        !ims.is_empty() && !zero_time && date(&ims).is_some_and(|t| secs(mtime) <= t)
    };
    if not_modified {
        return (StatusCode::NOT_MODIFIED, base).into_response();
    }
    let mut range = hdr(header::RANGE);
    let if_range = hdr(header::IF_RANGE);
    if !range.is_empty() && !if_range.is_empty() {
        // No ETag is ever set, so an entity tag never matches; a date must be the modification time.
        let ok = !scan_etag(&if_range) && !zero_time && date(&if_range) == Some(secs(mtime));
        if !ok {
            range.clear();
        }
    }
    let ct = match content_type {
        Some(c) => c.to_string(),
        None => content_type_of(file),
    };
    let Ok(data) = tokio::fs::read(file).await else { return not_found() };
    let size = data.len() as u64;
    let ranges = match parse_range(&range, size) {
        Ok(r) => r,
        Err(true) if size == 0 => vec![],
        Err(no_overlap) => {
            let mut h = HeaderMap::new();
            if no_overlap {
                h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{size}")).unwrap());
            }
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
            h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
            let msg = if no_overlap { "invalid range: failed to overlap\n" } else { "invalid range\n" };
            return (StatusCode::RANGE_NOT_SATISFIABLE, h, msg).into_response();
        }
    };
    let ranges = if ranges.iter().map(|r| r.1).sum::<u64>() > size { vec![] } else { ranges };
    base.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let (status, body): (StatusCode, Vec<u8>) = match ranges.len() {
        0 => (StatusCode::OK, data),
        1 => {
            let (start, len) = ranges[0];
            let cr = format!("bytes {}-{}/{size}", start, start + len - 1);
            base.insert(header::CONTENT_RANGE, HeaderValue::from_str(&cr).unwrap());
            (StatusCode::PARTIAL_CONTENT, data[start as usize..(start + len) as usize].to_vec())
        }
        _ => {
            let boundary = multipart_boundary();
            let mut out = Vec::new();
            for (i, (start, len)) in ranges.iter().enumerate() {
                let sep = if i == 0 { "" } else { "\r\n" };
                let end = start + len - 1;
                let head = format!(
                    "{sep}--{boundary}\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Type: {ct}\r\n\r\n"
                );
                out.extend_from_slice(head.as_bytes());
                out.extend_from_slice(&data[*start as usize..(*start + *len) as usize]);
            }
            out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            let mp = format!("multipart/byteranges; boundary={boundary}");
            base.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mp).unwrap());
            (StatusCode::PARTIAL_CONTENT, out)
        }
    };
    if !base.contains_key(header::CONTENT_TYPE) {
        base.insert(header::CONTENT_TYPE, HeaderValue::from_str(&ct).unwrap_or(HeaderValue::from_static("text/plain")));
    }
    base.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    if head {
        return (status, base).into_response();
    }
    (status, base, body).into_response()
}

/// Go's type for a file's extension (`mime.TypeByExtension`), as `serveContent` picks it.
fn content_type_of(file: &std::path::Path) -> String {
    content_type(&file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

/// `checkIfMatch` / `checkIfNoneMatch` for a file without an ETag: only `*` matches.
fn etag_list_has_star(v: &str) -> bool {
    let mut s = v;
    loop {
        s = s.trim_matches([' ', '\t']);
        if s.is_empty() {
            return false;
        }
        if let Some(rest) = s.strip_prefix(',') {
            s = rest;
            continue;
        }
        if s.starts_with('*') {
            return true;
        }
        match scan_etag_rest(s) {
            Some(rest) => s = rest,
            None => return false,
        }
    }
}

/// `scanETag`: whether the value starts with a valid entity tag.
fn scan_etag(s: &str) -> bool {
    scan_etag_rest(s.trim_matches([' ', '\t'])).is_some()
}

fn scan_etag_rest(s: &str) -> Option<&str> {
    let start = if s.starts_with("W/") { 2 } else { 0 };
    let b = s.as_bytes();
    if b.len() < start + 2 || b[start] != b'"' {
        return None;
    }
    for (i, &c) in b.iter().enumerate().skip(start + 1) {
        match c {
            b'"' => return Some(&s[i + 1..]),
            0x21 | 0x23..=0x7e | 0x80..=0xff => {}
            _ => return None,
        }
    }
    None
}

/// `parseRange`: (start, length) pairs; `Err(true)` when no range overlaps the content, `Err(false)` when the
/// header is malformed.
fn parse_range(s: &str, size: u64) -> Result<Vec<(u64, u64)>, bool> {
    if s.is_empty() {
        return Ok(vec![]);
    }
    let Some(spec) = s.strip_prefix("bytes=") else { return Err(false) };
    let mut out = vec![];
    let mut no_overlap = false;
    for ra in spec.split(',') {
        let ra = ra.trim_matches([' ', '\t']);
        if ra.is_empty() {
            continue;
        }
        let Some((start, end)) = ra.split_once('-') else { return Err(false) };
        let (start, end) = (start.trim_matches([' ', '\t']), end.trim_matches([' ', '\t']));
        let int = |v: &str| v.parse::<i64>().ok();
        if start.is_empty() {
            if end.is_empty() || end.starts_with('-') {
                return Err(false);
            }
            let i = (int(end).filter(|i| *i >= 0).ok_or(false)? as u64).min(size);
            out.push((size - i, i));
        } else {
            let i = int(start).filter(|i| *i >= 0).ok_or(false)? as u64;
            if i >= size {
                no_overlap = true;
                continue;
            }
            let len = if end.is_empty() {
                size - i
            } else {
                let e = int(end).ok_or(false)?;
                if (i as i64) > e {
                    return Err(false);
                }
                (e as u64).min(size - 1) - i + 1
            };
            out.push((i, len));
        }
    }
    if no_overlap && out.is_empty() {
        return Err(true);
    }
    Ok(out)
}

/// `multipart.Writer`'s random boundary: 30 random bytes in hex.
fn multipart_boundary() -> String {
    let b: [u8; 30] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `/static/` itself: `staticFilesHandler`'s 404 for a path ending in a slash.
async fn static_dir(req: Request) -> Response {
    serve(client_dir(), "", req.uri().path(), req.headers(), req.method() == Method::HEAD).await
}

async fn static_files(Path(rel): Path<String>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    serve(client_dir(), &rel, req.uri().path(), req.headers(), head).await
}

async fn static_plugins(AxState(app): AxState<State>, Path(rel): Path<String>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    // An active plugin's webapp, served from its bundle.
    if let Some((dir, file)) = crate::plugins::http::webapp_file(&rel) {
        return serve(dir, &file, req.uri().path(), req.headers(), head).await;
    }
    let dir = match app.cfg.str("PluginSettings.ClientDirectory") {
        "" => "./client/plugins".to_string(),
        d => d.to_string(),
    };
    let head = req.method() == Method::HEAD;
    serve(PathBuf::from(dir), &rel, req.uri().path(), req.headers(), head).await
}

async fn robots(req: Request) -> Response {
    if req.uri().path().ends_with('/') {
        return not_found();
    }
    // Served by Go's main router directly: none of `web.Handler`'s headers.
    let mut r = "User-agent: *\nDisallow: /\n".into_response();
    r.extensions_mut().insert(Raw);
    r
}

/// Marks an answer the Go router writes itself, outside `web.Handler`: none of its headers are added.
#[derive(Clone, Copy)]
pub struct Raw;

/// Marks a static file answer: Go's static handler never adds `Expires: 0`, whatever the status.
#[derive(Clone, Copy)]
pub struct NoExpires;

/// The Go router's own 404 for a path outside `/api/` that no route takes with this method (only GET and HEAD
/// reach the client's page): `http.NotFound`.
pub fn router_not_found() -> Response {
    let mut r = (StatusCode::NOT_FOUND, "404 page not found\n").into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    r.extensions_mut().insert(Raw);
    r
}

/// `unsupportedBrowserScriptHandler`: the script of the unsupported-browser page, from the templates directory
/// (`EBACHAT_TEMPLATES_DIR`, default `templates/`) as `http.ServeFile` serves it, or the built-in copy.
async fn unsupported_browser_js(req: Request) -> Response {
    // Go's main router serves it directly: none of `web.Handler`'s headers.
    let mut r = unsupported_browser_js_file(req).await;
    r.extensions_mut().insert(Raw);
    r
}

async fn unsupported_browser_js_file(req: Request) -> Response {
    if req.uri().path().ends_with('/') {
        let mut r = (StatusCode::NOT_FOUND, "404 page not found\n").into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        return r;
    }
    let dir = PathBuf::from(std::env::var("EBACHAT_TEMPLATES_DIR").unwrap_or_else(|_| "templates".into()));
    let file = dir.join("unsupported_browser.js");
    let head = req.method() == Method::HEAD;
    if tokio::fs::metadata(&file).await.is_ok() {
        return serve_file(&file, req.headers(), head, None).await;
    }
    let body = crate::unsupported::BROWSER_SCRIPT;
    let mut r = if head { StatusCode::OK.into_response() } else { body.into_response() };
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    r
}

/// `root` for a request: an old browser or desktop app gets its page instead of the client.
pub async fn root_for(app: &State, headers: &HeaderMap, uri: &axum::http::Uri) -> Response {
    match crate::unsupported::page_for(app, headers, uri) {
        Some(r) => static_headers(app, r),
        None => root(app).await,
    }
}

/// `root`: the web client's page with the site's name and description, for every GET or HEAD outside `/api/`,
/// with the headers of Go's static handler (`NewStaticHandler`): no framing by other origins, and the script
/// policy root.html also states.
pub async fn root(app: &State) -> Response {
    static_headers(app, root_page(app).await)
}

fn static_headers(app: &App, mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    if let Ok(v) = HeaderValue::from_str(&content_security_policy(app)) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

/// The `Content-Security-Policy` of Go's static handler: the frame ancestors, the hash of the inline script that
/// sets the subpath (`GetStaticScriptHashes`), and the `DeveloperFlags` this build is not a "dev" build for.
fn content_security_policy(app: &App) -> String {
    use base64::Engine;
    use sha2::Digest;
    let sub = subpath(app.cfg.str("ServiceSettings.SiteURL"));
    let sha = if sub == "/" {
        String::new()
    } else {
        let script = format!("window.publicPath='{sub}/static/'");
        format!(
            " 'sha256-{}'",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(script.as_bytes()))
        )
    };
    let mut dev = String::new();
    for kv in app.cfg.str("ServiceSettings.DeveloperFlags").split(',').filter(|s| !s.is_empty()) {
        if let Some((k @ ("unsafe-eval" | "unsafe-inline"), "true")) = kv.split_once('=') {
            dev.push_str(&format!(" '{k}'"));
        }
    }
    format!("frame-ancestors 'self' {}; script-src 'self'{sha}{dev}", app.cfg.str("ServiceSettings.FrameAncestors"))
}

async fn root_page(app: &State) -> Response {
    let file = client_dir().join("root.html");
    let mut contents = match tokio::fs::read(&file).await {
        Ok(c) => c,
        Err(e) => {
            let mut r = (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
            return r;
        }
    };
    let esc = html_escape;
    let original = format!("<title>{}</title>", esc("Mattermost"));
    // Go falls back to "Mattermost"; this server names the site (`crate::brand`).
    let site_name = crate::brand::name();
    let description = app.cfg.str("TeamSettings.CustomDescriptionText");
    let mut modified = format!("<title>{}</title>", esc(site_name));
    if !description.is_empty() {
        modified.push_str(&format!("<meta property=\"og:description\" content=\"{}\" />", esc(description)));
    }
    if original != modified
        && let Ok(text) = String::from_utf8(contents.clone())
    {
        contents = text.replace(&original, &modified).into_bytes();
    }
    let mut r = contents.into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, max-age=31556926, public"));
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    r
}

/// Go's `html.EscapeString` (and `template.HTMLEscapeString`).
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('\'', "&#39;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&#34;")
}

// ---------- error pages ----------

/// `GetSubpathFromConfig`: the cleaned path of the site URL, `/` without one.
pub fn subpath(site_url: &str) -> String {
    let rest = site_url.split_once("://").map_or(site_url, |(_, r)| r);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        return "/".into();
    }
    format!("/{}", clean(path).join("/"))
}

/// `url.Values.Encode`: sorted by key, keys and values query-escaped.
fn encode_query(params: &[(&str, &str)]) -> String {
    let mut p = params.to_vec();
    p.sort_by(|a, b| a.0.cmp(b.0));
    let esc = crate::opengraph::query_escape;
    p.iter().map(|(k, v)| format!("{}={}", esc(k), esc(v))).collect::<Vec<_>>().join("&")
}

/// `template.JSEscapeString`.
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '=' => out.push_str("\\u003D"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `RenderWebError`: to `/error?<params>&s=<signature>`, the path and query signed with the server's
/// `AsymmetricSigningKey` (ECDSA P-256 over SHA-256, ASN.1, URL-safe base64). A redirect for a 3xx status,
/// otherwise an HTML page that goes there.
pub async fn render_web_error(app: &App, status: StatusCode, params: &[(&str, &str)]) -> Response {
    use base64::Engine;
    use p256::ecdsa::signature::Signer;
    let sub = subpath(app.cfg.str("ServiceSettings.SiteURL"));
    let error_path = if sub == "/" { "/error".to_string() } else { format!("{sub}/error") };
    let base = format!("{error_path}?{}", encode_query(params));
    let Ok(keys) = app.integration_keys().await else {
        // `http.Error(w, "", http.StatusInternalServerError)`.
        let mut r = (StatusCode::INTERNAL_SERVER_ERROR, "\n").into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        return r;
    };
    let sig: p256::ecdsa::Signature = keys.signing.sign(base.as_bytes());
    let destination = format!("{base}&s={}", base64::engine::general_purpose::URL_SAFE.encode(sig.to_der().as_bytes()));
    if status.is_redirection() {
        // `http.Redirect` after the handler set its JSON content type: no body.
        let mut r = status.into_response();
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Ok(v) = HeaderValue::from_str(&destination) {
            h.insert(header::LOCATION, v);
        }
        return r;
    }
    let href = html_escape(&destination);
    let body = format!(
        "<!DOCTYPE html><html><head></head>\n\
         <body onload=\"window.location = '{}'\">\n\
         <noscript><meta http-equiv=\"refresh\" content=\"0; url={href}\"></noscript>\n\
         <!-- web error message -->\n\
         <a href=\"{href}\" style=\"color: #c0c0c0;\">...</a>\n\
         </body></html>\n",
        html_escape(&js_escape(&destination)),
    );
    let mut r = (status, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    r
}

/// `handleContextError` for a handler outside `/api/`, `/hooks/` and the OAuth API paths: the JSON error for the
/// mobile app (`X-Mobile-App`), otherwise the web error page with the error's message (`RenderWebAppError`).
pub async fn web_error(app: &App, headers: &HeaderMap, mut e: AppError) -> Response {
    if app.cfg.bool("ServiceSettings.EnableHardenedMode") && e.status_code >= 500 {
        e.id.clear();
        e.message = "Internal Server Error".into();
        e.detailed_error.clear();
        e.status_code = 500;
    }
    if headers.get("x-mobile-app").is_some_and(|v| !v.is_empty()) {
        return e.into_response();
    }
    let status = StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut r = render_web_error(app, status, &[("message", &e.message)]).await;
    if let Ok(v) = e.request_id.parse() {
        r.headers_mut().insert("x-request-id", v);
    }
    r
}

/// `loginWithSaml` (GET) and `completeSaml` (POST): SAML is an Enterprise feature, never available here.
async fn saml(AxState(app): AxState<State>, headers: HeaderMap) -> Response {
    let e = AppError::new(
        StatusCode::FOUND,
        "api.user.saml.not_available.app_error",
        "SAML 2.0 is not configured or supported on this server.",
    );
    web_error(&app, &headers, e).await
}

/// The web routes; `InitStatic`'s only when `WebserverMode` (read once, here) is not "disabled".
pub fn routes(webserver_mode: &str) -> axum::Router<State> {
    use axum::routing::get;
    let _ = WEBSERVER_MODE.set(webserver_mode.to_string());
    let r = axum::Router::new().route("/login/sso/saml", get(saml).post(saml));
    if !static_enabled() {
        return r;
    }
    r
        // Go's file servers answer every method as GET.
        .route("/static/plugins/{*rel}", axum::routing::any(static_plugins))
        .route("/static/", axum::routing::any(static_dir))
        .route("/static/plugins/", axum::routing::any(static_dir))
        .route("/_documentation", axum::routing::any(docs_redirect))
        .route("/_documentation/", axum::routing::any(docs))
        .route("/_documentation/{*rel}", axum::routing::any(docs))
        .route("/static/{*rel}", axum::routing::any(static_files))
        .route("/robots.txt", axum::routing::any(robots))
        .route("/unsupported_browser.js", axum::routing::any(unsupported_browser_js))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_page_escaping() {
        assert_eq!(subpath("http://localhost:8065"), "/");
        assert_eq!(subpath("https://x.org/chat/"), "/chat");
        assert_eq!(subpath("https://x.org/a//b/../c?q=1"), "/a/c");
        assert_eq!(encode_query(&[("type", "a b"), ("message", "x&y=z.")]), "message=x%26y%3Dz.&type=a+b");
        let d = "/error?message=Public+links+have+been+disabled.&s=MEQ=";
        // `=` for `=`, `&` for `&`.
        let (eq, amp) = (format!("{}u003D", char::from(92)), format!("{}u0026", char::from(92)));
        let want = format!("/error?message{eq}Public+links+have+been+disabled.{amp}s{eq}MEQ{eq}");
        assert_eq!(html_escape(&js_escape(d)), want);
        assert_eq!(html_escape(d), "/error?message=Public+links+have+been+disabled.&amp;s=MEQ=");
    }
}
