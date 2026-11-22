//! REST API v4: the routes of every area, and what is answered as a server without plugins or a license.

use axum::Router;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::app::{AppError, State};

/// Go's answer for a path (or a method) the API does not have.
/// No route: Go's 404 under `/api/`, the web client's page for any other GET or HEAD (`InitStatic`).
async fn not_found(
    axum::extract::State(app): axum::extract::State<State>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    uri: Uri,
) -> axum::response::Response {
    let api = uri.path().starts_with("/api/");
    if !api && crate::web::static_enabled() && (method == axum::http::Method::GET || method == axum::http::Method::HEAD)
    {
        return crate::web::root_for(&app, &headers, &uri).await;
    }
    if !api {
        return crate::web::router_not_found();
    }
    route_not_found(uri.path()).into_response()
}

/// The router itself, for `method_mismatch` to route a request again.
static ROUTER: std::sync::OnceLock<Router> = std::sync::OnceLock::new();

#[derive(Clone)]
struct Rerouted;

/// A path whose static route does not take this method. Gorilla then goes on to the next routes, so a parameter
/// route at the same place takes it (`GET /users/search` is `getUser` with user id "search": 400
/// invalid_url_param); axum stops at the static one. Route again with one segment percent-encoded, so that it can
/// only match a parameter (which decodes it back), from the last segment to the first; else the 404.
async fn method_mismatch(
    state: axum::extract::State<State>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    uri: Uri,
    req: axum::extract::Request,
) -> axum::response::Response {
    use tower::ServiceExt;
    let path = uri.path().to_string();
    if req.extensions().get::<Rerouted>().is_none()
        && path.starts_with("/api/v4/")
        && let Some(router) = ROUTER.get()
    {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, 64 << 20).await.unwrap_or_default();
        let segs: Vec<&str> = path.split('/').collect();
        for i in (3..segs.len()).rev() {
            let Some(first) = segs[i].bytes().next() else { continue };
            let mut s = segs.clone();
            let enc = format!("%{first:02X}{}", &segs[i][1..]);
            s[i] = &enc;
            let p = s.join("/");
            let pq = match uri.query() {
                Some(q) => format!("{p}?{q}"),
                None => p,
            };
            let mut r = axum::http::Request::from_parts(parts.clone(), axum::body::Body::from(body.clone()));
            *r.uri_mut() = pq.parse().unwrap_or_else(|_| uri.clone());
            r.extensions_mut().insert(Rerouted);
            let res = router.clone().oneshot(r).await.into_response();
            if res.extensions().get::<Rerouted>().is_none() {
                return res;
            }
        }
        let mut res = not_found(state, method, headers, uri).await;
        res.extensions_mut().insert(Rerouted);
        return res;
    }
    let mut res = not_found(state, method, headers, uri).await;
    res.extensions_mut().insert(Rerouted);
    res
}

/// Go's router 404, also for a path whose parameter fails the route's pattern (`{timestamp:[0-9]+}`).
pub fn route_not_found(path: &str) -> AppError {
    let mut e = AppError::not_found("api.context.404.app_error", "Sorry, we could not find the page.").detail(format!(
        "There doesn't appear to be an api call for the url='{path}'.  Typo? are you missing a team_id or user_id as part of the url?"
    ));
    e.request_id.clear();
    e
}

/// Go registers each API route for its own methods: a `HEAD` falls through to the 404 handler, but for the file
/// routes (`/files/{id}`, `/thumbnail`, `/preview`) that take it. Axum answers `HEAD` on every `GET` route.
async fn head_as_go(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if req.method() == axum::http::Method::HEAD {
        let p = req.uri().path();
        let file = p.strip_prefix("/api/v4/files/").is_some_and(|rest| {
            matches!(rest.split('/').collect::<Vec<_>>().as_slice(), [_] | [_, "thumbnail"] | [_, "preview"])
        });
        if p.starts_with("/api/v4/") && !file {
            return route_not_found(p).into_response();
        }
    }
    next.run(req).await
}

const ID: &str = "[A-Za-z0-9]+";

static PARAM_PATTERNS: std::sync::LazyLock<std::collections::HashMap<&'static str, regex::Regex>> =
    std::sync::LazyLock::new(|| {
        [
            ("action_id", "[A-Za-z0-9_-]+"),
            ("app_id", ID),
            ("bookmark_id", ID),
            ("bot_user_id", ID),
            ("category", "[A-Za-z0-9_]+"),
            ("category_id", "[A-Za-z0-9_-]+"),
            ("channel_id", ID),
            ("channel_name", "[A-Za-z0-9_-]+"),
            ("command_id", ID),
            ("content_reviewer_id", ID),
            ("email", ".+"),
            ("emoji_id", ID),
            ("emoji_name", "[A-Za-z0-9_+-]+"),
            ("export_name", r".+\.zip"),
            ("field_id", ID),
            ("file_id", ID),
            ("fingerprint", "[a-f0-9]{32}"),
            ("group_id", ID),
            ("group_name", "[a-z][a-z0-9_]*"),
            ("hook_id", ID),
            ("import_name", r".+\.zip"),
            ("invite_id", ID),
            ("invoice_id", "[_A-Za-z0-9]+"),
            ("job_id", ID),
            ("job_type", "[A-Za-z0-9_-]+"),
            ("object_type", "[a-z]+"),
            ("other_user_id", ID),
            ("outgoing_oauth_connection_id", ID),
            ("plugin_id", "[A-Za-z0-9_.-]+"),
            ("policy_id", ID),
            ("post_id", ID),
            ("preference_name", "[A-Za-z0-9_]+"),
            ("recap_id", ID),
            ("remote_id", ID),
            ("report_id", ID),
            ("request_id", ID),
            ("restore_version_id", ID),
            ("role_id", ID),
            ("role_name", "[a-z0-9_]+"),
            ("scheduled_post_id", ID),
            ("scheduled_recap_id", ID),
            ("scheme_id", ID),
            ("syncable_id", ID),
            ("syncable_type", "teams|channels"),
            ("target_id", ID),
            ("team_id", ID),
            ("team_name", "[A-Za-z0-9_-]+"),
            ("thread_id", ID),
            ("timestamp", "[0-9]+"),
            ("token_id", ID),
            ("upload_id", ID),
            ("user_id", ID),
            ("username", "[A-Za-z0-9_.-]+"),
            ("view_id", ID),
        ]
        .into_iter()
        .filter_map(|(name, p)| regex::Regex::new(&format!("^(?:{p})$")).ok().map(|r| (name, r)))
        .collect()
    });

/// A path parameter its route's pattern does not take: no route.
async fn param_patterns(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::extract::FromRequestParts;
    use axum::extract::rejection::RawPathParamsRejection;
    let (mut parts, body) = req.into_parts();
    // LDAP routes take any parameter.
    if parts.uri.path().starts_with("/api/v4/") && !parts.uri.path().starts_with("/api/v4/ldap/") {
        match axum::extract::RawPathParams::from_request_parts(&mut parts, &()).await {
            Ok(params) => {
                if params.iter().any(|(k, v)| PARAM_PATTERNS.get(k).is_some_and(|r| !r.is_match(v))) {
                    return route_not_found(parts.uri.path()).into_response();
                }
            }
            Err(RawPathParamsRejection::InvalidUtf8InPathParam(_)) => {
                return route_not_found(parts.uri.path()).into_response();
            }
            Err(_) => {}
        }
    }
    next.run(axum::extract::Request::from_parts(parts, body)).await
}

/// `LOG_REQUESTS=1`: one line per request on stderr (method, path, status, ms), to watch a real client.
async fn log_request(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let (method, uri, t) = (req.method().clone(), req.uri().clone(), std::time::Instant::now());
    let r = next.run(req).await;
    eprintln!("{} {} {} {}ms", method, uri, r.status().as_u16(), t.elapsed().as_millis());
    r
}

/// At most this many API requests at once; the next one is answered 503 `api.context.server_busy.app_error` at
/// once. Go has no such bound: past what it can serve, every request waits, for a pool connection or a CPU, and
/// memory grows with the queue (5 GB at twice its capacity in the bench, docs/BENCH.md). The websocket (held for the
/// whole connection), `system/ping` and files outside `/api` do not count. `EBACHAT_MAX_REQUESTS`, by default eight
/// per database connection and at least 256.
fn request_limit(db_connections: u32) -> usize {
    std::env::var("EBACHAT_MAX_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| (db_connections as usize * 8).max(256))
}

async fn limit_requests(
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let p = req.uri().path();
    if !p.starts_with("/api/") || p == "/api/v4/websocket" || p == "/api/v4/system/ping" {
        return next.run(req).await;
    }
    let Ok(_permit) = permits.try_acquire_owned() else {
        return AppError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "api.context.server_busy.app_error", "")
            .into_response();
    };
    next.run(req).await
}

/// The headers `web.Handler` puts on every API answer.
async fn go_headers(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::http::HeaderValue;
    let get = req.method() == axum::http::Method::GET;
    let mut r = next.run(req).await;
    if r.extensions().get::<crate::web::Raw>().is_some() {
        return r;
    }
    // Static, plugin and docs files: only `setStaticSecurityHeaders`' headers, no request or version id.
    if r.extensions().get::<crate::web::NoExpires>().is_some() {
        let h = r.headers_mut();
        h.insert("permissions-policy", HeaderValue::from_static(""));
        h.entry("x-content-type-options").or_insert(HeaderValue::from_static("nosniff"));
        h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        return r;
    }
    let h = r.headers_mut();
    if !h.contains_key("x-request-id") {
        h.insert("x-request-id", HeaderValue::from_str(&crate::app::new_id()).unwrap());
    }
    h.insert("x-version-id", HeaderValue::from_static(concat!("12.1.0.12.1.0.", "0", ".false")));
    h.insert("permissions-policy", HeaderValue::from_static(""));
    h.entry("x-content-type-options").or_insert(HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    let static_file = r.extensions().get::<crate::web::NoExpires>().is_some();
    let h = r.headers_mut();
    if get && !static_file && !h.contains_key("expires") && !h.contains_key("cache-control") {
        h.insert("expires", HeaderValue::from_static("0"));
    }
    r
}

/// A path with `.`, `..` or empty segments: a permanent redirect to the cleaned one, query kept.
async fn clean_path(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let path = req.uri().path();
    let clean = cleaned_path(path);
    if clean != path {
        let location = match req.uri().query() {
            Some(q) => format!("{clean}?{q}"),
            None => clean,
        };
        return (axum::http::StatusCode::MOVED_PERMANENTLY, [(axum::http::header::LOCATION, location)]).into_response();
    }
    next.run(req).await
}

fn cleaned_path(p: &str) -> String {
    let mut out = format!("/{}", crate::files::clean(p));
    if p.ends_with('/') && out != "/" {
        out.push('/');
    }
    out
}

pub fn router(app: State) -> Router {
    crate::app::DEVELOPER.store(app.cfg.bool("ServiceSettings.EnableDeveloper"), std::sync::atomic::Ordering::Relaxed);
    crate::outgoing::spawn_worker(app.clone());
    crate::push::spawn(app.clone());
    crate::email::spawn_batching(app.clone());
    let cors = crate::cors::Cors::from_config(&app.cfg);
    let permits =
        std::sync::Arc::new(tokio::sync::Semaphore::new(request_limit(app.db.options().get_max_connections())));
    let r = routes(app.clone())
        .layer(axum::middleware::from_fn_with_state(app, crate::audit::layer))
        .layer(axum::middleware::from_fn(move |req, next| limit_requests(permits.clone(), req, next)))
        .layer(axum::middleware::from_fn(go_headers))
        .layer(axum::middleware::from_fn(clean_path));
    // Go wraps its whole handler in rs/cors when AllowCorsFrom is set.
    let r = match cors {
        Some(c) => r.layer(axum::middleware::from_fn(move |req, next| crate::cors::layer(c.clone(), req, next))),
        None => r,
    };
    if std::env::var("LOG_REQUESTS").is_ok_and(|v| v == "1") {
        r.layer(axum::middleware::from_fn(log_request))
    } else {
        r
    }
}

/// Every cache's size and hit, miss and invalidation counters (Go exports them as Prometheus metrics); system
/// admins only.
async fn cache_stats(
    axum::extract::State(app): axum::extract::State<State>,
    crate::app::Auth(s): crate::app::Auth,
) -> crate::app::ApiResult<axum::Json<std::collections::BTreeMap<&'static str, crate::cache::Stats>>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    Ok(axum::Json(app.caches.stats()))
}

fn routes(app: State) -> Router {
    let gzip = app.cfg.str("ServiceSettings.WebserverMode") == "gzip";
    let r = Router::new()
        .merge(crate::web::routes(app.cfg.str("ServiceSettings.WebserverMode")))
        .route("/api/v4/ebachat/caches", get(cache_stats))
        .route("/api/v4/websocket", get(crate::hub::connect))
        .merge(crate::users::routes())
        .merge(crate::user_lists::routes())
        .merge(crate::user_edit::routes())
        .merge(crate::user_image::routes())
        .merge(crate::user_admin::routes())
        .merge(crate::tokens::routes())
        .merge(crate::bots::routes())
        .merge(crate::oauth::routes())
        .merge(crate::oauth_provider::routes())
        .merge(crate::sso::routes())
        .merge(crate::terms::routes())
        .merge(crate::system::routes())
        .merge(crate::config_api::routes())
        .merge(crate::admin::routes())
        .merge(crate::jobs::routes())
        .merge(crate::bulk_export::routes())
        .merge(crate::logs::routes())
        .merge(crate::license::routes())
        .merge(crate::cpa::routes())
        .merge(crate::unlicensed::routes())
        .merge(crate::plugins::http::routes())
        .merge(crate::properties::routes())
        .merge(crate::commands::routes())
        .merge(crate::command_admin::routes())
        .merge(crate::outgoing::routes())
        .merge(crate::status::routes())
        .merge(crate::channels::routes())
        .merge(crate::sidebar::routes())
        .merge(crate::posts::routes())
        .merge(crate::file_api::routes())
        .merge(crate::emoji::routes())
        .merge(crate::preferences::routes())
        .merge(crate::webhooks::routes())
        .merge(crate::integrations::routes())
        .merge(crate::teams::routes())
        .merge(crate::team_admin::routes())
        .merge(crate::schemes::routes())
        .merge(crate::roles::routes());
    // Go wraps each handler in gzhttp (not its 404 for unknown paths): a layer over the routes, not the fallback.
    let r = if gzip { r.layer(axum::middleware::from_fn(crate::compress::layer)) } else { r };
    let r = r
        .layer(axum::middleware::from_fn(head_as_go))
        .layer(axum::middleware::from_fn(param_patterns))
        .fallback(not_found_marked)
        .method_not_allowed_fallback(method_mismatch)
        .with_state(app);
    let _ = ROUTER.set(r.clone());
    r
}

/// The 404, marked so that a request routed again by `method_mismatch` knows it found nothing.
async fn not_found_marked(
    state: axum::extract::State<State>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    uri: Uri,
) -> axum::response::Response {
    let mut res = not_found(state, method, headers, uri).await;
    res.extensions_mut().insert(Rerouted);
    res
}

#[cfg(test)]
mod tests {
    use super::PARAM_PATTERNS;

    #[test]
    fn param_patterns() {
        assert_eq!(PARAM_PATTERNS.len(), 54);
        let ok = |k: &str, v: &str| PARAM_PATTERNS[k].is_match(v);
        assert!(ok("user_id", "me") && ok("user_id", "abc123"));
        assert!(!ok("user_id", "ж") && !ok("user_id", " ") && !ok("user_id", "a.b") && !ok("user_id", ""));
        assert!(ok("export_name", "x.zip") && !ok("export_name", "x"));
        assert!(ok("syncable_type", "teams") && !ok("syncable_type", "teamsx"));
        assert!(ok("emoji_name", "+1") && ok("username", "a.b-c_d"));
    }

    #[test]
    fn cleaned_paths() {
        use super::cleaned_path as c;
        assert_eq!(c("/api/v4/users/../system/ping"), "/api/v4/system/ping");
        assert_eq!(c("/api/v4//users/./me/"), "/api/v4/users/me/");
        assert_eq!(c("/.."), "/");
        assert_eq!(c("/a/.."), "/");
        assert_eq!(c("/api/v4/users/me"), "/api/v4/users/me");
        assert_eq!(c("/"), "/");
    }
}
