//! The pages Go's web client handler shows instead of the client: an old browser (Internet Explorer or Safari
//! before 12) gets `unsupported_browser`, a desktop app older than `ServiceSettings.MinimumDesktopAppVersion` gets
//! `unsupported_desktop_app`. Both are the server's own templates (`templates/`, Apache-2.0), rendered as Go's
//! `html/template` renders them; `/unsupported_browser.js` is the script the first one loads.

// Based on Mattermost server/channels/web/web.go (CheckClientCompatibility, CheckDesktopAppCompatibility),
// web/static.go, web/unsupported_browser.go, web/unsupported_desktop_app.go and app/user_agent.go
// (GetDesktopAppVersion), Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0. The detection
// follows github.com/avct/uasurfer (Apache-2.0) for the browsers it concerns.

use std::collections::HashMap;

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::app::App;

static BROWSER_TEMPLATE: &str = include_str!("../templates/unsupported_browser.html");
static DESKTOP_TEMPLATE: &str = include_str!("../templates/unsupported_desktop_app.html");
pub static BROWSER_SCRIPT: &[u8] = include_bytes!("../templates/unsupported_browser.js");

// ---------- user agents ----------

/// uasurfer's `Version.parse`: the major version at the start of `s`.
fn major_at(s: &str) -> Option<i64> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() { None } else { digits.parse().ok() }
}

fn find_major(ua: &str, marker: &str) -> Option<i64> {
    ua.find(marker).and_then(|i| major_at(&ua[i + marker.len()..]))
}

/// The browser uasurfer names, for the two Go checks (`BrowserIE`, `BrowserSafari`), with its major version.
fn old_browser(ua: &str) -> Option<(&'static str, i64)> {
    let ua = ua.to_lowercase();
    let has = |s: &str| ua.contains(s);
    let name = if has("blackberry") || has("playbook") || has("bb10") || has("rim ") {
        return None;
    } else if has("applewebkit") {
        if has("googlebot") || has("qq/") || has("qqbrowser/") || has("opr/") || has("opios/") || has("silk/") {
            return None;
        } else if has("edg/") || has("edgios/") || has("edga/") || has("edge/") || has("iemobile/") || has("msie ") {
            "ie"
        } else if has("ucbrowser/")
            || has("ucweb/")
            || has("nintendobrowser/")
            || has("samsungbrowser/")
            || has("coc_coc_browser/")
            || has("yabrowser/")
            || has("chrome/")
            || has("crios/")
            || has("chromium/")
            || has("crmo/")
            || has("android") && has("version/") && !has("like android")
            || has("fxios")
            || has(" spotify/")
            || has("applebot")
        {
            return None;
        } else if has("like gecko")
            && has("mozilla/")
            && has("safari/")
            && !has("linux")
            && !has("android")
            && !has("browser/")
            && !has("os/")
            || has("iphone")
            || has("ipad")
            || has(" gsa/")
        {
            "safari"
        } else if has("qq/") || has("qqbrowser/") {
            return None;
        } else if has("msie") || has("trident") {
            "ie"
        } else {
            return None;
        }
    } else if has("qq/") || has("qqbrowser/") {
        return None;
    } else if has("msie") || has("trident") {
        "ie"
    } else {
        return None;
    };
    // `IsBot` turns these into bots; a bot is never told its browser is old.
    if has("bot") || has("crawl") || has("spider") || has("http://") || has("https://") {
        return None;
    }
    if let Some(v) = find_major(&ua, "version/") {
        return Some((name, v));
    }
    let v = if name == "ie" {
        ["msie ", "edge/", "edgios/", "edga/", "edg/"]
            .iter()
            .find_map(|m| find_major(&ua, m))
            .or_else(|| find_major(&ua, "trident/").map(|t| if (3..=7).contains(&t) { t + 4 } else { t }))
    } else {
        None
    };
    Some((name, v.unwrap_or(0)))
}

/// `CheckClientCompatibility`: Internet Explorer and Safari before 12 are not supported.
pub fn client_compatible(ua: &str) -> bool {
    !matches!(old_browser(ua), Some((_, v)) if v < 12)
}

/// `GetDesktopAppVersion`: the version after `Mattermost/` at the start of the agent or after a space.
pub fn desktop_app_version(ua: &str) -> Option<String> {
    let i = ua.find("Mattermost/")?;
    if i > 0 && ua.as_bytes()[i - 1] != b' ' {
        return None;
    }
    let v = ua[i + "Mattermost/".len()..].split_whitespace().next()?;
    Some(v.chars().take(128).collect())
}

/// A semantic version as `Masterminds/semver` reads it; `strict` is `StrictNewVersion`.
#[derive(PartialEq, Eq, Debug)]
struct SemVer {
    nums: [u64; 3],
    pre: String,
}

fn semver(s: &str, strict: bool) -> Option<SemVer> {
    let s = if strict { s } else { s.strip_prefix('v').unwrap_or(s) };
    let (core, _meta) = s.split_once('+').unwrap_or((s, ""));
    let (core, pre) = core.split_once('-').unwrap_or((core, ""));
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 || strict && parts.len() != 3 {
        return None;
    }
    let mut nums = [0u64; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || strict && p.len() > 1 && p.starts_with('0') {
            return None;
        }
        nums[i] = p.parse().ok()?;
    }
    Some(SemVer { nums, pre: pre.to_string() })
}

fn semver_ge(a: &SemVer, b: &SemVer) -> bool {
    if a.nums != b.nums {
        return a.nums > b.nums;
    }
    match (a.pre.is_empty(), b.pre.is_empty()) {
        (true, _) => true,
        (false, true) => false,
        (false, false) => {
            let (pa, pb): (Vec<&str>, Vec<&str>) = (a.pre.split('.').collect(), b.pre.split('.').collect());
            for (x, y) in pa.iter().zip(pb.iter()) {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                    (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                    _ => x.cmp(y),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord == std::cmp::Ordering::Greater;
                }
            }
            pa.len() >= pb.len()
        }
    }
}

/// `CheckDesktopAppCompatibility`: no minimum, no desktop app, or a version that does not read, all pass.
pub fn desktop_compatible(ua: &str, min: &str) -> bool {
    if min.is_empty() {
        return true;
    }
    let Some(v) = desktop_app_version(ua) else { return true };
    let (Some(client), Some(required)) = (semver(&v, false), semver(min, true)) else { return true };
    semver_ge(&client, &required)
}

// ---------- html/template ----------

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    /// A field path and how its value is escaped.
    Value(Vec<String>, Ctx),
    If(Vec<String>, Vec<Node>),
    Range(Vec<String>, Vec<Node>),
    Template(String, Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ctx {
    Html,
    /// A URL attribute; `true` at the start of its value.
    Url(bool),
}

/// Where an action sits, from the template text before it: in a tag's attribute value or in text.
fn context_of(before: &str) -> Ctx {
    let lt = before.rfind('<');
    let gt = before.rfind('>');
    let Some(lt) = lt.filter(|lt| gt.is_none_or(|gt| gt < *lt)) else { return Ctx::Html };
    let tag = &before[lt..];
    // The last `name="` or `name='` still open.
    let mut open: Option<(usize, char)> = None;
    let b = tag.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match open {
            Some((_, q)) if b[i] as char == q => open = None,
            None if (b[i] == b'"' || b[i] == b'\'') && i > 0 && b[i - 1] == b'=' => open = Some((i, b[i] as char)),
            _ => {}
        }
        i += 1;
    }
    let Some((q, _)) = open else { return Ctx::Html };
    let name_end = q - 1;
    let name_start = tag[..name_end].rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
    let name = tag[name_start..name_end].to_ascii_lowercase();
    let name = name.strip_prefix("data-").unwrap_or(&name);
    let url = matches!(name, "href" | "src" | "action" | "background" | "cite" | "poster")
        || name.contains("src")
        || name.contains("uri")
        || name.contains("url");
    if url { Ctx::Url(q + 1 == tag.len()) } else { Ctx::Html }
}

fn path_of(s: &str) -> Vec<String> {
    s.trim().trim_start_matches('.').split('.').filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// The templates `{{define}}`d in `src`.
fn parse_templates(src: &str) -> HashMap<String, Vec<Node>> {
    let mut defs = HashMap::new();
    // (name, nodes) for the define being read, then the stack of if/range blocks inside it.
    let mut stack: Vec<(String, Vec<String>, Vec<Node>)> = vec![];
    let mut rest = src;
    // The literal text of the current define so far, for the escaping context.
    let mut literal = String::new();
    while let Some(i) = rest.find("{{") {
        let text = &rest[..i];
        if let Some(top) = stack.last_mut() {
            top.2.push(Node::Text(text.to_string()));
            literal.push_str(text);
        }
        let end = rest[i..].find("}}").map(|e| i + e).unwrap_or(rest.len());
        let action = rest[i + 2..end].trim();
        rest = &rest[(end + 2).min(rest.len())..];
        if let Some(name) = action.strip_prefix("define ") {
            stack.push(("define".into(), vec![name.trim().trim_matches('"').to_string()], vec![]));
            literal.clear();
        } else if let Some(p) = action.strip_prefix("if ") {
            stack.push(("if".into(), path_of(p), vec![]));
        } else if let Some(p) = action.strip_prefix("range ") {
            stack.push(("range".into(), path_of(p), vec![]));
        } else if let Some(t) = action.strip_prefix("template ") {
            let t = t.trim();
            let (name, arg) = match t[1..].find('"') {
                Some(q) => (&t[1..q + 1], &t[q + 2..]),
                None => (t, ""),
            };
            if let Some(top) = stack.last_mut() {
                top.2.push(Node::Template(name.to_string(), path_of(arg)));
            }
        } else if action == "end" {
            let Some((kind, p, nodes)) = stack.pop() else { continue };
            match kind.as_str() {
                "define" => {
                    defs.insert(p[0].clone(), nodes);
                }
                "if" => stack.last_mut().map(|t| t.2.push(Node::If(p, nodes))).unwrap_or_default(),
                _ => stack.last_mut().map(|t| t.2.push(Node::Range(p, nodes))).unwrap_or_default(),
            }
        } else if let Some(top) = stack.last_mut() {
            top.2.push(Node::Value(path_of(action), context_of(&literal)));
        }
    }
    defs
}

fn lookup<'a>(dot: &'a Value, path: &[String]) -> &'a Value {
    path.iter().fold(dot, |v, k| v.get(k).unwrap_or(&Value::Null))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}

/// html/template's `htmlEscaper` (text, quoted attributes, RCDATA).
fn html_escaper(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push('\u{FFFD}'),
            '"' => out.push_str("&#34;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '+' => out.push_str("&#43;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// `urlNormalizer`: bytes a URL may not hold percent-encoded (lowercase hex), valid escapes kept.
fn url_normalizer(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in b.iter().enumerate() {
        let keep = c.is_ascii_alphanumeric()
            || b"!#$&*+,/:;=?@[]-._~".contains(&c)
            || c == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit();
        if keep { out.push(c as char) } else { out.push_str(&format!("%{c:02x}")) }
    }
    out
}

/// `urlFilter`: only http, https and mailto URLs (or none with a scheme).
fn url_filter(s: &str) -> String {
    if let Some((proto, _)) = s.split_once(':')
        && !proto.contains('/')
        && !["http", "https", "mailto"].iter().any(|p| p.eq_ignore_ascii_case(proto))
    {
        return "#ZgotmplZ".into();
    }
    s.to_string()
}

fn render_nodes(defs: &HashMap<String, Vec<Node>>, nodes: &[Node], dot: &Value, out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Value(p, ctx) => {
                let v = lookup(dot, p);
                let s = match v {
                    Value::Null => String::new(),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                match ctx {
                    Ctx::Html => out.push_str(&html_escaper(&s)),
                    Ctx::Url(start) => {
                        let s = if *start { url_filter(&s) } else { s };
                        out.push_str(&html_escaper(&url_normalizer(&s)));
                    }
                }
            }
            Node::If(p, body) => {
                if truthy(lookup(dot, p)) {
                    render_nodes(defs, body, dot, out);
                }
            }
            Node::Range(p, body) => {
                if let Value::Array(items) = lookup(dot, p) {
                    for item in items {
                        render_nodes(defs, body, item, out);
                    }
                }
            }
            Node::Template(name, p) => {
                if let Some(body) = defs.get(name) {
                    render_nodes(defs, body, lookup(dot, p), out);
                }
            }
        }
    }
}

fn render(src: &str, name: &str, data: &Value) -> String {
    let defs = parse_templates(src);
    let mut out = String::new();
    if let Some(body) = defs.get(name) {
        render_nodes(&defs, body, data, &mut out);
    }
    out
}

fn t(id: &str) -> String {
    crate::i18n::t(id).unwrap_or("").to_string()
}

/// `ensureTrailingSlash` of the subpath.
fn subpath_slash(app: &App) -> String {
    let s = crate::web::subpath(app.cfg.str("ServiceSettings.SiteURL"));
    if s.ends_with('/') { s } else { format!("{s}/") }
}

fn page(body: String) -> Response {
    let mut r = (StatusCode::OK, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    r
}

/// `renderUnsupportedBrowser`.
fn unsupported_browser(app: &App, headers: &HeaderMap, uri: &Uri) -> Response {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let l = ua.to_lowercase();
    let windows = l.contains("windows") && !l.contains("windows phone") && !l.contains("xbox");
    let windows10 = windows && find_major(&l, "windows nt ") == Some(10);
    let mac_os_x = l.contains("macintosh") && find_major(&l, "os x ") == Some(10);
    let safari = matches!(old_browser(&ua), Some(("safari", _)));
    let mut props = json!({
        "Subpath": subpath_slash(app),
        "DownloadAppOrUpgradeBrowserString": t("web.error.unsupported_browser.download_app_or_upgrade_browser"),
        "LearnMoreString": t("web.error.unsupported_browser.learn_more"),
        "NoLongerSupportString": t(if safari {
            "web.error.unsupported_browser.no_longer_support_version"
        } else {
            "web.error.unsupported_browser.no_longer_support"
        }),
    });
    let app_entry = |os: &str, guide: &str| {
        json!({
            "LogoSrc": if os == "mac" { "mac.png" } else { "windows.svg" },
            "Title": t("web.error.unsupported_browser.download_the_app"),
            "SupportedVersionString": t(&format!("web.error.unsupported_browser.min_os_version.{os}")),
            "Label": t("web.error.unsupported_browser.download"),
            // Go links Mattermost's downloads; this server, the configured app download link.
            "Link": app.cfg.str("NativeAppSettings.AppDownloadLink"),
            "InstallGuide": t(&format!("web.error.unsupported_browser.install_guide.{os}")),
            "InstallGuideLink": guide,
        })
    };
    if windows {
        props["App"] =
            app_entry("windows", "https://docs.mattermost.com/install/desktop.html#windows-10-windows-8-1-windows-7");
    } else if mac_os_x {
        props["App"] = app_entry("mac", "https://docs.mattermost.com/install/desktop.html#mac-os-x-10-9");
    }
    let browser = |key: &str, logo: &str, src: &str| {
        json!({
            "LogoSrc": logo,
            "Title": t(&format!("web.error.unsupported_browser.browser_title.{key}")),
            "SupportedVersionString": t(&format!("web.error.unsupported_browser.min_browser_version.{key}")),
            "Src": src,
            "GetLatestString": t(&format!("web.error.unsupported_browser.browser_get_latest.{key}")),
        })
    };
    let mut browsers = vec![
        browser("chrome", "chrome.svg", "http://www.google.com/chrome"),
        browser("firefox", "firefox.svg", "https://www.mozilla.org/firefox/new/"),
    ];
    if safari {
        browsers.push(browser("safari", "safari.svg", "macappstore://showUpdatesPage"));
    }
    props["Browsers"] = Value::Array(browsers);
    if windows10 {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        let request_uri = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        props["SystemBrowser"] = json!({
            "LogoSrc": "edge.svg",
            "Title": t("web.error.unsupported_browser.browser_title.edge"),
            "SupportedVersionString": t("web.error.unsupported_browser.min_browser_version.edge"),
            "LabelOpen": t("web.error.unsupported_browser.open_system_browser.edge"),
            "LinkOpen": format!("microsoft-edge:http://{host}{request_uri}"),
            "LinkMakeDefault": "ms-settings:defaultapps",
            "OrString": t("web.error.unsupported_browser.system_browser_or"),
            "MakeDefaultString": t("web.error.unsupported_browser.system_browser_make_default"),
        });
    }
    page(render(BROWSER_TEMPLATE, "unsupported_browser", &json!({ "Props": props })))
}

/// `renderUnsupportedDesktopApp`.
fn unsupported_desktop_app(app: &App, ua: &str) -> Response {
    let site_name = crate::brand::name();
    let current = desktop_app_version(ua).unwrap_or_else(|| "unknown".into());
    let min = app.cfg.str("ServiceSettings.MinimumDesktopAppVersion");
    let message = crate::i18n::tr(
        "web.error.unsupported_desktop_app.message",
        &[("SiteName", site_name), ("CurrentVersion", &current), ("MinimumVersion", min)],
    )
    .unwrap_or_default();
    let year = crate::users::civil_from_days(crate::app::now_ms().div_euclid(86_400_000)).0;
    let props = json!({
        "Subpath": subpath_slash(app),
        "Title": t("web.error.unsupported_desktop_app.title"),
        "MessageString": message,
        "DownloadButtonLabel": t("web.error.unsupported_desktop_app.download_button"),
        "AssistanceString": t("web.error.unsupported_desktop_app.assistance"),
        "FooterAboutLabel": t("web.error.unsupported_desktop_app.footer_about"),
        "FooterPrivacyLabel": t("web.error.unsupported_desktop_app.footer_privacy"),
        "FooterTermsLabel": t("web.error.unsupported_desktop_app.footer_terms"),
        "FooterHelpLabel": t("web.error.unsupported_desktop_app.footer_help"),
        "DownloadLink": app.cfg.str("NativeAppSettings.AppDownloadLink"),
        "CopyrightYear": year,
        "SiteName": site_name,
        "AboutLink": app.cfg.str("SupportSettings.AboutLink"),
        "PrivacyPolicyLink": app.cfg.str("SupportSettings.PrivacyPolicyLink"),
        "TermsOfServiceLink": app.cfg.str("SupportSettings.TermsOfServiceLink"),
        "HelpLink": app.cfg.str("SupportSettings.HelpLink"),
    });
    page(render(DESKTOP_TEMPLATE, "unsupported_desktop_app", &json!({ "Props": props })))
}

/// The page Go's `root` shows instead of the client for this agent, if any.
pub fn page_for(app: &App, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !client_compatible(ua) {
        return Some(unsupported_browser(app, headers, uri));
    }
    if !desktop_compatible(ua, app.cfg.str("ServiceSettings.MinimumDesktopAppVersion")) {
        return Some(unsupported_desktop_app(app, ua));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents() {
        let ie11 = "Mozilla/5.0 (Windows NT 10.0; Trident/7.0; rv:11.0) like Gecko";
        assert_eq!(old_browser(ie11), Some(("ie", 11)));
        assert!(!client_compatible(ie11));
        let safari9 = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_11_6) AppleWebKit/601.7.7 (KHTML, like Gecko) Version/9.1.2 Safari/601.7.7";
        assert_eq!(old_browser(safari9), Some(("safari", 9)));
        let chrome = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
        assert!(client_compatible(chrome));
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36 Edg/120.0";
        assert!(client_compatible(edge));
        let desktop = "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 (KHTML, like Gecko) Mattermost/5.9.0 Chrome/124.0 Electron/30.1.2 Safari/537.36";
        assert_eq!(desktop_app_version(desktop).as_deref(), Some("5.9.0"));
        assert!(!desktop_compatible(desktop, "5.10.0"));
        assert!(desktop_compatible(desktop, "5.9.0"));
        assert!(desktop_compatible(desktop, "5.9"));
        assert!(desktop_compatible("curl/8", "5.10.0"));
    }

    #[test]
    fn template() {
        let src = r#"{{define "a"}}<a href='{{.L}}' title="{{.T}}">{{.T}}</a><img src='x/{{.S}}'>{{end}}{{define "p"}}[{{range .Props.Items}}{{template "a" .}}{{end}}]{{if .Props.No}}no{{end}}{{end}}"#;
        let data = json!({"Props": {"Items": [{"L": "javascript:x", "T": "a'b+c", "S": "a b"}]}});
        assert_eq!(
            render(src, "p", &data),
            r#"[<a href='#ZgotmplZ' title="a&#39;b&#43;c">a&#39;b&#43;c</a><img src='x/a%20b'>]"#
        );
    }
}
