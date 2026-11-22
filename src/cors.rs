//! CORS as Go wraps its whole handler when `ServiceSettings.AllowCorsFrom` is set (`rs/cors` 1.11.1 with
//! Mattermost's options): preflights answered here with 204, actual requests given `Vary: Origin` and, for an
//! allowed origin, `Access-Control-Allow-Origin` and the exposed headers. Built once at start, as Go's wrapper.

// Based on Mattermost server/channels/app/server.go (Start, corsAllowedMethods), Copyright (c) 2015-present
// Mattermost, Inc. Licensed under AGPL-3.0; and on github.com/rs/cors (MIT), Copyright (c) 2014 Olivier Poitrey.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// `corsAllowedMethods`.
const METHODS: &[&str] = &["POST", "GET", "OPTIONS", "PUT", "PATCH", "DELETE"];
const PREFLIGHT_VARY: &str = "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

pub struct Cors {
    all: bool,
    origins: Vec<String>,
    wildcards: Vec<(String, String)>,
    exposed: Option<HeaderValue>,
    credentials: bool,
}

impl Cors {
    /// `cors.New`: the space-separated `AllowCorsFrom`, lowercased; `*` alone allows all; one `*` inside an origin
    /// is a wildcard. `None` when the setting is empty (no wrapper in Go).
    pub fn from_config(cfg: &crate::config::Config) -> Option<Arc<Cors>> {
        let allowed = cfg.str("ServiceSettings.AllowCorsFrom");
        if allowed.is_empty() {
            return None;
        }
        let mut c = Cors {
            all: false,
            origins: Vec::new(),
            wildcards: Vec::new(),
            exposed: None,
            credentials: cfg.bool("ServiceSettings.CorsAllowCredentials"),
        };
        for o in allowed.split_whitespace().map(str::to_lowercase) {
            if o == "*" {
                c.all = true;
                c.origins.clear();
                c.wildcards.clear();
                break;
            }
            match o.find('*') {
                Some(i) => c.wildcards.push((o[..i].to_string(), o[i + 1..].to_string())),
                None => c.origins.push(o),
            }
        }
        let exposed: Vec<String> =
            cfg.str("ServiceSettings.CorsExposedHeaders").split_whitespace().map(canonical).collect();
        if !exposed.is_empty() {
            c.exposed = HeaderValue::from_str(&exposed.join(", ")).ok();
        }
        Some(Arc::new(c))
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        if self.all {
            return true;
        }
        let o = origin.to_lowercase();
        self.origins.contains(&o)
            || self.wildcards.iter().any(|(p, s)| o.len() >= p.len() + s.len() && o.starts_with(p) && o.ends_with(s))
    }

    fn allow_origin(&self, origin: &HeaderValue) -> HeaderValue {
        if self.all { HeaderValue::from_static("*") } else { origin.clone() }
    }
}

/// `http.CanonicalHeaderKey`.
fn canonical(h: &str) -> String {
    h.split('-')
        .map(|p| {
            let mut c = p.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn prepend_vary(h: &mut HeaderMap, v: &'static str) {
    let old: Vec<HeaderValue> = h.get_all("vary").iter().cloned().collect();
    h.remove("vary");
    h.append("vary", HeaderValue::from_static(v));
    for o in old {
        h.append("vary", o);
    }
}

/// `Cors.Handler`.
pub async fn layer(cors: Arc<Cors>, req: Request, next: Next) -> Response {
    let origin = req.headers().get("origin").cloned();
    let origin_str = origin.as_ref().and_then(|o| o.to_str().ok()).unwrap_or("").to_string();
    if req.method() == Method::OPTIONS
        && req.headers().get("access-control-request-method").is_some_and(|v| !v.is_empty())
    {
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        h.append("vary", HeaderValue::from_static(PREFLIGHT_VARY));
        let req_method = req.headers().get("access-control-request-method").cloned();
        let method_ok =
            req_method.as_ref().and_then(|m| m.to_str().ok()).is_some_and(|m| m == "OPTIONS" || METHODS.contains(&m));
        if origin_str.is_empty() || !cors.origin_allowed(&origin_str) || !method_ok {
            return r;
        }
        h.insert("access-control-allow-origin", cors.allow_origin(origin.as_ref().unwrap()));
        if let Some(m) = req_method {
            h.insert("access-control-allow-methods", m);
        }
        if let Some(rh) = req.headers().get("access-control-request-headers").filter(|v| !v.is_empty()) {
            h.insert("access-control-allow-headers", rh.clone());
        }
        if cors.credentials {
            h.insert("access-control-allow-credentials", HeaderValue::from_static("true"));
        }
        h.insert("access-control-max-age", HeaderValue::from_static("86400"));
        return r;
    }
    let method_ok = METHODS.contains(&req.method().as_str());
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    // Go sets Vary: Origin before the handler runs, so it comes first.
    prepend_vary(h, "Origin");
    if origin_str.is_empty() || !cors.origin_allowed(&origin_str) || !method_ok {
        return r;
    }
    h.insert("access-control-allow-origin", cors.allow_origin(origin.as_ref().unwrap()));
    if let Some(e) = &cors.exposed {
        h.insert("access-control-expose-headers", e.clone());
    }
    if cors.credentials {
        h.insert("access-control-allow-credentials", HeaderValue::from_static("true"));
    }
    r
}
