//! The Go structs plugins exchange with the server, as gob types, and the conversions between them and the JSON of
//! this server's models. Field names and types come from `schema_gen.rs`: a struct goes to a plugin
//! with its Go field names, and comes back to JSON with its JSON names.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use goplugin::gob::{Iface, Ty, Value};
use serde_json::{Map, Value as J};

use super::schema_gen::{ALIASES, API, HOOKS, Methods, STRUCTS, StructFields};

/// A Go type expression.
#[derive(Clone, Debug, PartialEq)]
enum Go {
    Bool,
    Int,
    Uint,
    Float,
    String,
    Bytes,
    Any,
    Slice(Box<Go>),
    Map(Box<Go>, Box<Go>),
    Named(String),
}

fn parse(t: &str) -> Go {
    let t = t.trim().trim_start_matches('*');
    if let Some(e) = t.strip_prefix("[]") {
        return if e == "byte" || e == "uint8" { Go::Bytes } else { Go::Slice(Box::new(parse(e))) };
    }
    if let Some(rest) = t.strip_prefix("map[") {
        // map[K]V: K is a plain name here.
        if let Some((k, v)) = rest.split_once(']') {
            return Go::Map(Box::new(parse(k)), Box::new(parse(v)));
        }
    }
    match t {
        "bool" => Go::Bool,
        "int" | "int8" | "int16" | "int32" | "int64" => Go::Int,
        "uint" | "uint8" | "uint16" | "uint32" | "uint64" | "byte" => Go::Uint,
        "float32" | "float64" => Go::Float,
        "string" => Go::String,
        "any" | "interface{}" | "error" => Go::Any,
        "json.RawMessage" | "RawMessage" => Go::Bytes,
        n => match ALIASES.iter().find(|(a, _)| *a == n) {
            Some((_, target)) => parse(target),
            None => Go::Named(n.to_string()),
        },
    }
}

fn fields(name: &str) -> Option<StructFields> {
    static BY_NAME: LazyLock<HashMap<&'static str, StructFields>> = LazyLock::new(|| STRUCTS.iter().copied().collect());
    BY_NAME.get(name).copied()
}

/// The gob type of a Go type expression (`*Post`, `[]string`, `map[string]any`); a struct this side does not know
/// is an empty struct: the plugin keeps its zero value.
pub fn ty(go: &str) -> Ty {
    static CACHE: LazyLock<Mutex<HashMap<String, Ty>>> = LazyLock::new(Mutex::default);
    if let Some(t) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(go) {
        return t.clone();
    }
    let t = go_ty(&parse(go), &mut vec![]);
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(go.to_string(), t.clone());
    t
}

// ---------- API calls and hooks ----------

pub type Fields = &'static [(&'static str, &'static str)];

fn find(table: Methods, name: &str) -> Option<(Fields, Fields)> {
    table.iter().find(|(n, _, _)| *n == name).map(|(_, a, r)| (*a, *r))
}

/// An API method's argument and result fields.
pub fn api(method: &str) -> Option<(Fields, Fields)> {
    find(API, method)
}

/// A hook's argument and result fields.
pub fn hook(name: &str) -> Option<(Fields, Fields)> {
    find(HOOKS, name)
}

/// The arguments of a call (`Z_..Args`), by position.
pub struct Args<'a> {
    fields: Fields,
    v: &'a Value,
}

impl<'a> Args<'a> {
    pub fn new(fields: Fields, v: &'a Value) -> Args<'a> {
        Args { fields, v }
    }
    pub fn raw(&self, i: usize) -> Option<&'a Value> {
        let v = self.fields.get(i).and_then(|(f, _)| self.v.get(f))?;
        match v {
            Value::Interface(None) => None,
            v => Some(v),
        }
    }
    /// The argument as this server's JSON (`null` when nil or absent).
    pub fn json(&self, i: usize) -> J {
        match (self.raw(i), self.fields.get(i)) {
            (Some(v), Some((_, t))) => typed_from_gob(&parse(t), v),
            _ => J::Null,
        }
    }
    pub fn str(&self, i: usize) -> String {
        self.raw(i).and_then(Value::as_str).unwrap_or_default().to_string()
    }
    pub fn bytes(&self, i: usize) -> Option<Vec<u8>> {
        self.raw(i).and_then(Value::as_bytes).map(<[u8]>::to_vec)
    }
    pub fn i64(&self, i: usize) -> i64 {
        match self.raw(i) {
            Some(Value::Uint(u)) => i64::try_from(*u).unwrap_or(i64::MAX),
            Some(v) => v.as_i64().unwrap_or(0),
            None => 0,
        }
    }
    pub fn bool(&self, i: usize) -> bool {
        self.raw(i).and_then(Value::as_bool).unwrap_or(false)
    }
    /// A `[]string` argument.
    pub fn strings(&self, i: usize) -> Vec<String> {
        match self.raw(i) {
            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
            _ => vec![],
        }
    }
}

/// One result of a call.
pub enum Out {
    /// This server's JSON, converted to the result's Go type (`null` leaves it nil).
    J(J),
    /// A gob value as it is (bytes, an `error`).
    Raw(Value),
}

impl From<J> for Out {
    fn from(j: J) -> Out {
        Out::J(j)
    }
}

/// A `Z_..Returns` (or a hook's `Z_..Args`) struct: `outs` by position, converted to the fields' types.
pub fn pack(fields: Fields, outs: Vec<Out>) -> (Ty, Value) {
    let tys: Vec<(&str, Ty)> = fields.iter().map(|(f, t)| (*f, ty(t))).collect();
    let mut vals = vec![];
    for ((f, t), o) in fields.iter().zip(outs) {
        let v = match o {
            Out::Raw(v) => Some(v),
            Out::J(j) => value_to_gob(&parse(t), &j),
        };
        if let Some(v) = v {
            vals.push((f.to_string(), v));
        }
    }
    (Ty::strukt("Z_Returns", &tys), Value::Struct(vals))
}

fn build(name: &str, stack: &mut Vec<String>) -> Ty {
    let Some(fs) = fields(name) else { return Ty::strukt(name, &[]) };
    if stack.iter().any(|s| s == name) {
        // A struct containing itself is only ever received; sent, the inner one stays empty.
        return Ty::strukt(name, &[]);
    }
    stack.push(name.to_string());
    let built: Vec<(&str, Ty)> = fs.iter().map(|(g, _, t)| (*g, go_ty(&parse(t), stack))).collect();
    stack.pop();
    Ty::strukt(name, &built)
}

fn go_ty(g: &Go, stack: &mut Vec<String>) -> Ty {
    match g {
        Go::Bool => Ty::Bool,
        Go::Int => Ty::Int,
        Go::Uint => Ty::Uint,
        Go::Float => Ty::Float,
        Go::String => Ty::String,
        Go::Bytes => Ty::Bytes,
        Go::Any => Ty::Interface,
        Go::Slice(e) => Ty::Slice(Arc::new(go_ty(e, stack))),
        Go::Map(k, e) => Ty::Map(Arc::new(go_ty(k, stack)), Arc::new(go_ty(e, stack))),
        Go::Named(n) => build(n, stack),
    }
}

// ---------- JSON to gob ----------

/// A model struct (this server's JSON, by the struct's JSON names) as the gob value of its Go struct. Fields the
/// JSON lacks, or holds as null, are left out (the zero value, or nil).
pub fn to_gob(name: &str, j: &J) -> Value {
    struct_to_gob(name, j)
}

/// A field's JSON name and whether it is `omitempty`.
fn json_name(js: &str) -> (&str, bool) {
    match js.split_once(',') {
        Some((n, opts)) => (n, opts.split(',').any(|o| o == "omitempty")),
        None => (js, false),
    }
}

fn struct_to_gob(name: &str, j: &J) -> Value {
    let (Some(fs), Some(obj)) = (fields(name), j.as_object()) else { return Value::Struct(vec![]) };
    let mut out = vec![];
    for (go, js, t) in fs {
        let (js, _) = json_name(js);
        if js == "-" {
            continue;
        }
        let Some(v) = obj.get(js) else { continue };
        if let Some(x) = value_to_gob(&parse(t), v) {
            out.push((go.to_string(), x));
        }
    }
    Value::Struct(out)
}

fn value_to_gob(g: &Go, v: &J) -> Option<Value> {
    if v.is_null() {
        return None;
    }
    Some(match g {
        Go::Bool => Value::Bool(v.as_bool().or_else(|| v.as_str().map(|s| s == "true"))?),
        Go::Int => Value::Int(v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?),
        Go::Uint => Value::Uint(v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))?),
        Go::Float => Value::Float(v.as_f64()?),
        Go::String => Value::String(v.as_str()?.to_string()),
        Go::Bytes => Value::Bytes(match v {
            J::String(s) => s.as_bytes().to_vec(),
            other => other.to_string().into_bytes(),
        }),
        Go::Any => any_to_gob(v),
        Go::Slice(e) => Value::List(v.as_array()?.iter().filter_map(|x| value_to_gob(e, x).or(zero(e))).collect()),
        Go::Map(_, e) => Value::Map(
            v.as_object()?
                .iter()
                .filter_map(|(k, x)| value_to_gob(e, x).or(zero(e)).map(|x| (Value::String(k.clone()), x)))
                .collect(),
        ),
        Go::Named(n) => struct_to_gob(n, v),
    })
}

/// What a null element of a list or map becomes: a nil interface or an empty struct.
fn zero(g: &Go) -> Option<Value> {
    match g {
        Go::Any => Some(Value::Interface(None)),
        Go::Named(_) => Some(Value::Struct(vec![])),
        _ => None,
    }
}

/// A JSON value in an `interface{}`, typed as a plugin's JSON decoder types it (as the props of posts read from
/// the database are): numbers are float64, arrays `[]interface {}`, objects `map[string]interface {}`.
pub fn any_to_gob(v: &J) -> Value {
    static LIST: LazyLock<Ty> = LazyLock::new(|| Ty::slice(Ty::Interface));
    static MAP: LazyLock<Ty> = LazyLock::new(|| Ty::map(Ty::String, Ty::Interface));
    match v {
        J::Null => Value::Interface(None),
        J::Bool(b) => Value::iface("bool", Ty::Bool, Value::Bool(*b)),
        J::Number(n) => Value::iface("float64", Ty::Float, Value::Float(n.as_f64().unwrap_or(0.0))),
        J::String(s) => Value::iface("string", Ty::String, Value::String(s.clone())),
        J::Array(a) => Value::iface("[]interface {}", LIST.clone(), Value::List(a.iter().map(any_to_gob).collect())),
        J::Object(o) => Value::iface(
            "map[string]interface {}",
            MAP.clone(),
            Value::Map(o.iter().map(|(k, x)| (Value::String(k.clone()), any_to_gob(x))).collect()),
        ),
    }
}

// ---------- gob to JSON ----------

/// A gob struct from a plugin as this server's JSON for `name`, as the struct is written as JSON: JSON names in
/// its order, and the fields gob left out (zero values) as their zero, unless `omitempty`. Fields this
/// side has no name for get their Go name in snake case.
pub fn from_gob(name: &str, v: &Value) -> J {
    struct_from_gob(Some(name), v)
}

fn struct_from_gob(name: Option<&str>, v: &Value) -> J {
    let Value::Struct(f) = v else { return any_from_gob(v) };
    let fs = name.and_then(fields);
    let mut out = Map::new();
    for (go, js, t) in fs.unwrap_or_default() {
        let (js, omitempty) = json_name(js);
        if js == "-" {
            continue;
        }
        match f.iter().find(|(g, _)| g == go) {
            Some((_, x)) => {
                out.insert(js.to_string(), typed_from_gob(&parse(t), x));
            }
            None if !omitempty => {
                out.insert(js.to_string(), zero_json(t, 0));
            }
            None => {}
        }
    }
    for (go, x) in f {
        if !fs.is_some_and(|fs| fs.iter().any(|(g, _, _)| g == go)) {
            out.insert(snake(go), any_from_gob(x));
        }
    }
    J::Object(out)
}

/// JSON read into a model struct and written back: the struct's fields in order (keys matched ignoring case),
/// missing ones as their zero unless omitempty, empty omitempty ones and unknown keys dropped.
pub fn normalize(name: &str, j: &J) -> J {
    normalize_struct(name, j, 0)
}

fn normalize_struct(name: &str, j: &J, depth: usize) -> J {
    let (Some(fs), Some(obj)) = (fields(name), j.as_object()) else { return j.clone() };
    let mut out = Map::new();
    for (go, js, t) in fs {
        let (js, omitempty) = json_name(js);
        if js == "-" {
            continue;
        }
        let v = obj.get(js).or_else(|| {
            obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(js) || k.eq_ignore_ascii_case(go)).map(|(_, v)| v)
        });
        match v {
            Some(v) if !(omitempty && empty(t, v)) => {
                out.insert(js.to_string(), normalize_typed(&parse(t), v, depth));
            }
            None if !omitempty => {
                out.insert(js.to_string(), zero_json(t, 0));
            }
            _ => {}
        }
    }
    J::Object(out)
}

fn normalize_typed(g: &Go, v: &J, depth: usize) -> J {
    if depth > 16 {
        return v.clone();
    }
    match (g, v) {
        (Go::Named(n), J::Object(_)) => normalize_struct(n, v, depth + 1),
        (Go::Slice(e), J::Array(a)) => J::Array(a.iter().map(|x| normalize_typed(e, x, depth + 1)).collect()),
        (Go::Map(_, e), J::Object(m)) => {
            J::Object(m.iter().map(|(k, x)| (k.clone(), normalize_typed(e, x, depth + 1))).collect())
        }
        _ => v.clone(),
    }
}

/// Whether an omitempty field of type `t` holding `v` is left out: null, false, 0, "", or an empty list or map;
/// a struct, even empty, is kept.
fn empty(t: &str, v: &J) -> bool {
    match v {
        J::Null => true,
        J::Bool(b) => !b,
        J::Number(n) => n.as_f64() == Some(0.0),
        J::String(s) => s.is_empty(),
        J::Array(a) => a.is_empty(),
        J::Object(o) => o.is_empty() && !t.trim().starts_with('*') && !matches!(parse(t), Go::Named(_)),
    }
}

/// The JSON of a Go type's zero value: null for pointers, slices, maps and interfaces, a struct's fields.
fn zero_json(t: &str, depth: usize) -> J {
    let t = t.trim();
    if t.starts_with('*') || depth > 16 {
        return J::Null;
    }
    match parse(t) {
        Go::Bool => J::Bool(false),
        Go::Int | Go::Uint | Go::Float => J::from(0),
        Go::String => J::String(String::new()),
        Go::Bytes | Go::Any | Go::Slice(_) | Go::Map(..) => J::Null,
        Go::Named(n) => match fields(&n) {
            Some(fs) => J::Object(
                fs.iter()
                    .filter_map(|(_, js, t)| {
                        let (js, omitempty) = json_name(js);
                        (js != "-" && !omitempty).then(|| (js.to_string(), zero_json(t, depth + 1)))
                    })
                    .collect(),
            ),
            None => J::Null,
        },
    }
}

fn typed_from_gob(g: &Go, v: &Value) -> J {
    match (g, v) {
        (Go::Named(n), Value::Struct(_)) => struct_from_gob(Some(n), v),
        (Go::Slice(e), Value::List(l)) => J::Array(l.iter().map(|x| typed_from_gob(e, x)).collect()),
        (Go::Map(_, e), Value::Map(m)) => {
            J::Object(m.iter().map(|(k, x)| (key_string(k), typed_from_gob(e, x))).collect())
        }
        _ => any_from_gob(v),
    }
}

/// Any gob value as JSON: interfaces unwrapped (a struct inside one named by its registered Go name, as
/// `[]*model.SlackAttachment`), bytes as text.
pub fn any_from_gob(v: &Value) -> J {
    match v {
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::from(*i),
        Value::Uint(u) => J::from(*u),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Value::Complex(..) => J::Null,
        Value::String(s) => J::String(s.clone()),
        Value::Bytes(b) | Value::Marshaled(b) => J::String(String::from_utf8_lossy(b).into_owned()),
        Value::List(l) => J::Array(l.iter().map(any_from_gob).collect()),
        Value::Map(m) => J::Object(m.iter().map(|(k, x)| (key_string(k), any_from_gob(x))).collect()),
        Value::Struct(_) => struct_from_gob(None, v),
        Value::Interface(None) => J::Null,
        Value::Interface(Some(i)) => iface_from_gob(i),
    }
}

fn iface_from_gob(i: &Iface) -> J {
    // `[]*model.SlackAttachment` is the old name of []*MessageAttachment, kept for older plugins.
    let base = i.name.trim_start_matches("[]").trim_start_matches('*').trim_start_matches("model.");
    let base = if base == "SlackAttachment" { "MessageAttachment" } else { base };
    if fields(base).is_some() {
        let g =
            if i.name.starts_with("[]") { Go::Slice(Box::new(Go::Named(base.into()))) } else { Go::Named(base.into()) };
        return typed_from_gob(&g, &i.value);
    }
    any_from_gob(&i.value)
}

fn key_string(k: &Value) -> String {
    match k {
        Value::String(s) => s.clone(),
        other => match any_from_gob(other) {
            J::String(s) => s,
            j => j.to_string(),
        },
    }
}

/// `UserId` → `user_id`, `ImageURL` → `image_url`.
fn snake(go: &str) -> String {
    let chars: Vec<char> = go.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower = i > 0 && chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_uppercase();
            if i > 0 && (prev_lower || (prev_upper && next_lower)) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use goplugin::gob::Encoder;
    use serde_json::json;

    /// Every struct in the table builds into a gob type a connection can define.
    #[test]
    fn every_type_builds_and_encodes() {
        let mut enc = Encoder::default();
        for (name, _) in STRUCTS {
            let t = ty(name);
            enc.encode(&t, &Value::Struct(vec![])).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn post_both_ways() {
        let post = json!({
            "id": "p1", "create_at": 5, "user_id": "u1", "channel_id": "c1", "message": "hi", "type": "",
            "props": {"from_plugin": "true", "attachments": [{"title": "T", "image_url": "http://x/i.png", "fields": [{"title": "f", "value": 3, "short": true}]}]},
            "file_ids": ["f1"], "is_pinned": false, "metadata": null
        });
        let v = to_gob("Post", &post);
        assert_eq!(v.get("Id"), Some(&Value::String("p1".into())));
        assert_eq!(v.get("CreateAt"), Some(&Value::Int(5)));
        let mut enc = Encoder::default();
        enc.encode(&ty("Post"), &v).unwrap();
        let back = from_gob("Post", &v);
        assert_eq!(back["channel_id"], "c1");
        assert_eq!(back["props"]["attachments"][0]["image_url"], "http://x/i.png");
        assert_eq!(back["props"]["attachments"][0]["fields"][0]["value"], 3.0);
    }

    /// What a plugin sends: attachments as `[]*model.SlackAttachment` structs inside the props interface.
    #[test]
    fn attachments_from_a_plugin() {
        let att = Value::strukt(vec![
            ("Title", Value::String("Connect".into())),
            ("ImageURL", Value::String("http://i".into())),
            ("Timestamp", Value::iface("float64", Ty::Float, Value::Float(1.0))),
        ]);
        let post = Value::strukt(vec![(
            "Props",
            Value::Map(vec![(
                Value::String("attachments".into()),
                Value::Interface(Some(Box::new(Iface {
                    name: "[]*model.SlackAttachment".into(),
                    ty: None,
                    value: Value::List(vec![att]),
                }))),
            )]),
        )]);
        let j = from_gob("Post", &post);
        let a = &j["props"]["attachments"][0];
        assert_eq!((&a["title"], &a["image_url"], &a["ts"]), (&json!("Connect"), &json!("http://i"), &json!(1.0)));
        // What gob leaves out is the zero value, written unless omitempty.
        assert_eq!((&a["fallback"], &a["id"], &a["fields"]), (&json!(""), &json!(0), &J::Null));
        assert!(a.get("actions").is_none());
        assert_eq!(j["file_ids"], J::Null);
        assert!(j.get("message_source").is_none());
    }

    #[test]
    fn config_keeps_go_names() {
        let cfg = json!({"ServiceSettings": {"SiteURL": "http://s", "EnableDeveloper": false}, "FileSettings": {"MaxFileSize": 100}});
        let v = to_gob("Config", &cfg);
        let ss = v.get("ServiceSettings").unwrap();
        assert_eq!(ss.get("SiteURL"), Some(&Value::String("http://s".into())));
        assert_eq!(v.get("FileSettings").and_then(|f| f.get("MaxFileSize")), Some(&Value::Int(100)));
        Encoder::default().encode(&ty("Config"), &v).unwrap();
    }

    /// A manifest as the server writes it back: zero fields filled in, empty omitempty ones and unknown keys gone.
    #[test]
    fn manifest_normalized() {
        let m = json!({"id": "x.y.z", "version": "1.0.0", "description": "", "extra": 1,
            "settings_schema": {"settings": [{"key": "K", "type": "text", "secret": true}]}, "webapp": {}});
        let n = normalize("Manifest", &m);
        assert_eq!(n["name"], "");
        assert!(n.get("description").is_none() && n.get("extra").is_none());
        let s = &n["settings_schema"]["settings"][0];
        assert_eq!((&s["secret"], &s["hosting"], &s["default"]), (&json!(true), &json!(""), &J::Null));
        assert!(s.get("options").is_none());
        assert_eq!(n["webapp"], json!({"bundle_path": ""}));
    }

    #[test]
    fn snake_case() {
        assert_eq!(snake("UserId"), "user_id");
        assert_eq!(snake("ImageURL"), "image_url");
        assert_eq!(snake("SiteURLFoo"), "site_url_foo");
    }
}
