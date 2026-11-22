//! Go's `encoding/gob` stream format, without Go types: values are decoded by the types the stream defines, and
//! encoded from types described here ([`Ty`]).
//!
//! Based on Go's encoding/gob, Copyright 2009 The Go Authors, BSD-3-Clause.
//!
//! A gob stream is a sequence of messages, each preceded by its length. A message holds type definitions
//! (negative ids) and then one value (its type id, then the value). The decoder keeps one current message and only
//! reads the next one inside a type sequence; an interface value can therefore continue across messages, with the
//! definitions of its concrete types in between. The encoder sends every definition a value needs in messages of
//! their own before the value, which Go's decoder reads the same way.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

pub const BOOL: i64 = 1;
pub const INT: i64 = 2;
pub const UINT: i64 = 3;
pub const FLOAT: i64 = 4;
pub const BYTES: i64 = 5;
pub const STRING: i64 = 6;
pub const COMPLEX: i64 = 7;
pub const INTERFACE: i64 = 8;
/// Ids below this are Go's predefined types; the encoder numbers its own from here.
const FIRST_USER_ID: i64 = 65;
/// Nesting deeper than this is refused: a value from the other side cannot recurse the decoder off the stack.
const MAX_DEPTH: usize = 64;
/// A message longer than this is refused before it is read.
pub const MAX_MESSAGE: u64 = 1 << 30;

/// A type as this side describes it, to encode values of it.
#[derive(Clone, Debug)]
pub enum Ty {
    Bool,
    Int,
    Uint,
    Float,
    Complex,
    Bytes,
    String,
    Interface,
    Slice(Arc<Ty>),
    Array(Arc<Ty>, usize),
    Map(Arc<Ty>, Arc<Ty>),
    Struct(Arc<StructTy>),
    /// A type that marshals itself (`GobEncoder`, `BinaryMarshaler`, `TextMarshaler`), by its Go name.
    Marshaled(Marshal, Arc<str>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Marshal {
    Gob,
    Binary,
    Text,
}

#[derive(Debug)]
pub struct StructTy {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    /// The struct's shape (name, field names and types), its identity on a connection: a description dropped and
    /// built again must map to the same definition, and a different one to another.
    shape: Arc<str>,
}

impl Ty {
    pub fn slice(elem: Ty) -> Ty {
        Ty::Slice(Arc::new(elem))
    }

    pub fn map(key: Ty, elem: Ty) -> Ty {
        Ty::Map(Arc::new(key), Arc::new(elem))
    }

    pub fn strukt(name: &str, fields: &[(&str, Ty)]) -> Ty {
        let shape = format!(
            "{name}{{{}}}",
            fields.iter().map(|(n, t)| format!("{n} {}", t.shape())).collect::<Vec<_>>().join("; ")
        );
        Ty::Struct(Arc::new(StructTy {
            name: name.into(),
            fields: fields.iter().map(|(n, t)| (n.to_string(), t.clone())).collect(),
            shape: shape.into(),
        }))
    }

    fn shape(&self) -> String {
        match self {
            Ty::Struct(s) => s.shape.to_string(),
            Ty::Slice(e) => format!("[]{}", e.shape()),
            Ty::Array(e, n) => format!("[{n}]{}", e.shape()),
            Ty::Map(k, e) => format!("map[{}]{}", k.shape(), e.shape()),
            t => t.go_name(),
        }
    }

    fn builtin_id(&self) -> Option<i64> {
        Some(match self {
            Ty::Bool => BOOL,
            Ty::Int => INT,
            Ty::Uint => UINT,
            Ty::Float => FLOAT,
            Ty::Complex => COMPLEX,
            Ty::Bytes => BYTES,
            Ty::String => STRING,
            Ty::Interface => INTERFACE,
            _ => return None,
        })
    }

    /// Identity for the set of types a connection has been sent: the structure.
    fn key(&self) -> Key {
        match self {
            Ty::Slice(e) => Key::Slice(Box::new(e.key())),
            Ty::Array(e, n) => Key::Array(Box::new(e.key()), *n),
            Ty::Map(k, e) => Key::Map(Box::new(k.key()), Box::new(e.key())),
            Ty::Struct(s) => Key::Struct(s.shape.clone()),
            Ty::Marshaled(m, n) => Key::Marshaled(*m, n.clone()),
            t => Key::Builtin(t.builtin_id().unwrap_or(0)),
        }
    }

    fn go_name(&self) -> String {
        match self {
            Ty::Bool => "bool".into(),
            Ty::Int => "int".into(),
            Ty::Uint => "uint".into(),
            Ty::Float => "float64".into(),
            Ty::Complex => "complex128".into(),
            Ty::Bytes => "[]uint8".into(),
            Ty::String => "string".into(),
            Ty::Interface => "interface {}".into(),
            Ty::Slice(e) => format!("[]{}", e.go_name()),
            Ty::Array(e, n) => format!("[{n}]{}", e.go_name()),
            Ty::Map(k, e) => format!("map[{}]{}", k.go_name(), e.go_name()),
            Ty::Struct(s) => s.name.clone(),
            Ty::Marshaled(_, n) => n.to_string(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Builtin(i64),
    Slice(Box<Key>),
    Array(Box<Key>, usize),
    Map(Box<Key>, Box<Key>),
    Struct(Arc<str>),
    Marshaled(Marshal, Arc<str>),
}

/// A value. Structs carry the fields that were sent (decoding) or are to be sent (encoding), by Go field name;
/// arrays and slices are both lists.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Complex(f64, f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Struct(Vec<(String, Value)>),
    /// `None`: a nil interface.
    Interface(Option<Box<Iface>>),
    /// The bytes of a type that marshals itself.
    Marshaled(Vec<u8>),
}

/// The concrete value in an interface: its registered Go name, and its type (needed to encode; absent when
/// decoded).
#[derive(Clone, Debug, PartialEq)]
pub struct Iface {
    pub name: String,
    pub ty: Option<Ty>,
    pub value: Value,
}

impl PartialEq for Ty {
    fn eq(&self, other: &Ty) -> bool {
        self.key() == other.key()
    }
}

impl Value {
    pub fn iface(name: &str, ty: Ty, value: Value) -> Value {
        Value::Interface(Some(Box::new(Iface { name: name.into(), ty: Some(ty), value })))
    }

    pub fn strukt(fields: Vec<(&str, Value)>) -> Value {
        Value::Struct(fields.into_iter().map(|(n, v)| (n.to_string(), v)).collect())
    }

    /// A struct's field by Go name.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Value::Struct(f) => f.iter().find(|(n, _)| n == name).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Uint(u) => i64::try_from(*u).ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) | Value::Marshaled(b) => Some(b),
            _ => None,
        }
    }
}

// ---------- primitives ----------

pub fn put_uint(out: &mut Vec<u8>, x: u64) {
    if x < 128 {
        out.push(x as u8);
        return;
    }
    let bytes = x.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    out.push((256 - (8 - skip)) as u8);
    out.extend_from_slice(&bytes[skip..]);
}

pub fn put_int(out: &mut Vec<u8>, i: i64) {
    let u = if i < 0 { (!(i as u64) << 1) | 1 } else { (i as u64) << 1 };
    put_uint(out, u);
}

fn put_float(out: &mut Vec<u8>, f: f64) {
    put_uint(out, f.to_bits().swap_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// One message on the stream: its length, then the bytes.
fn frame(out: &mut Vec<u8>, msg: &[u8]) {
    put_uint(out, msg.len() as u64);
    out.extend_from_slice(msg);
}

/// An unsigned integer from its first byte and a way to read the rest: the length prefix of messages.
pub fn uint_len(first: u8) -> Option<usize> {
    if first < 128 {
        return Some(0);
    }
    let n = 256 - first as usize;
    (n <= 8).then_some(n)
}

// ---------- decoding ----------

/// A definition the other side sent.
#[derive(Clone, Debug)]
enum Wire {
    Array { elem: i64, len: u64 },
    Slice { elem: i64 },
    Struct { fields: Vec<(String, i64)> },
    Map { key: i64, elem: i64 },
    Marshaled,
}

enum DecErr {
    /// The value continues in a message not received yet.
    More,
    Bad(anyhow::Error),
}

type D<T> = std::result::Result<T, DecErr>;

fn bad<T>(msg: impl Into<String>) -> D<T> {
    Err(DecErr::Bad(anyhow!("gob: {}", msg.into())))
}

/// The decoder of one direction of a connection: the definitions received so far, and the messages not decoded
/// yet.
#[derive(Default)]
pub struct Decoder {
    types: HashMap<i64, Wire>,
    msgs: Vec<Vec<u8>>,
}

impl Decoder {
    /// A message read from the stream (without its length).
    pub fn push(&mut self, msg: Vec<u8>) {
        self.msgs.push(msg);
    }

    /// The next value, once the messages pushed hold all of it. The messages it used are dropped, with what is left
    /// of the last one (`DecodeValue` resets its buffer too).
    pub fn next_value(&mut self) -> Result<Option<Value>> {
        let mut d = Dec { types: &mut self.types, msgs: &self.msgs, idx: None, pos: 0, depth: 0 };
        match d.top() {
            Ok(v) => {
                let used = d.idx.map_or(0, |i| i + 1);
                self.msgs.drain(..used);
                Ok(Some(v))
            }
            Err(DecErr::More) => Ok(None),
            Err(DecErr::Bad(e)) => Err(e),
        }
    }
}

struct Dec<'a> {
    types: &'a mut HashMap<i64, Wire>,
    msgs: &'a [Vec<u8>],
    /// The current message, `None` before the first.
    idx: Option<usize>,
    pos: usize,
    depth: usize,
}

impl Dec<'_> {
    fn remaining(&self) -> usize {
        match self.idx {
            Some(i) => self.msgs.get(i).map_or(0, |m| m.len().saturating_sub(self.pos)),
            None => 0,
        }
    }

    fn recv_message(&mut self) -> D<()> {
        let next = self.idx.map_or(0, |i| i + 1);
        if next >= self.msgs.len() {
            return Err(DecErr::More);
        }
        self.idx = Some(next);
        self.pos = 0;
        Ok(())
    }

    fn take(&mut self, n: usize) -> D<&[u8]> {
        if self.remaining() < n {
            return bad("unexpected end of message");
        }
        let Some(m) = self.idx.and_then(|i| self.msgs.get(i)) else { return bad("no message") };
        let s = &m[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn uint(&mut self) -> D<u64> {
        let b = self.take(1)?[0];
        if b < 128 {
            return Ok(b as u64);
        }
        let n = 256 - b as usize;
        if n > 8 {
            return bad("invalid uint");
        }
        Ok(self.take(n)?.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64))
    }

    fn int(&mut self) -> D<i64> {
        let u = self.uint()?;
        Ok(if u & 1 != 0 { !(u >> 1) as i64 } else { (u >> 1) as i64 })
    }

    fn float(&mut self) -> D<f64> {
        Ok(f64::from_bits(self.uint()?.swap_bytes()))
    }

    fn len(&mut self) -> D<usize> {
        let n = self.uint()?;
        if n > self.remaining() as u64 {
            return bad(format!("length {n} exceeds the message"));
        }
        Ok(n as usize)
    }

    fn bytes(&mut self) -> D<Vec<u8>> {
        let n = self.len()?;
        Ok(self.take(n)?.to_vec())
    }

    fn string(&mut self) -> D<String> {
        let b = self.bytes()?;
        Ok(String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    /// `DecodeValue`: a fresh message, its type sequence, then the value.
    fn top(&mut self) -> D<Value> {
        let id = self.type_sequence(false)?;
        self.value(id)
    }

    /// `decodeTypeSequence`: definitions until a value's type id. Inside an interface, the definitions may be
    /// delimited (a length to skip) or continue in the next message.
    fn type_sequence(&mut self, interface: bool) -> D<i64> {
        loop {
            if self.remaining() == 0 {
                self.recv_message()?;
            }
            let id = self.int()?;
            if id >= 0 {
                return Ok(id);
            }
            self.recv_type(id.checked_neg().unwrap_or(0))?;
            if self.remaining() > 0 {
                if !interface {
                    return bad("extra data in buffer");
                }
                self.uint()?;
            }
        }
    }

    /// Reads a struct's fields by number until the terminator.
    fn fields(&mut self, mut f: impl FnMut(&mut Self, i64) -> D<()>) -> D<()> {
        let mut field: i64 = -1;
        loop {
            let delta = self.uint()?;
            if delta == 0 {
                return Ok(());
            }
            field = field.saturating_add(delta.min(i64::MAX as u64) as i64);
            f(self, field)?;
        }
    }

    /// `CommonType`: its name and id (neither is needed here).
    fn common(&mut self) -> D<()> {
        self.fields(|d, f| match f {
            0 => d.string().map(drop),
            1 => d.int().map(drop),
            _ => bad("bad CommonType field"),
        })
    }

    fn recv_type(&mut self, id: i64) -> D<()> {
        if id < FIRST_USER_ID - 1 {
            return bad(format!("type id {id} redefines a predefined type"));
        }
        let mut wire = None;
        self.fields(|d, f| {
            wire = Some(match f {
                0 => {
                    let (mut elem, mut len) = (0, 0);
                    d.fields(|d, f| match f {
                        0 => d.common(),
                        1 => d.int().map(|v| elem = v),
                        2 => d.int().map(|v| len = v.max(0) as u64),
                        _ => bad("bad arrayType field"),
                    })?;
                    Wire::Array { elem, len }
                }
                1 => {
                    let mut elem = 0;
                    d.fields(|d, f| match f {
                        0 => d.common(),
                        1 => d.int().map(|v| elem = v),
                        _ => bad("bad sliceType field"),
                    })?;
                    Wire::Slice { elem }
                }
                2 => {
                    let mut fields = vec![];
                    d.fields(|d, f| match f {
                        0 => d.common(),
                        1 => {
                            let n = d.len()?;
                            for _ in 0..n {
                                let (mut name, mut fid) = (String::new(), 0);
                                d.fields(|d, f| match f {
                                    0 => d.string().map(|s| name = s),
                                    1 => d.int().map(|v| fid = v),
                                    _ => bad("bad fieldType field"),
                                })?;
                                fields.push((name, fid));
                            }
                            Ok(())
                        }
                        _ => bad("bad structType field"),
                    })?;
                    Wire::Struct { fields }
                }
                3 => {
                    let (mut key, mut elem) = (0, 0);
                    d.fields(|d, f| match f {
                        0 => d.common(),
                        1 => d.int().map(|v| key = v),
                        2 => d.int().map(|v| elem = v),
                        _ => bad("bad mapType field"),
                    })?;
                    Wire::Map { key, elem }
                }
                4..=6 => {
                    d.fields(|d, f| if f == 0 { d.common() } else { bad("bad gobEncoderType field") })?;
                    Wire::Marshaled
                }
                _ => return bad("bad wireType field"),
            });
            Ok(())
        })?;
        match wire {
            Some(w) => {
                self.types.insert(id, w);
                Ok(())
            }
            None => bad("empty type definition"),
        }
    }

    /// `decodeValue`: a struct as its fields, anything else as a singleton (a zero delta, then the value).
    fn value(&mut self, id: i64) -> D<Value> {
        if matches!(self.types.get(&id), Some(Wire::Struct { .. })) {
            return self.field(id);
        }
        if self.uint()? != 0 {
            return bad("non-zero delta for singleton");
        }
        self.field(id)
    }

    fn field(&mut self, id: i64) -> D<Value> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return bad("value nested too deep");
        }
        let v = self.field_inner(id);
        self.depth -= 1;
        v
    }

    fn field_inner(&mut self, id: i64) -> D<Value> {
        Ok(match id {
            BOOL => Value::Bool(self.uint()? != 0),
            INT => Value::Int(self.int()?),
            UINT => Value::Uint(self.uint()?),
            FLOAT => Value::Float(self.float()?),
            COMPLEX => Value::Complex(self.float()?, self.float()?),
            BYTES => Value::Bytes(self.bytes()?),
            STRING => Value::String(self.string()?),
            INTERFACE => self.interface()?,
            _ => match self.types.get(&id).cloned() {
                Some(Wire::Struct { fields }) => {
                    let mut out = vec![];
                    self.fields(|d, f| {
                        let Some((name, fid)) = usize::try_from(f).ok().and_then(|f| fields.get(f)) else {
                            return bad("field number out of range");
                        };
                        let v = d.field(*fid)?;
                        out.push((name.clone(), v));
                        Ok(())
                    })?;
                    Value::Struct(out)
                }
                Some(Wire::Slice { elem }) => Value::List(self.list(elem)?),
                Some(Wire::Array { elem, len }) => {
                    let l = self.list(elem)?;
                    if l.len() as u64 != len {
                        return bad("array length mismatch");
                    }
                    Value::List(l)
                }
                Some(Wire::Map { key, elem }) => {
                    let n = self.len()?;
                    let mut out = Vec::with_capacity(n.min(1024));
                    for _ in 0..n {
                        let k = self.field(key)?;
                        let v = self.field(elem)?;
                        out.push((k, v));
                    }
                    Value::Map(out)
                }
                Some(Wire::Marshaled) => Value::Marshaled(self.bytes()?),
                None => return bad(format!("unknown type id {id}")),
            },
        })
    }

    fn list(&mut self, elem: i64) -> D<Vec<Value>> {
        let n = self.len()?;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push(self.field(elem)?);
        }
        Ok(out)
    }

    /// `decodeInterface`: the concrete type's name, its definitions, its id, a length, then the value.
    fn interface(&mut self) -> D<Value> {
        let name = self.string()?;
        if name.is_empty() {
            return Ok(Value::Interface(None));
        }
        let id = self.type_sequence(true)?;
        self.uint()?;
        let value = self.value(id)?;
        Ok(Value::Interface(Some(Box::new(Iface { name, ty: None, value }))))
    }
}

// ---------- encoding ----------

/// The encoder of one direction of a connection: the types it has defined so far.
#[derive(Default)]
pub struct Encoder {
    sent: HashMap<Key, i64>,
    next: i64,
}

impl Encoder {
    /// One `Encode`: the definitions this connection has not had yet, each in a message of its own, then the value.
    pub fn encode(&mut self, ty: &Ty, v: &Value) -> Result<Vec<u8>> {
        let mut defs = vec![];
        self.collect(ty, &mut defs);
        self.collect_value(ty, v, &mut defs)?;
        let mut out = vec![];
        for (id, t) in &defs {
            let mut m = vec![];
            put_int(&mut m, -id);
            self.wire(t, *id, &mut m)?;
            frame(&mut out, &m);
        }
        let mut m = vec![];
        put_int(&mut m, self.id(ty)?);
        self.top(ty, v, &mut m)?;
        frame(&mut out, &m);
        Ok(out)
    }

    fn id(&self, ty: &Ty) -> Result<i64> {
        if let Some(id) = ty.builtin_id() {
            return Ok(id);
        }
        self.sent.get(&ty.key()).copied().ok_or_else(|| anyhow!("gob: type {} not defined", ty.go_name()))
    }

    /// Gives every composite type reachable from `ty` an id, queueing the definitions not sent yet.
    fn collect(&mut self, ty: &Ty, defs: &mut Vec<(i64, Ty)>) {
        if ty.builtin_id().is_some() || self.sent.contains_key(&ty.key()) {
            return;
        }
        if self.next == 0 {
            self.next = FIRST_USER_ID;
        }
        let id = self.next;
        self.next += 1;
        self.sent.insert(ty.key(), id);
        defs.push((id, ty.clone()));
        match ty {
            Ty::Slice(e) | Ty::Array(e, _) => self.collect(e, defs),
            Ty::Map(k, e) => {
                self.collect(k, defs);
                self.collect(e, defs);
            }
            Ty::Struct(s) => {
                for (_, t) in &s.fields {
                    self.collect(t, defs);
                }
            }
            _ => {}
        }
    }

    /// The concrete types inside the interfaces of a value.
    fn collect_value(&mut self, ty: &Ty, v: &Value, defs: &mut Vec<(i64, Ty)>) -> Result<()> {
        match (ty, v) {
            (Ty::Interface, Value::Interface(Some(i))) => {
                let t = i.ty.as_ref().ok_or_else(|| anyhow!("gob: interface value {} has no type", i.name))?;
                self.collect(t, defs);
                self.collect_value(t, &i.value, defs)?;
            }
            (Ty::Slice(e) | Ty::Array(e, _), Value::List(l)) => {
                for x in l {
                    self.collect_value(e, x, defs)?;
                }
            }
            (Ty::Map(kt, et), Value::Map(m)) => {
                for (k, x) in m {
                    self.collect_value(kt, k, defs)?;
                    self.collect_value(et, x, defs)?;
                }
            }
            (Ty::Struct(s), Value::Struct(f)) => {
                for (name, t) in &s.fields {
                    if let Some((_, x)) = f.iter().find(|(n, _)| n == name) {
                        self.collect_value(t, x, defs)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A `wireType` value describing `ty`.
    fn wire(&self, ty: &Ty, id: i64, out: &mut Vec<u8>) -> Result<()> {
        let common = |out: &mut Vec<u8>| {
            put_uint(out, 1);
            put_uint(out, 1);
            put_bytes(out, ty.go_name().as_bytes());
            put_uint(out, 1);
            put_int(out, id);
            put_uint(out, 0);
        };
        match ty {
            Ty::Array(e, n) => {
                put_uint(out, 1);
                common(out);
                put_uint(out, 1);
                put_int(out, self.id(e)?);
                put_uint(out, 1);
                put_int(out, *n as i64);
                put_uint(out, 0);
            }
            Ty::Slice(e) => {
                put_uint(out, 2);
                common(out);
                put_uint(out, 1);
                put_int(out, self.id(e)?);
                put_uint(out, 0);
            }
            Ty::Struct(s) => {
                put_uint(out, 3);
                common(out);
                if !s.fields.is_empty() {
                    put_uint(out, 1);
                    put_uint(out, s.fields.len() as u64);
                    for (name, t) in &s.fields {
                        put_uint(out, 1);
                        put_bytes(out, name.as_bytes());
                        put_uint(out, 1);
                        put_int(out, self.id(t)?);
                        put_uint(out, 0);
                    }
                }
                put_uint(out, 0);
            }
            Ty::Map(k, e) => {
                put_uint(out, 4);
                common(out);
                put_uint(out, 1);
                put_int(out, self.id(k)?);
                put_uint(out, 1);
                put_int(out, self.id(e)?);
                put_uint(out, 0);
            }
            Ty::Marshaled(m, _) => {
                put_uint(
                    out,
                    match m {
                        Marshal::Gob => 5,
                        Marshal::Binary => 6,
                        Marshal::Text => 7,
                    },
                );
                common(out);
                put_uint(out, 0);
            }
            _ => bail!("gob: {} needs no definition", ty.go_name()),
        }
        put_uint(out, 0);
        Ok(())
    }

    /// A top-level value (also the contents of an interface): a struct as its fields, anything else as a singleton.
    fn top(&self, ty: &Ty, v: &Value, out: &mut Vec<u8>) -> Result<()> {
        if !matches!(ty, Ty::Struct(_)) {
            put_uint(out, 0);
        }
        self.field(ty, v, out)
    }

    fn field(&self, ty: &Ty, v: &Value, out: &mut Vec<u8>) -> Result<()> {
        match (ty, v) {
            (Ty::Bool, Value::Bool(b)) => put_uint(out, *b as u64),
            (Ty::Int, Value::Int(i)) => put_int(out, *i),
            (Ty::Int, Value::Uint(u)) => put_int(out, i64::try_from(*u)?),
            (Ty::Uint, Value::Uint(u)) => put_uint(out, *u),
            (Ty::Uint, Value::Int(i)) => put_uint(out, u64::try_from(*i)?),
            (Ty::Float, Value::Float(f)) => put_float(out, *f),
            (Ty::Float, Value::Int(i)) => put_float(out, *i as f64),
            (Ty::Complex, Value::Complex(r, i)) => {
                put_float(out, *r);
                put_float(out, *i);
            }
            (Ty::Bytes, Value::Bytes(b)) => put_bytes(out, b),
            (Ty::String, Value::String(s)) => put_bytes(out, s.as_bytes()),
            (Ty::Slice(e), Value::List(l)) => {
                put_uint(out, l.len() as u64);
                for x in l {
                    self.field(e, x, out)?;
                }
            }
            (Ty::Array(e, n), Value::List(l)) => {
                if l.len() != *n {
                    bail!("gob: array of {n} given {} elements", l.len());
                }
                put_uint(out, l.len() as u64);
                for x in l {
                    self.field(e, x, out)?;
                }
            }
            (Ty::Map(kt, et), Value::Map(m)) => {
                put_uint(out, m.len() as u64);
                for (k, x) in m {
                    self.field(kt, k, out)?;
                    self.field(et, x, out)?;
                }
            }
            (Ty::Struct(s), Value::Struct(f)) => {
                let mut last: i64 = -1;
                for (i, (name, t)) in s.fields.iter().enumerate() {
                    let Some((_, x)) = f.iter().find(|(n, _)| n == name) else { continue };
                    put_uint(out, (i as i64 - last) as u64);
                    last = i as i64;
                    self.field(t, x, out)?;
                }
                if let Some((n, _)) = f.iter().find(|(n, _)| !s.fields.iter().any(|(fname, _)| fname == n)) {
                    bail!("gob: {} has no field {n}", s.name);
                }
                put_uint(out, 0);
            }
            (Ty::Interface, Value::Interface(None)) => put_uint(out, 0),
            (Ty::Interface, Value::Interface(Some(i))) => {
                let t = i.ty.as_ref().ok_or_else(|| anyhow!("gob: interface value {} has no type", i.name))?;
                put_bytes(out, i.name.as_bytes());
                put_int(out, self.id(t)?);
                let mut inner = vec![];
                self.top(t, &i.value, &mut inner)?;
                put_bytes(out, &inner);
            }
            (Ty::Marshaled(..), Value::Marshaled(b) | Value::Bytes(b)) => put_bytes(out, b),
            (t, v) => bail!("gob: cannot encode {v:?} as {}", t.go_name()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8]) -> Vec<Value> {
        let mut dec = Decoder::default();
        let mut out = vec![];
        let mut pos = 0;
        while pos < bytes.len() {
            let n = uint_len(bytes[pos]).unwrap();
            let len = if n == 0 {
                bytes[pos] as usize
            } else {
                bytes[pos + 1..pos + 1 + n].iter().fold(0usize, |a, b| (a << 8) | *b as usize)
            };
            pos += 1 + n;
            dec.push(bytes[pos..pos + len].to_vec());
            pos += len;
            if let Some(v) = dec.next_value().unwrap() {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn uints_and_ints() {
        for (x, want) in [
            (0u64, vec![0u8]),
            (7, vec![7]),
            (256, vec![0xfe, 1, 0]),
            (u64::MAX, {
                let mut v = vec![0xf8];
                v.extend([0xff; 8]);
                v
            }),
        ] {
            let mut b = vec![];
            put_uint(&mut b, x);
            assert_eq!(b, want, "{x}");
        }
        let mut b = vec![];
        put_int(&mut b, -1);
        put_int(&mut b, 1);
        assert_eq!(b, vec![1, 2]);
    }

    /// The example in Go's gob documentation: `Point{22, 33}` with its type definition.
    #[test]
    fn go_doc_point() {
        let ty = Ty::strukt("Point", &[("X", Ty::Int), ("Y", Ty::Int)]);
        let bytes = Encoder::default().encode(&ty, &Value::strukt(vec![("X", Value::Int(22)), ("Y", Value::Int(33))]));
        let bytes = bytes.unwrap();
        let v = decode_all(&bytes);
        assert_eq!(v, vec![Value::strukt(vec![("X", Value::Int(22)), ("Y", Value::Int(33))])]);
        // Go's encoding of the value message: 07 ff 82 01 2c 01 42 00 (id 65 = 0xff82 as int).
        assert!(bytes.ends_with(&[0x07, 0xff, 0x82, 0x01, 0x2c, 0x01, 0x42, 0x00]), "{bytes:02x?}");
    }

    #[test]
    fn round_trip_nested_interfaces() {
        let any_list = Ty::slice(Ty::Interface);
        let any_map = Ty::map(Ty::String, Ty::Interface);
        let post = Ty::strukt(
            "Post",
            &[("Id", Ty::String), ("Props", any_map.clone()), ("FileIds", Ty::slice(Ty::String)), ("Count", Ty::Uint)],
        );
        let att = Value::Map(vec![(
            Value::String("text".into()),
            Value::iface("string", Ty::String, Value::String("hi".into())),
        )]);
        let props = Value::Map(vec![
            (Value::String("from_bot".into()), Value::iface("string", Ty::String, Value::String("true".into()))),
            (
                Value::String("attachments".into()),
                Value::iface(
                    "[]interface {}",
                    any_list.clone(),
                    Value::List(vec![Value::iface("map[string]interface {}", any_map.clone(), att)]),
                ),
            ),
            (Value::String("none".into()), Value::Interface(None)),
        ]);
        let v = Value::strukt(vec![
            ("Id", Value::String("p1".into())),
            ("Props", props),
            ("FileIds", Value::List(vec![Value::String("f".into())])),
            ("Count", Value::Uint(300)),
        ]);
        let mut enc = Encoder::default();
        let mut bytes = enc.encode(&post, &v).unwrap();
        // The second value on the connection reuses the definitions.
        let second = enc.encode(&post, &v).unwrap();
        assert!(second.len() < bytes.len());
        bytes.extend(second);
        let got = decode_all(&bytes);
        assert_eq!(got.len(), 2);
        let strip = |v: &Value| format!("{v:?}").replace("ty: Some(", "ty: (");
        assert_eq!(got[0].get("Id"), Some(&Value::String("p1".into())));
        assert_eq!(got[0].get("Count"), Some(&Value::Uint(300)));
        let props = got[1].get("Props").unwrap();
        assert!(strip(props).contains("\"hi\""), "{props:?}");
    }

    /// Reply types are built per call: one dropped and built again reuses its definition, a different one with the
    /// same name gets its own (keying by address once sent a bool under a []byte definition).
    #[test]
    fn struct_identity_is_its_shape() {
        let mut enc = Encoder::default();
        let bytes_reply = || Ty::strukt("Z_Returns", &[("A", Ty::Bytes)]);
        let x = enc.encode(&bytes_reply(), &Value::strukt(vec![("A", Value::Bytes(vec![1]))])).unwrap();
        let y = enc.encode(&bytes_reply(), &Value::strukt(vec![("A", Value::Bytes(vec![2]))])).unwrap();
        assert!(y.len() < x.len(), "a rebuilt description reuses its definition");
        let bool_reply = Ty::strukt("Z_Returns", &[("A", Ty::Bool)]);
        let z = enc.encode(&bool_reply, &Value::strukt(vec![("A", Value::Bool(true))])).unwrap();
        let mut all = x;
        all.extend(y);
        all.extend(z);
        let got = decode_all(&all);
        assert_eq!(got[1].get("A"), Some(&Value::Bytes(vec![2])));
        assert_eq!(got[2].get("A"), Some(&Value::Bool(true)));
    }

    #[test]
    fn truncated_and_hostile_input_is_an_error() {
        let ty = Ty::strukt("S", &[("A", Ty::slice(Ty::String))]);
        let bytes = Encoder::default().encode(&ty, &Value::strukt(vec![("A", Value::List(vec![]))])).unwrap();
        let mut dec = Decoder::default();
        // A message cut short: the value is incomplete, never a panic.
        dec.push(bytes[1..bytes.len() - 2].to_vec());
        assert!(dec.next_value().is_err() || dec.next_value().unwrap().is_none());
        // A length far beyond the message.
        let mut dec = Decoder::default();
        dec.push(vec![0x0c, 0xf8, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(dec.next_value().is_err());
        // Nesting past the limit.
        let mut deep = vec![];
        put_int(&mut deep, INTERFACE);
        put_uint(&mut deep, 0);
        for _ in 0..200 {
            put_bytes(&mut deep, b"x");
            put_int(&mut deep, INTERFACE);
            put_uint(&mut deep, 1);
            put_uint(&mut deep, 0);
        }
        let mut dec = Decoder::default();
        dec.push(deep);
        assert!(dec.next_value().is_err());
    }
}
