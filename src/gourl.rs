//! URLs as Go's `net/url` parses and prints them, for the redirects the web routes build: `url.Parse`,
//! `URL.String`, `URL.Query` / `url.ParseQuery`, `url.Values.Encode`, `url.QueryEscape` and `url.PathUnescape`.
//! Only what those redirects need: no IPv6 zone checks, no user-info escaping rules beyond Go's defaults.

// Based on Go's net/url (BSD-3-Clause, Copyright (c) 2009 The Go Authors) as Mattermost
// server/channels/web/oauth.go, utils/utils.go and app/oauth.go use it.

/// `url.URL`, with the path and fragment kept as they came (Go's `RawPath`/`RawFragment` when valid).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub opaque: String,
    pub user: Option<String>,
    pub host: String,
    /// The path as written in the URL (escaped).
    pub raw_path: String,
    pub raw_query: String,
    pub force_query: bool,
    pub fragment: Option<String>,
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn unhex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// `url.unescape` in path mode (`PathUnescape`): `%XX` decoded, `+` kept; a bad escape is an error.
pub fn path_unescape(s: &str) -> Result<String, String> {
    unescape(s, false)
}

/// `url.QueryUnescape`: `%XX` decoded, `+` is a space.
pub fn query_unescape(s: &str) -> Result<String, String> {
    unescape(s, true)
}

fn unescape(s: &str, query: bool) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len() || !is_hex(b[i + 1]) || !is_hex(b[i + 2]) {
                    let end = (i + 3).min(b.len());
                    return Err(format!("invalid URL escape {:?}", String::from_utf8_lossy(&b[i..end])));
                }
                out.push(unhex(b[i + 1]) << 4 | unhex(b[i + 2]));
                i += 3;
            }
            b'+' if query => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Path,
    PathSegment,
    QueryComponent,
    Fragment,
}

/// `shouldEscape`.
fn should_escape(c: u8, mode: Mode) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Mode::Path => c == b'?',
            Mode::PathSegment => c == b'/' || c == b';' || c == b',' || c == b'?',
            Mode::QueryComponent => true,
            Mode::Fragment => false,
        },
        _ => {
            if mode == Mode::Fragment {
                return !matches!(c, b'!' | b'(' | b')' | b'*');
            }
            true
        }
    }
}

fn escape(s: &str, mode: Mode) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if should_escape(c, mode) {
            if c == b' ' && mode == Mode::QueryComponent {
                out.push('+');
            } else {
                out.push_str(&format!("%{c:02X}"));
            }
        } else {
            out.push(c as char);
        }
    }
    out
}

/// `url.QueryEscape`.
pub fn query_escape(s: &str) -> String {
    escape(s, Mode::QueryComponent)
}

/// `url.PathEscape`.
pub fn path_escape(s: &str) -> String {
    escape(s, Mode::PathSegment)
}

/// `validEncoded`: a raw path or fragment Go would print as it is.
fn valid_encoded(s: &str, mode: Mode) -> bool {
    for &c in s.as_bytes() {
        match c {
            b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':' | b'@' | b'[' | b']' => {
            }
            b'%' => {}
            _ if should_escape(c, mode) => return false,
            _ => {}
        }
    }
    true
}

/// `getScheme`.
fn get_scheme(raw: &str) -> Result<(&str, &str), String> {
    for (i, c) in raw.bytes().enumerate() {
        if c.is_ascii_alphabetic() {
            continue;
        }
        if c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.' {
            if i == 0 {
                return Ok(("", raw));
            }
            continue;
        }
        if c == b':' {
            if i == 0 {
                return Err("missing protocol scheme".into());
            }
            return Ok((&raw[..i], &raw[i + 1..]));
        }
        return Ok(("", raw));
    }
    Ok(("", raw))
}

impl Url {
    /// `url.Parse`.
    pub fn parse(raw: &str) -> Result<Url, String> {
        let (rest, frag) = match raw.split_once('#') {
            Some((r, f)) => (r, Some(f)),
            None => (raw, None),
        };
        let mut u = Url::parse_no_fragment(rest)?;
        if let Some(f) = frag {
            path_unescape(f).map_err(|e| format!("parse {raw:?}: {e}"))?;
            u.fragment = Some(f.to_string());
        }
        Ok(u)
    }

    fn parse_no_fragment(raw: &str) -> Result<Url, String> {
        if raw.bytes().any(|c| c < 0x20 || c == 0x7f) {
            return Err(format!("parse {raw:?}: net/url: invalid control character in URL"));
        }
        if raw.is_empty() {
            return Ok(Url::default());
        }
        let mut u = Url::default();
        if raw == "*" {
            u.raw_path = "*".into();
            return Ok(u);
        }
        let (scheme, mut rest) = get_scheme(raw).map_err(|e| format!("parse {raw:?}: {e}"))?;
        u.scheme = scheme.to_ascii_lowercase();
        if rest.ends_with('?') && rest.matches('?').count() == 1 {
            u.force_query = true;
            rest = &rest[..rest.len() - 1];
        } else if let Some((r, q)) = rest.split_once('?') {
            u.raw_query = q.to_string();
            rest = r;
        }
        if !rest.starts_with('/') {
            if !u.scheme.is_empty() {
                u.opaque = rest.to_string();
                return Ok(u);
            }
            // A first segment with a colon would read as a scheme.
            let seg = rest.split('/').next().unwrap_or("");
            if seg.contains(':') {
                return Err(format!("parse {raw:?}: first path segment in URL cannot contain colon"));
            }
        }
        if (!u.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
            let after = &rest[2..];
            let (authority, path) = match after.find('/') {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            };
            let (user, host) = match authority.rfind('@') {
                Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
                None => (None, authority),
            };
            if let Some((h, port)) = host.rsplit_once(':')
                && !h.ends_with(']')
                && !h.contains(':')
                && !port.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(format!("parse {raw:?}: invalid port {:?} after host", format!(":{port}")));
            }
            if host.contains('%') {
                path_unescape(host).map_err(|e| format!("parse {raw:?}: {e}"))?;
            }
            u.user = user;
            u.host = host.to_string();
            rest = path;
        }
        path_unescape(rest).map_err(|e| format!("parse {raw:?}: {e}"))?;
        u.raw_path = rest.to_string();
        Ok(u)
    }

    /// The decoded path (`URL.Path`).
    pub fn path(&self) -> String {
        path_unescape(&self.raw_path).unwrap_or_default()
    }

    /// `URL.EscapedPath`.
    pub fn escaped_path(&self) -> String {
        if valid_encoded(&self.raw_path, Mode::Path) && path_unescape(&self.raw_path).is_ok() {
            return self.raw_path.clone();
        }
        if self.raw_path == "*" {
            return "*".into();
        }
        escape(&self.path(), Mode::Path)
    }

    /// `URL.SetPath` from a decoded path.
    pub fn set_path(&mut self, p: &str) {
        self.raw_path = escape(p, Mode::Path);
    }

    /// `URL.IsAbs`.
    pub fn is_abs(&self) -> bool {
        !self.scheme.is_empty()
    }

    /// `URL.Hostname`: the host without its port.
    pub fn hostname(&self) -> &str {
        let h = &self.host;
        if let Some(rest) = h.strip_prefix('[') {
            return rest.split(']').next().unwrap_or("");
        }
        h.split(':').next().unwrap_or("")
    }

    /// `URL.Query`: `ParseQuery` with the bad pairs dropped.
    pub fn query(&self) -> Values {
        parse_query(&self.raw_query).0
    }

    /// `URL.String`.
    pub fn to_go_string(&self) -> String {
        let mut buf = String::new();
        if !self.scheme.is_empty() {
            buf.push_str(&self.scheme);
            buf.push(':');
        }
        if !self.opaque.is_empty() {
            buf.push_str(&self.opaque);
        } else {
            let path = self.escaped_path();
            if !self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some() {
                if !self.host.is_empty() || !path.is_empty() || self.user.is_some() {
                    buf.push_str("//");
                }
                if let Some(u) = &self.user {
                    buf.push_str(u);
                    buf.push('@');
                }
                buf.push_str(&self.host);
            }
            if !path.is_empty() && !path.starts_with('/') && !self.host.is_empty() {
                buf.push('/');
            }
            if buf.is_empty() {
                // A relative path whose first segment has a colon is written "./a:b".
                let seg = path.split('/').next().unwrap_or("");
                if seg.contains(':') {
                    buf.push_str("./");
                }
            }
            buf.push_str(&path);
        }
        if self.force_query || !self.raw_query.is_empty() {
            buf.push('?');
            buf.push_str(&self.raw_query);
        }
        if let Some(f) = &self.fragment {
            buf.push('#');
            if valid_encoded(f, Mode::Fragment) && path_unescape(f).is_ok() {
                buf.push_str(f);
            } else {
                buf.push_str(&escape(&path_unescape(f).unwrap_or_default(), Mode::Fragment));
            }
        }
        buf
    }
}

/// `url.Values`: keys in first-seen order, values in order.
#[derive(Debug, Clone, Default)]
pub struct Values(pub Vec<(String, Vec<String>)>);

impl Values {
    pub fn get(&self, k: &str) -> &str {
        self.0.iter().find(|(key, _)| key == k).and_then(|(_, v)| v.first()).map(String::as_str).unwrap_or("")
    }
    pub fn has(&self, k: &str) -> bool {
        self.0.iter().any(|(key, _)| key == k)
    }
    pub fn set(&mut self, k: &str, v: &str) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some((_, vals)) => *vals = vec![v.to_string()],
            None => self.0.push((k.to_string(), vec![v.to_string()])),
        }
    }
    pub fn add(&mut self, k: &str, v: &str) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some((_, vals)) => vals.push(v.to_string()),
            None => self.0.push((k.to_string(), vec![v.to_string()])),
        }
    }
    /// `Values.Encode`: sorted by key.
    pub fn encode(&self) -> String {
        let mut keys: Vec<&(String, Vec<String>)> = self.0.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = String::new();
        for (k, vs) in keys {
            let ke = query_escape(k);
            for v in vs {
                if !out.is_empty() {
                    out.push('&');
                }
                out.push_str(&ke);
                out.push('=');
                out.push_str(&query_escape(v));
            }
        }
        out
    }
}

/// `url.ParseQuery`: the values, and whether some pair was bad (a semicolon, a bad escape).
pub fn parse_query(q: &str) -> (Values, bool) {
    let mut v = Values::default();
    let mut bad = false;
    for kv in q.split('&') {
        if kv.is_empty() {
            continue;
        }
        if kv.contains(';') {
            bad = true;
            continue;
        }
        let (k, val) = kv.split_once('=').unwrap_or((kv, ""));
        let (Ok(k), Ok(val)) = (query_unescape(k), query_unescape(val)) else {
            bad = true;
            continue;
        };
        v.add(&k, &val);
    }
    (v, bad)
}

/// `path.Clean`.
pub fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|s| *s != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// `path.Join`.
pub fn join_path(elems: &[&str]) -> String {
    let nonempty: Vec<&str> = elems.iter().copied().filter(|e| !e.is_empty()).collect();
    if nonempty.is_empty() {
        return String::new();
    }
    clean_path(&nonempty.join("/"))
}

/// `url.JoinPath(base, elem)`.
pub fn url_join_path(base: &str, elem: &str) -> Result<String, String> {
    let mut u = Url::parse(base)?;
    let first = u.escaped_path();
    let p = if !first.starts_with('/') {
        let j = join_path(&[&format!("/{first}"), elem]);
        j.get(1..).unwrap_or("").to_string()
    } else {
        join_path(&[&first, elem])
    };
    let mut p = p;
    if elem.ends_with('/') && !p.ends_with('/') {
        p.push('/');
    }
    let decoded = path_unescape(&p)?;
    u.set_path(&decoded);
    Ok(u.to_go_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_print() {
        let u = Url::parse("http://localhost:8065/cb?x=1&y=a+b#frag").unwrap();
        assert_eq!(
            (u.scheme.as_str(), u.host.as_str(), u.raw_path.as_str(), u.raw_query.as_str()),
            ("http", "localhost:8065", "/cb", "x=1&y=a+b")
        );
        assert_eq!(u.to_go_string(), "http://localhost:8065/cb?x=1&y=a+b#frag");
        assert_eq!(u.query().get("y"), "a b");
        let m = Url::parse("mmauth://callback").unwrap();
        assert_eq!((m.scheme.as_str(), m.host.as_str(), m.raw_path.as_str()), ("mmauth", "callback", ""));
        assert!(Url::parse("http://a b.com/%zz").is_err());
        assert_eq!(url_join_path("http://x.org", "/oauth/authorize").unwrap(), "http://x.org/oauth/authorize");
        assert_eq!(
            url_join_path("http://x.org/chat/", "/oauth/authorize").unwrap(),
            "http://x.org/chat/oauth/authorize"
        );
        assert_eq!(url_join_path("http://x.org", "/a/../../b").unwrap(), "http://x.org/b");
        let mut v = Values::default();
        v.set("state", "s t");
        v.set("code", "c&d");
        assert_eq!(v.encode(), "code=c%26d&state=s+t");
        assert_eq!(clean_path("/a/./b/../c/"), "/a/c");
    }
}
