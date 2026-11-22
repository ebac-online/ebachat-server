//! The email templates, rendered as Go's `html/template` renders them. `data/email_templates.txt` holds them
//! already escaped by Go (`tools/emailtpl`): every action names the escapers of its context and the comments are
//! gone, so this interpreter only evaluates the pipelines and runs those escapers.

// Based on Go's html/template escapers (html.go, url.go, css.go, content.go; BSD-3-Clause, Copyright The Go
// Authors) and Mattermost server/templates/*.html (Apache-2.0, Copyright (c) 2015-present Mattermost, Inc.).

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

/// A value in the template data, typed as Go's `html/template` sees it.
#[derive(Clone, Debug, Default)]
pub enum V {
    #[default]
    Nil,
    Str(String),
    /// `template.HTML`: written without escaping in text.
    Html(String),
    Bool(bool),
    Int(i64),
    List(Vec<V>),
    Map(BTreeMap<String, V>),
}

impl V {
    fn truth(&self) -> bool {
        match self {
            V::Nil => false,
            V::Str(s) | V::Html(s) => !s.is_empty(),
            V::Bool(b) => *b,
            V::Int(i) => *i != 0,
            V::List(l) => !l.is_empty(),
            // Structs are always true; maps stand for structs here.
            V::Map(_) => true,
        }
    }
    fn field(&self, k: &str) -> V {
        match self {
            V::Map(m) => m.get(k).cloned().unwrap_or_default(),
            _ => V::Nil,
        }
    }
    /// `stringify`: the text and whether it is trusted HTML.
    fn text(&self) -> (String, bool) {
        match self {
            V::Nil => (String::new(), false),
            V::Str(s) => (s.clone(), false),
            V::Html(s) => (s.clone(), true),
            V::Bool(b) => (b.to_string(), false),
            V::Int(i) => (i.to_string(), false),
            V::List(_) | V::Map(_) => (String::new(), false),
        }
    }
}

impl From<&str> for V {
    fn from(s: &str) -> V {
        V::Str(s.to_string())
    }
}
impl From<String> for V {
    fn from(s: String) -> V {
        V::Str(s)
    }
}
impl From<bool> for V {
    fn from(b: bool) -> V {
        V::Bool(b)
    }
}

#[derive(Debug)]
enum Node {
    Text(String),
    Action { assign: Option<String>, cmd: Cmd, escapers: Vec<String> },
    If { cond: Cmd, then: Vec<Node>, els: Vec<Node> },
    Range { over: Cmd, body: Vec<Node> },
    Template { name: String, arg: Cmd },
}

#[derive(Debug)]
enum Arg {
    /// `.A.B` (dot when empty).
    Dot(Vec<String>),
    /// `$.A.B` or `$var.A`.
    Var(String, Vec<String>),
    Int(i64),
}

#[derive(Debug)]
enum Cmd {
    Arg(Arg),
    Call(String, Vec<Arg>),
}

fn parse_arg(s: &str) -> Arg {
    if let Some(rest) = s.strip_prefix('$') {
        let mut parts = rest.split('.');
        let var = parts.next().unwrap_or("").to_string();
        return Arg::Var(var, parts.filter(|p| !p.is_empty()).map(str::to_string).collect());
    }
    if let Ok(i) = s.parse() {
        return Arg::Int(i);
    }
    Arg::Dot(s.split('.').filter(|p| !p.is_empty()).map(str::to_string).collect())
}

fn parse_cmd(s: &str) -> Cmd {
    let words: Vec<&str> = s.split_whitespace().collect();
    match words.as_slice() {
        [one] => Cmd::Arg(parse_arg(one)),
        [f, args @ ..] => Cmd::Call(f.to_string(), args.iter().map(|a| parse_arg(a)).collect()),
        [] => Cmd::Arg(Arg::Dot(vec![])),
    }
}

/// The templates by name.
struct Set(HashMap<String, Vec<Node>>);

fn parse(src: &str) -> Set {
    let mut set = HashMap::new();
    let mut toks: Vec<(bool, &str)> = vec![];
    let mut rest = src;
    while let Some(i) = rest.find("{{") {
        toks.push((false, &rest[..i]));
        let end = rest[i..].find("}}").map(|e| i + e).unwrap_or(rest.len());
        toks.push((true, rest[i + 2..end].trim()));
        rest = &rest[(end + 2).min(rest.len())..];
    }
    toks.push((false, rest));
    let mut it = toks.into_iter().peekable();
    while let Some((action, t)) = it.next() {
        if action && let Some(name) = t.strip_prefix("define ") {
            let name = name.trim().trim_matches('"').to_string();
            let (body, _) = parse_list(&mut it);
            set.insert(name, body);
        }
    }
    Set(set)
}

/// Nodes up to the `{{end}}` or `{{else}}` that closes them (returned: whether it was `else`).
fn parse_list(it: &mut std::iter::Peekable<std::vec::IntoIter<(bool, &str)>>) -> (Vec<Node>, bool) {
    let mut out = vec![];
    while let Some((action, t)) = it.next() {
        if !action {
            if !t.is_empty() {
                out.push(Node::Text(t.to_string()));
            }
            continue;
        }
        if t == "end" {
            return (out, false);
        }
        if t == "else" {
            return (out, true);
        }
        if t.starts_with("/*") {
            continue;
        }
        if let Some(c) = t.strip_prefix("if ") {
            let (then, has_else) = parse_list(it);
            let els = if has_else { parse_list(it).0 } else { vec![] };
            out.push(Node::If { cond: parse_cmd(c), then, els });
        } else if let Some(c) = t.strip_prefix("range ") {
            let (body, _) = parse_list(it);
            out.push(Node::Range { over: parse_cmd(c), body });
        } else if let Some(c) = t.strip_prefix("template ") {
            let (name, arg) = c.trim().trim_start_matches('"').split_once('"').unwrap_or((c, "."));
            out.push(Node::Template { name: name.to_string(), arg: parse_cmd(arg.trim()) });
        } else {
            let (assign, pipe) = match t.split_once(":=") {
                Some((v, p)) => (Some(v.trim().trim_start_matches('$').to_string()), p),
                None => (None, t),
            };
            let mut stages = pipe.split('|').map(str::trim);
            let cmd = parse_cmd(stages.next().unwrap_or(""));
            out.push(Node::Action { assign, cmd, escapers: stages.map(str::to_string).collect() });
        }
    }
    (out, false)
}

fn set() -> &'static Set {
    static S: OnceLock<Set> = OnceLock::new();
    S.get_or_init(|| parse(include_str!("../data/email_templates.txt")))
}

struct Scope<'a> {
    root: &'a V,
    vars: Vec<(String, V)>,
}

fn eval_arg(a: &Arg, dot: &V, sc: &Scope) -> V {
    let walk = |mut v: V, path: &[String]| {
        for p in path {
            v = v.field(p);
        }
        v
    };
    match a {
        Arg::Dot(path) => walk(dot.clone(), path),
        Arg::Var(name, path) if name.is_empty() => walk(sc.root.clone(), path),
        Arg::Var(name, path) => {
            let v = sc.vars.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
            walk(v, path)
        }
        Arg::Int(i) => V::Int(*i),
    }
}

fn eval(c: &Cmd, dot: &V, sc: &Scope) -> V {
    match c {
        Cmd::Arg(a) => eval_arg(a, dot, sc),
        Cmd::Call(f, args) => {
            let vals: Vec<V> = args.iter().map(|a| eval_arg(a, dot, sc)).collect();
            match f.as_str() {
                "len" => V::Int(match vals.first() {
                    Some(V::List(l)) => l.len() as i64,
                    Some(V::Map(m)) => m.len() as i64,
                    Some(V::Str(s) | V::Html(s)) => s.len() as i64,
                    _ => 0,
                }),
                "eq" => V::Bool(match (vals.first(), vals.get(1)) {
                    (Some(V::Int(a)), Some(V::Int(b))) => a == b,
                    (Some(a), Some(b)) => a.text() == b.text(),
                    _ => false,
                }),
                "or" => vals.iter().find(|v| v.truth()).or(vals.last()).cloned().unwrap_or_default(),
                "and" => vals.iter().find(|v| !v.truth()).or(vals.last()).cloned().unwrap_or_default(),
                "not" => V::Bool(!vals.first().is_some_and(V::truth)),
                _ => V::Nil,
            }
        }
    }
}

fn exec(nodes: &[Node], dot: &V, sc: &mut Scope, out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Action { assign: Some(var), cmd, .. } => {
                let v = eval(cmd, dot, sc);
                sc.vars.push((var.clone(), v));
            }
            Node::Action { assign: None, cmd, escapers } => {
                let v = eval(cmd, dot, sc);
                let (mut s, mut html) = v.text();
                for e in escapers {
                    s = escape(e, &s, html);
                    html = false;
                }
                out.push_str(&s);
            }
            Node::If { cond, then, els } => {
                let depth = sc.vars.len();
                if eval(cond, dot, sc).truth() {
                    exec(then, dot, sc, out);
                } else {
                    exec(els, dot, sc, out);
                }
                sc.vars.truncate(depth);
            }
            Node::Range { over, body } => {
                if let V::List(items) = eval(over, dot, sc) {
                    for item in &items {
                        let depth = sc.vars.len();
                        exec(body, item, sc, out);
                        sc.vars.truncate(depth);
                    }
                }
            }
            Node::Template { name, arg } => {
                let v = eval(arg, dot, sc);
                if let Some(t) = set().0.get(name) {
                    let mut inner = Scope { root: &v, vars: vec![] };
                    exec(t, &v, &mut inner, out);
                }
            }
        }
    }
}

/// `Container.RenderToString(name, data)`.
pub fn render(name: &str, data: &V) -> String {
    let mut out = String::new();
    if let Some(t) = set().0.get(name) {
        let mut sc = Scope { root: data, vars: vec![] };
        exec(t, data, &mut sc, &mut out);
    }
    out
}

/// Go's `htmlReplacementTable`.
pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => o.push('\u{FFFD}'),
            '"' => o.push_str("&#34;"),
            '&' => o.push_str("&amp;"),
            '\'' => o.push_str("&#39;"),
            '+' => o.push_str("&#43;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            c => o.push(c),
        }
    }
    o
}

/// `processURLOnto`, normalizing.
fn url_normalize(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = matches!(
            b,
            b'!' | b'#' | b'$' | b'&' | b'*' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' | b'[' | b']'
        ) || matches!(b, b'-' | b'.' | b'_' | b'~' | b'%')
            || b.is_ascii_alphanumeric();
        if keep {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02x}"));
        }
    }
    o
}

/// `isSafeURL`.
fn safe_url(s: &str) -> bool {
    if let Some(i) = s.find(':')
        && !s[..i].contains('/')
    {
        let scheme = s[..i].to_ascii_lowercase();
        return scheme == "http" || scheme == "https" || scheme == "mailto";
    }
    true
}

/// `cssValueFilter`, for the values the templates hold (colors).
fn css_value(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let bad = s.bytes().any(|b| {
        matches!(
            b,
            0 | b'"'
                | b'\''
                | b'('
                | b')'
                | b'/'
                | b';'
                | b'@'
                | b'['
                | b'\\'
                | b']'
                | b'`'
                | b'{'
                | b'}'
                | b'<'
                | b'>'
        )
    }) || lower.contains("expression")
        || lower.contains("mozbinding");
    if bad { "ZgotmplZ".into() } else { s.to_string() }
}

fn escape(name: &str, s: &str, html: bool) -> String {
    match name {
        "_html_template_htmlescaper" if html => s.to_string(),
        "_html_template_htmlescaper" | "_html_template_attrescaper" | "_html_template_rcdataescaper" => html_escape(s),
        "_html_template_urlfilter" => {
            if safe_url(s) {
                s.to_string()
            } else {
                "#ZgotmplZ".into()
            }
        }
        "_html_template_urlnormalizer" => url_normalize(s),
        "_html_template_cssvaluefilter" => css_value(s),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_escapes() {
        let mut props = BTreeMap::new();
        props.insert("SiteURL".to_string(), V::from("http://x.y/a b"));
        props.insert("Title".to_string(), V::from("It's <b>"));
        props.insert("SupportEmail".to_string(), V::from("a+b@c.d"));
        let mut d = BTreeMap::new();
        d.insert("Props".to_string(), V::Map(props));
        let out = render("password_change_body", &V::Map(d));
        assert!(out.contains("http://x.y/a%20b/static/images/logo-email.png"));
        assert!(out.contains("It&#39;s &lt;b&gt;"));
        assert!(out.contains("mailto:a&#43;b@c.d"));
        assert!(!out.contains("{{"));
    }
}
