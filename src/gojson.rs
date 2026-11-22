//! Go's `encoding/json.Unmarshal` into a struct, for inputs whose errors and key matching must be Go's (bulk
//! import lines):
//!
//! - the syntax check first (`checkValid`), with the scanner's messages: `invalid character 'x' looking for
//!   beginning of value`, `unexpected end of JSON input`, ...;
//! - object keys matched to fields exactly, else case-insensitively (`foldName`); unknown keys ignored;
//! - type mismatches as `UnmarshalTypeError` (`json: cannot unmarshal string into Go struct field
//!   UserImportData.user.username of type string`), the first one in document order, after decoding the rest;
//! - `null` clears a pointer, slice or map field and leaves a plain one as it is; a field given twice merges an
//!   object into the struct or map it already holds, anything else is replaced;
//! - strings decoded as Go does (an invalid UTF-8 byte or a lone surrogate is U+FFFD).
//!
//! The result is a `serde_json::Value` with the fields under their JSON names, for serde to take.

// Based on Go's encoding/json (decode.go, scanner.go, fold.go), Copyright 2010 The Go Authors (BSD-3-Clause).

use serde_json::{Map, Value};

// ---------- the scanner (`checkValid`) ----------

#[derive(Clone, Copy, PartialEq)]
enum St {
    BeginValueOrEmpty,
    BeginValue,
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    EndTop,
    InString,
    InStringEsc,
    EscU,
    EscU1,
    EscU12,
    EscU123,
    Neg,
    One,
    Zero,
    Dot,
    Dot0,
    E,
    ESign,
    E0,
    Lit(&'static [u8], usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Parse {
    ObjectKey,
    ObjectValue,
    ArrayValue,
}

const MAX_DEPTH: usize = 10000;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

/// `quoteChar`: the byte as Go's error shows it, `string(c)` being the code point c.
fn quote_char(c: u8) -> String {
    match c {
        b'\'' => r"'\''".into(),
        b'"' => "'\"'".into(),
        _ => {
            let ch = char::from(c);
            let s = match ch {
                '\x07' => r"\a".into(),
                '\x08' => r"\b".into(),
                '\x0c' => r"\f".into(),
                '\n' => r"\n".into(),
                '\r' => r"\r".into(),
                '\t' => r"\t".into(),
                '\x0b' => r"\v".into(),
                '\\' => r"\\".into(),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => format!(r"\x{:02x}", c as u32),
                // strconv.IsPrint: U+0080..U+009F and U+00AD are not printable.
                c if (0x80..=0x9f).contains(&(c as u32)) || c as u32 == 0xad => format!(r"\u{:04x}", c as u32),
                c => c.to_string(),
            };
            format!("'{s}'")
        }
    }
}

struct Scanner {
    step: St,
    stack: Vec<Parse>,
    end_top: bool,
}

impl Scanner {
    fn err(c: u8, context: &str) -> Result<(), String> {
        Err(format!("invalid character {} {context}", quote_char(c)))
    }

    fn push(&mut self, c: u8, p: Parse) -> Result<(), String> {
        self.stack.push(p);
        if self.stack.len() <= MAX_DEPTH { Ok(()) } else { Self::err(c, "exceeded max depth") }
    }

    fn pop(&mut self) {
        self.stack.pop();
        if self.stack.is_empty() {
            self.step = St::EndTop;
            self.end_top = true;
        } else {
            self.step = St::EndValue;
        }
    }

    fn step(&mut self, c: u8) -> Result<(), String> {
        let hex = |c: u8| c.is_ascii_hexdigit();
        match self.step {
            St::BeginValueOrEmpty => {
                if is_space(c) {
                    return Ok(());
                }
                if c == b']' {
                    return self.end_value(c);
                }
                self.begin_value(c)
            }
            St::BeginValue => self.begin_value(c),
            St::BeginStringOrEmpty => {
                if is_space(c) {
                    return Ok(());
                }
                if c == b'}' {
                    *self.stack.last_mut().unwrap() = Parse::ObjectValue;
                    return self.end_value(c);
                }
                self.begin_string(c)
            }
            St::BeginString => self.begin_string(c),
            St::EndValue => self.end_value(c),
            St::EndTop => {
                if !is_space(c) {
                    return Self::err(c, "after top-level value");
                }
                Ok(())
            }
            St::InString => {
                match c {
                    b'"' => self.step = St::EndValue,
                    b'\\' => self.step = St::InStringEsc,
                    c if c < 0x20 => return Self::err(c, "in string literal"),
                    _ => {}
                }
                Ok(())
            }
            St::InStringEsc => match c {
                b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => {
                    self.step = St::InString;
                    Ok(())
                }
                b'u' => {
                    self.step = St::EscU;
                    Ok(())
                }
                _ => Self::err(c, "in string escape code"),
            },
            St::EscU | St::EscU1 | St::EscU12 | St::EscU123 => {
                if !hex(c) {
                    return Self::err(c, "in \\u hexadecimal character escape");
                }
                self.step = match self.step {
                    St::EscU => St::EscU1,
                    St::EscU1 => St::EscU12,
                    St::EscU12 => St::EscU123,
                    _ => St::InString,
                };
                Ok(())
            }
            St::Neg => match c {
                b'0' => {
                    self.step = St::Zero;
                    Ok(())
                }
                b'1'..=b'9' => {
                    self.step = St::One;
                    Ok(())
                }
                _ => Self::err(c, "in numeric literal"),
            },
            St::One if c.is_ascii_digit() => Ok(()),
            St::One | St::Zero => match c {
                b'.' => {
                    self.step = St::Dot;
                    Ok(())
                }
                b'e' | b'E' => {
                    self.step = St::E;
                    Ok(())
                }
                _ => self.end_value(c),
            },
            St::Dot => {
                if c.is_ascii_digit() {
                    self.step = St::Dot0;
                    return Ok(());
                }
                Self::err(c, "after decimal point in numeric literal")
            }
            St::Dot0 => match c {
                b'0'..=b'9' => Ok(()),
                b'e' | b'E' => {
                    self.step = St::E;
                    Ok(())
                }
                _ => self.end_value(c),
            },
            St::E if c == b'+' || c == b'-' => {
                self.step = St::ESign;
                Ok(())
            }
            St::E | St::ESign => {
                if c.is_ascii_digit() {
                    self.step = St::E0;
                    return Ok(());
                }
                Self::err(c, "in exponent of numeric literal")
            }
            St::E0 => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                self.end_value(c)
            }
            St::Lit(word, i) => {
                if c == word[i] {
                    self.step = if i + 1 == word.len() { St::EndValue } else { St::Lit(word, i + 1) };
                    return Ok(());
                }
                let name = std::str::from_utf8(word).unwrap_or("");
                Self::err(c, &format!("in literal {name} (expecting {})", quote_char(word[i])))
            }
        }
    }

    fn begin_value(&mut self, c: u8) -> Result<(), String> {
        if is_space(c) {
            return Ok(());
        }
        match c {
            b'{' => {
                self.step = St::BeginStringOrEmpty;
                self.push(c, Parse::ObjectKey)
            }
            b'[' => {
                self.step = St::BeginValueOrEmpty;
                self.push(c, Parse::ArrayValue)
            }
            b'"' => {
                self.step = St::InString;
                Ok(())
            }
            b'-' => {
                self.step = St::Neg;
                Ok(())
            }
            b'0' => {
                self.step = St::Zero;
                Ok(())
            }
            b't' => {
                self.step = St::Lit(b"true", 1);
                Ok(())
            }
            b'f' => {
                self.step = St::Lit(b"false", 1);
                Ok(())
            }
            b'n' => {
                self.step = St::Lit(b"null", 1);
                Ok(())
            }
            b'1'..=b'9' => {
                self.step = St::One;
                Ok(())
            }
            _ => Self::err(c, "looking for beginning of value"),
        }
    }

    fn begin_string(&mut self, c: u8) -> Result<(), String> {
        if is_space(c) {
            return Ok(());
        }
        if c == b'"' {
            self.step = St::InString;
            return Ok(());
        }
        Self::err(c, "looking for beginning of object key string")
    }

    fn end_value(&mut self, c: u8) -> Result<(), String> {
        let Some(&ps) = self.stack.last() else {
            self.step = St::EndTop;
            self.end_top = true;
            return self.step(c);
        };
        if is_space(c) {
            self.step = St::EndValue;
            return Ok(());
        }
        match ps {
            Parse::ObjectKey => {
                if c == b':' {
                    *self.stack.last_mut().unwrap() = Parse::ObjectValue;
                    self.step = St::BeginValue;
                    return Ok(());
                }
                Self::err(c, "after object key")
            }
            Parse::ObjectValue => {
                if c == b',' {
                    *self.stack.last_mut().unwrap() = Parse::ObjectKey;
                    self.step = St::BeginString;
                    return Ok(());
                }
                if c == b'}' {
                    self.pop();
                    return Ok(());
                }
                Self::err(c, "after object key:value pair")
            }
            Parse::ArrayValue => {
                if c == b',' {
                    self.step = St::BeginValue;
                    return Ok(());
                }
                if c == b']' {
                    self.pop();
                    return Ok(());
                }
                Self::err(c, "after array element")
            }
        }
    }
}

/// `checkValid`: Go's syntax error for the input, if any.
pub fn check_valid(data: &[u8]) -> Result<(), String> {
    let mut s = Scanner { step: St::BeginValue, stack: vec![], end_top: false };
    for &c in data {
        s.step(c)?;
    }
    // `eof`.
    if s.end_top {
        return Ok(());
    }
    s.step(b' ')?;
    if s.end_top {
        return Ok(());
    }
    Err("unexpected end of JSON input".into())
}

// ---------- a parsed value, keys in order ----------

#[derive(Debug, Clone)]
pub enum GVal {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<GVal>),
    Obj(Vec<(String, GVal)>),
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && is_space(self.b[self.i]) {
            self.i += 1;
        }
    }

    fn value(&mut self) -> GVal {
        self.ws();
        match self.b[self.i] {
            b'{' => {
                self.i += 1;
                let mut out = vec![];
                loop {
                    self.ws();
                    if self.b[self.i] == b'}' {
                        self.i += 1;
                        return GVal::Obj(out);
                    }
                    if self.b[self.i] == b',' {
                        self.i += 1;
                        self.ws();
                    }
                    let k = self.string();
                    self.ws();
                    self.i += 1; // ':'
                    let v = self.value();
                    out.push((k, v));
                }
            }
            b'[' => {
                self.i += 1;
                let mut out = vec![];
                loop {
                    self.ws();
                    if self.b[self.i] == b']' {
                        self.i += 1;
                        return GVal::Arr(out);
                    }
                    if self.b[self.i] == b',' {
                        self.i += 1;
                    }
                    out.push(self.value());
                }
            }
            b'"' => GVal::Str(self.string()),
            b't' => {
                self.i += 4;
                GVal::Bool(true)
            }
            b'f' => {
                self.i += 5;
                GVal::Bool(false)
            }
            b'n' => {
                self.i += 4;
                GVal::Null
            }
            _ => {
                let start = self.i;
                while self.i < self.b.len() && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    self.i += 1;
                }
                GVal::Num(String::from_utf8_lossy(&self.b[start..self.i]).into_owned())
            }
        }
    }

    /// `unquote`: escapes resolved, a lone surrogate or an invalid UTF-8 byte as U+FFFD.
    fn string(&mut self) -> String {
        self.i += 1;
        let mut out = String::new();
        let u4 = |b: &[u8]| -> Option<u32> {
            if b.len() < 6 || b[0] != b'\\' || b[1] != b'u' {
                return None;
            }
            u32::from_str_radix(std::str::from_utf8(&b[2..6]).ok()?, 16).ok()
        };
        loop {
            let c = self.b[self.i];
            match c {
                b'"' => {
                    self.i += 1;
                    return out;
                }
                b'\\' => {
                    let e = self.b[self.i + 1];
                    if e == b'u' {
                        let r = u4(&self.b[self.i..]).unwrap_or(0xFFFD);
                        self.i += 6;
                        if (0xD800..0xE000).contains(&r) {
                            if let Some(r2) = u4(&self.b[self.i..])
                                && (0xD800..0xDC00).contains(&r)
                                && (0xDC00..0xE000).contains(&r2)
                            {
                                self.i += 6;
                                out.push(
                                    char::from_u32(0x10000 + ((r - 0xD800) << 10) + (r2 - 0xDC00))
                                        .unwrap_or('\u{FFFD}'),
                                );
                            } else {
                                out.push('\u{FFFD}');
                            }
                        } else {
                            out.push(char::from_u32(r).unwrap_or('\u{FFFD}'));
                        }
                        continue;
                    }
                    out.push(match e {
                        b'b' => '\x08',
                        b'f' => '\x0c',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        other => char::from(other),
                    });
                    self.i += 2;
                }
                c if c < 0x80 => {
                    out.push(char::from(c));
                    self.i += 1;
                }
                _ => {
                    let (ch, n) = crate::extract::decode_rune(&self.b[self.i..]);
                    out.push(ch);
                    self.i += n.max(1);
                }
            }
        }
    }
}

/// The input as a value (after `check_valid`).
pub fn parse(data: &[u8]) -> GVal {
    let mut p = Parser { b: data, i: 0 };
    p.value()
}

/// A value as `interface{}`-free JSON for serde (numbers as written, read back by serde_json).
pub fn to_value(v: &GVal) -> Value {
    match v {
        GVal::Null => Value::Null,
        GVal::Bool(b) => Value::Bool(*b),
        GVal::Num(n) => serde_json::from_str(n).unwrap_or(Value::Null),
        GVal::Str(s) => Value::String(s.clone()),
        GVal::Arr(a) => Value::Array(a.iter().map(to_value).collect()),
        GVal::Obj(o) => {
            let mut m = Map::new();
            for (k, v) in o {
                m.insert(k.clone(), to_value(v));
            }
            Value::Object(m)
        }
    }
}

// ---------- decoding into a described struct ----------

/// A Go field's type, as far as decoding goes. `ptr` is whether `null` clears it.
#[derive(Clone, Copy)]
pub enum Ty {
    /// A string kind, its Go type name (`string`, `model.ChannelType`).
    Str {
        go: &'static str,
        ptr: bool,
    },
    /// An integer kind, its Go type name (`int64`, `int`).
    Int {
        go: &'static str,
    },
    Bool,
    /// `[]string`.
    StrList,
    /// A struct (behind a pointer, or not).
    Struct {
        st: &'static Struct,
        ptr: bool,
    },
    /// A slice of structs, its Go type name (`[]imports.ReactionImportData`); `elem_ptr` for `[]*T`.
    StructList {
        st: &'static Struct,
        go: &'static str,
        elem_ptr: bool,
    },
    /// `map[string]any` under a type name (`model.StringInterface`).
    Map {
        go: &'static str,
    },
    /// `json.RawMessage`: any value as it is.
    Raw,
    /// `time.Time` (its own `UnmarshalJSON`: a string, else its error).
    Time,
}

/// A Go struct: its name (`UserImportData`), package (`imports`) and fields by JSON name.
pub struct Struct {
    pub name: &'static str,
    pub pkg: &'static str,
    pub fields: &'static [(&'static str, Ty)],
}

impl Struct {
    fn go_type(&self) -> String {
        format!("{}.{}", self.pkg, self.name)
    }

    /// The field for an object key: the exact name, else the case-folded one. A field promoted from an embedded
    /// struct is listed as `Embedded.json_name`, as Go's error path shows it.
    fn field(&self, key: &str) -> Option<(&'static str, Ty)> {
        let json = |n: &'static str| n.rsplit('.').next().unwrap_or(n);
        if let Some(f) = self.fields.iter().find(|(n, _)| json(n) == key) {
            return Some(*f);
        }
        let k = fold(key);
        self.fields.iter().find(|(n, _)| fold(json(n)) == k).copied()
    }
}

/// `foldName`: ASCII upper-cased, each other rune as the smallest of its case-folding set (of which only ſ and
/// the Kelvin sign fold into ASCII).
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => c.to_ascii_uppercase(),
            '\u{17F}' => 'S',
            '\u{212A}' => 'K',
            c => c,
        })
        .collect()
}

/// What a JSON value is called in a type error.
fn kind_name(v: &GVal) -> String {
    match v {
        GVal::Null => "null".into(),
        GVal::Bool(_) => "bool".into(),
        GVal::Num(_) => "number".into(),
        GVal::Str(_) => "string".into(),
        GVal::Arr(_) => "array".into(),
        GVal::Obj(_) => "object".into(),
    }
}

struct Ctx {
    first: Option<String>,
    /// `errorContext`: the innermost struct's name and the JSON field names down to the value.
    stack: Vec<&'static str>,
    strct: &'static str,
}

impl Ctx {
    fn type_error(&mut self, value: String, go: &str) {
        if self.first.is_some() {
            return;
        }
        self.first = Some(if self.stack.is_empty() {
            format!("json: cannot unmarshal {value} into Go value of type {go}")
        } else {
            format!(
                "json: cannot unmarshal {value} into Go struct field {}.{} of type {go}",
                self.strct,
                self.stack.join(".")
            )
        });
    }
}

fn decode_struct(st: &'static Struct, v: &GVal, prev: Option<Value>, c: &mut Ctx) -> Option<Value> {
    let GVal::Obj(pairs) = v else {
        c.type_error(kind_name(v), &st.go_type());
        return prev;
    };
    let mut m = match prev {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for (k, fv) in pairs {
        let Some((path, ty)) = st.field(k) else { continue };
        let name = path.rsplit('.').next().unwrap_or(path);
        let saved = (c.stack.len(), c.strct);
        c.stack.push(path);
        c.strct = st.name;
        let old = m.remove(name);
        if let Some(nv) = decode(ty, fv, old, c) {
            m.insert(name.to_string(), nv);
        }
        c.stack.truncate(saved.0);
        c.strct = saved.1;
    }
    Some(Value::Object(m))
}

/// The field's value after decoding `v` into it, given what it held (`None`: nothing, or nil).
fn decode(ty: Ty, v: &GVal, prev: Option<Value>, c: &mut Ctx) -> Option<Value> {
    if let GVal::Null = v {
        return match ty {
            Ty::Str { ptr: false, .. } | Ty::Time => prev,
            Ty::Struct { ptr: false, .. } => prev,
            _ => None,
        };
    }
    match ty {
        Ty::Str { go, .. } => match v {
            GVal::Str(s) => Some(Value::String(s.clone())),
            other => {
                c.type_error(kind_name(other), go);
                prev
            }
        },
        Ty::Int { go } => match v {
            GVal::Num(n) => match n.parse::<i64>() {
                Ok(i) => Some(Value::from(i)),
                Err(_) => {
                    c.type_error(format!("number {n}"), go);
                    prev
                }
            },
            other => {
                c.type_error(kind_name(other), go);
                prev
            }
        },
        Ty::Bool => match v {
            GVal::Bool(b) => Some(Value::Bool(*b)),
            other => {
                c.type_error(kind_name(other), "bool");
                prev
            }
        },
        Ty::StrList => match v {
            GVal::Arr(items) => Some(Value::Array(
                items
                    .iter()
                    .map(|it| match it {
                        GVal::Str(s) => Value::String(s.clone()),
                        GVal::Null => Value::String(String::new()),
                        other => {
                            c.type_error(kind_name(other), "string");
                            Value::String(String::new())
                        }
                    })
                    .collect(),
            )),
            other => {
                c.type_error(kind_name(other), "[]string");
                prev
            }
        },
        Ty::Struct { st, .. } => decode_struct(st, v, prev, c),
        Ty::StructList { st, go, elem_ptr } => match v {
            GVal::Arr(items) => {
                let mut old = match prev {
                    Some(Value::Array(a)) => a.into_iter(),
                    _ => Vec::new().into_iter(),
                };
                let mut out = vec![];
                for it in items {
                    let before = old.next();
                    let elem = match it {
                        // A nil element of `[]*T`, a zero `T` otherwise: an empty object either way here.
                        GVal::Null if elem_ptr => Some(Value::Object(Map::new())),
                        GVal::Null => before.or_else(|| Some(Value::Object(Map::new()))),
                        GVal::Obj(_) => decode_struct(st, it, if elem_ptr { None } else { before }, c),
                        other => {
                            c.type_error(kind_name(other), &st.go_type());
                            None
                        }
                    };
                    out.push(elem.unwrap_or_else(|| Value::Object(Map::new())));
                }
                Some(Value::Array(out))
            }
            other => {
                c.type_error(kind_name(other), go);
                prev
            }
        },
        Ty::Map { go } => match v {
            GVal::Obj(pairs) => {
                let mut m = match prev {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                for (k, fv) in pairs {
                    m.insert(k.clone(), to_value(fv));
                }
                Some(Value::Object(m))
            }
            other => {
                c.type_error(kind_name(other), go);
                prev
            }
        },
        Ty::Raw => Some(to_value(v)),
        Ty::Time => match v {
            GVal::Str(s) => Some(Value::String(s.clone())),
            _ => {
                if c.first.is_none() {
                    c.first = Some("Time.UnmarshalJSON: input is not a JSON string".into());
                }
                prev
            }
        },
    }
}

/// `json.Unmarshal(data, &v)` for a struct described by `st`: the fields as JSON, or Go's error text.
pub fn unmarshal(data: &[u8], st: &'static Struct) -> Result<Value, String> {
    check_valid(data)?;
    let v = parse(data);
    let mut c = Ctx { first: None, stack: vec![], strct: "" };
    let out = decode(Ty::Struct { st, ptr: false }, &v, None, &mut c);
    match c.first {
        Some(e) => Err(e),
        None => Ok(out.unwrap_or_else(|| Value::Object(Map::new()))),
    }
}

/// `json.Unmarshal` of an already parsed value (a `json.RawMessage` field) into `st`.
pub fn unmarshal_value(v: &Value, st: &'static Struct) -> Result<Value, String> {
    unmarshal(v.to_string().as_bytes(), st)
}

#[cfg(test)]
mod tests {
    use super::*;

    static INNER: Struct = Struct { name: "Inner", pkg: "p", fields: &[("name", Ty::Str { go: "string", ptr: true })] };
    static OUTER: Struct = Struct {
        name: "Outer",
        pkg: "p",
        fields: &[
            ("type", Ty::Str { go: "string", ptr: false }),
            ("count", Ty::Int { go: "int64" }),
            ("inner", Ty::Struct { st: &INNER, ptr: true }),
            ("list", Ty::StructList { st: &INNER, go: "[]p.Inner", elem_ptr: false }),
            ("props", Ty::Map { go: "model.StringInterface" }),
        ],
    };

    #[test]
    fn syntax_errors_as_go() {
        let e = |s: &str| check_valid(s.as_bytes()).unwrap_err();
        assert_eq!(e("x"), "invalid character 'x' looking for beginning of value");
        assert_eq!(e("{\"a\" 1}"), "invalid character '1' after object key");
        assert_eq!(e("{\"a\":1 2}"), "invalid character '2' after object key:value pair");
        assert_eq!(e("[1 2]"), "invalid character '2' after array element");
        assert_eq!(e("{1:2}"), "invalid character '1' looking for beginning of object key string");
        assert_eq!(e("{} x"), "invalid character 'x' after top-level value");
        assert_eq!(e("\"a\x01\""), "invalid character '\\x01' in string literal");
        assert_eq!(e("\"\\x\""), "invalid character 'x' in string escape code");
        assert_eq!(e("\"\\u12g4\""), "invalid character 'g' in \\u hexadecimal character escape");
        assert_eq!(e("-a"), "invalid character 'a' in numeric literal");
        assert_eq!(e("1.e"), "invalid character 'e' after decimal point in numeric literal");
        assert_eq!(e("1ea"), "invalid character 'a' in exponent of numeric literal");
        assert_eq!(e("tru "), "invalid character ' ' in literal true (expecting 'e')");
        assert_eq!(e("nul"), "invalid character ' ' in literal null (expecting 'l')");
        assert_eq!(e("{\"a\":"), "unexpected end of JSON input");
        assert_eq!(e(""), "unexpected end of JSON input");
        assert_eq!(e("'"), r"invalid character '\'' looking for beginning of value");
        assert_eq!(e("\u{e9}"), "invalid character 'Ã' looking for beginning of value");
        assert!(check_valid(br#" {"a":[1,-0.5e+3,true,null,"\u00e9"]} "#).is_ok());
    }

    #[test]
    fn keys_fold_and_types_fail_as_go() {
        let v = unmarshal(br#"{"TYPE":"user","Inner":{"NAME":"a"},"inner":{"x":1},"count":3}"#, &OUTER).unwrap();
        assert_eq!(v, serde_json::json!({"type": "user", "inner": {"name": "a"}, "count": 3}));
        let v = unmarshal("{\"prop\u{17F}\":{\"a\":1},\"inner\":null}".as_bytes(), &OUTER).unwrap();
        assert_eq!(v, serde_json::json!({"props": {"a": 1}}));
        let e = |s: &str| unmarshal(s.as_bytes(), &OUTER).unwrap_err();
        assert_eq!(
            e(r#"{"count":1.5}"#),
            "json: cannot unmarshal number 1.5 into Go struct field Outer.count of type int64"
        );
        assert_eq!(
            e(r#"{"inner":{"name":5}}"#),
            "json: cannot unmarshal number into Go struct field Inner.inner.name of type string"
        );
        assert_eq!(
            e(r#"{"list":[{"name":true}]}"#),
            "json: cannot unmarshal bool into Go struct field Inner.list.name of type string"
        );
        assert_eq!(
            e(r#"{"list":"x","count":"y"}"#),
            "json: cannot unmarshal string into Go struct field Outer.list of type []p.Inner"
        );
        assert_eq!(
            e(r#"{"props":[1]}"#),
            "json: cannot unmarshal array into Go struct field Outer.props of type model.StringInterface"
        );
        assert_eq!(e("[1]"), "json: cannot unmarshal array into Go value of type p.Outer");
        let v = unmarshal(br#"{"type":"a","type":null,"props":{"a":1},"PROPS":{"b":"\ud800x"}}"#, &OUTER).unwrap();
        assert_eq!(v, serde_json::json!({"type": "a", "props": {"a": 1, "b": "\u{FFFD}x"}}));
    }
}
