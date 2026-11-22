//! A config as Go marshals its `model.Config` struct (the config file and database, the answers of `PUT` and `PATCH
//! /config`): the settings in the struct's field order (the order of `data/config_defaults.json`, made from the Go
//! model), not sorted, and strings escaped as encoding/json escapes them. Keys the struct does not have (maps such as
//! `PluginSettings.Plugins`) are sorted, as Go sorts map keys.

// Based on Mattermost server/public/model/config.go (the Config struct's field order), Copyright (c) 2015-present
// Mattermost, Inc. Licensed under AGPL-3.0.

use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// The keys of an object in file order, each with the order of its own object value.
#[derive(Default)]
pub struct KeyOrder(Vec<(String, KeyOrder)>);

impl<'de> Deserialize<'de> for KeyOrder {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<KeyOrder, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = KeyOrder;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<KeyOrder, A::Error> {
                let mut keys = vec![];
                while let Some(k) = m.next_key::<String>()? {
                    keys.push((k, m.next_value::<KeyOrder>()?));
                }
                Ok(KeyOrder(keys))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<KeyOrder, A::Error> {
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(KeyOrder::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
            fn visit_unit<E>(self) -> Result<KeyOrder, E> {
                Ok(KeyOrder::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// The field order of `model.Config`.
pub fn config_order() -> &'static KeyOrder {
    static O: std::sync::OnceLock<KeyOrder> = std::sync::OnceLock::new();
    O.get_or_init(|| serde_json::from_str(include_str!("../data/config_defaults.json")).expect("config defaults"))
}

/// `json.Marshal` (or `MarshalIndent` with four spaces) of a config.
pub fn to_go_json(v: &Value, pretty: bool) -> String {
    let mut out = String::new();
    write(v, Some(config_order()), pretty, 0, &mut out);
    out
}

fn write(v: &Value, order: Option<&KeyOrder>, pretty: bool, depth: usize, out: &mut String) {
    let nl = |out: &mut String, d: usize| {
        if pretty {
            out.push('\n');
            out.push_str(&"    ".repeat(d));
        }
    };
    match v {
        Value::Object(m) if !m.is_empty() => {
            let mut keys: Vec<(&String, Option<&KeyOrder>)> = vec![];
            if let Some(o) = order {
                for (k, sub) in &o.0 {
                    if let Some((key, _)) = m.get_key_value(k) {
                        keys.push((key, Some(sub)));
                    }
                }
            }
            let mut rest: Vec<&String> = m.keys().filter(|k| !keys.iter().any(|(x, _)| x == k)).collect();
            rest.sort();
            keys.extend(rest.into_iter().map(|k| (k, None)));
            out.push('{');
            for (i, (k, sub)) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, depth + 1);
                go_string(k, out);
                out.push(':');
                if pretty {
                    out.push(' ');
                }
                write(&m[*k], *sub, pretty, depth + 1, out);
            }
            nl(out, depth);
            out.push('}');
        }
        Value::Array(a) if !a.is_empty() => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, depth + 1);
                write(x, None, pretty, depth + 1, out);
            }
            nl(out, depth);
            out.push(']');
        }
        Value::String(s) => go_string(s, out),
        v => out.push_str(&serde_json::to_string(v).unwrap()),
    }
}

/// encoding/json's string: HTML characters and the line and paragraph separators escaped too.
fn go_string(s: &str, out: &mut String) {
    for ch in serde_json::to_string(s).unwrap().chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn go_order() {
        let v = serde_json::json!({"TeamSettings": {"SiteName": "x"}, "ServiceSettings": {"ListenAddress": ":1", "SiteURL": ""}, "Zzz": {"b": 1, "a": []}});
        assert_eq!(
            super::to_go_json(&v, false),
            r#"{"ServiceSettings":{"SiteURL":"","ListenAddress":":1"},"TeamSettings":{"SiteName":"x"},"Zzz":{"a":[],"b":1}}"#
        );
        assert_eq!(
            super::to_go_json(&serde_json::json!({"a": "<&>\u{2028}"}), false),
            "{\"a\":\"\\u003c\\u0026\\u003e\\u2028\"}"
        );
        assert!(super::to_go_json(&v, true).starts_with("{\n    \"ServiceSettings\": {\n        \"SiteURL\": \"\","));
    }
}
