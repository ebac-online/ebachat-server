//! Gob streams written by Go itself (`data/gen_post_props.go.txt`): a post whose props nest interfaces three deep
//! and carry attachments, as a plugin's `CreatePost` does. Go defines the concrete types of nested interfaces in the
//! middle of the value, across messages; the second value on the stream reuses the definitions.

use goplugin::gob::{Encoder, Ty, Value};
use goplugin::rpc::GobReader;

fn prop<'a>(post: &'a Value, key: &str) -> &'a Value {
    let Some(Value::Map(m)) = post.get("Props") else { panic!("no props in {post:?}") };
    let v = m.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v).unwrap_or_else(|| panic!("no {key}"));
    match v {
        Value::Interface(Some(i)) => &i.value,
        v => v,
    }
}

fn unwrap(v: &Value) -> &Value {
    match v {
        Value::Interface(Some(i)) => &i.value,
        v => v,
    }
}

#[tokio::test]
async fn decodes_what_go_writes() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/post_props.gob")).unwrap();
    let mut r = GobReader::new(&bytes[..]);
    for id in ["p1", "p2"] {
        let post = r.next().await.unwrap();
        assert_eq!(post.get("Id"), Some(&Value::String(id.into())));
        assert_eq!(post.get("CreateAt"), Some(&Value::Int(1_700_000_000_000)));
        assert_eq!(prop(&post, "from_webhook"), &Value::String("true".into()));
        assert_eq!(prop(&post, "count"), &Value::Int(3));
        // nested: []any{map[string]any{"a": []any{1.5, "x", map[string]any{"deep": true}}}}
        let Value::List(outer) = prop(&post, "nested") else { panic!() };
        let Value::Map(m) = unwrap(&outer[0]) else { panic!("{:?}", outer[0]) };
        let Value::List(a) = unwrap(&m[0].1) else { panic!() };
        assert_eq!(unwrap(&a[0]), &Value::Float(1.5));
        assert_eq!(unwrap(&a[1]), &Value::String("x".into()));
        let Value::Map(deep) = unwrap(&a[2]) else { panic!() };
        assert_eq!(unwrap(&deep[0].1), &Value::Bool(true));
        // attachments: []*model.SlackAttachment with a field holding an interface.
        let Value::List(atts) = prop(&post, "attachments") else { panic!() };
        assert_eq!(atts[0].get("Text"), Some(&Value::String("hello".into())));
        let Some(Value::List(fields)) = atts[0].get("Fields") else { panic!() };
        assert_eq!(fields[1].get("Value").map(unwrap), Some(&Value::Int(42)));
    }
}

/// What this side writes for Go to read: `GOB_OUT=<file> cargo test ... encodes_for_go`, then
/// `go run gen_post_props.go decode < <file>` prints both posts.
#[test]
fn encodes_for_go() {
    let any_list = Ty::slice(Ty::Interface);
    let any_map = Ty::map(Ty::String, Ty::Interface);
    let field = Ty::strukt("Field", &[("Title", Ty::String), ("Value", Ty::Interface), ("Short", Ty::Bool)]);
    let att = Ty::strukt("Attachment", &[("Text", Ty::String), ("Color", Ty::String), ("Fields", Ty::slice(field))]);
    let post = Ty::strukt(
        "Post",
        &[("Id", Ty::String), ("CreateAt", Ty::Int), ("FileIds", Ty::slice(Ty::String)), ("Props", any_map.clone())],
    );
    let s = |v: &str| Value::String(v.into());
    let iface = |n: &str, t: Ty, v: Value| Value::iface(n, t, v);
    let props = Value::Map(vec![
        (s("from_webhook"), iface("string", Ty::String, s("true"))),
        (
            s("nested"),
            iface(
                "[]interface {}",
                any_list.clone(),
                Value::List(vec![iface(
                    "map[string]interface {}",
                    any_map.clone(),
                    Value::Map(vec![(s("deep"), iface("bool", Ty::Bool, Value::Bool(true)))]),
                )]),
            ),
        ),
        (
            s("attachments"),
            iface(
                "[]*model.SlackAttachment",
                Ty::slice(att),
                Value::List(vec![Value::strukt(vec![
                    ("Text", s("hello")),
                    (
                        "Fields",
                        Value::List(vec![Value::strukt(vec![
                            ("Title", s("n")),
                            ("Value", iface("int", Ty::Int, Value::Int(42))),
                        ])]),
                    ),
                ])]),
            ),
        ),
    ]);
    let mut enc = Encoder::default();
    let mut out = vec![];
    for id in ["r1", "r2"] {
        let v = Value::strukt(vec![
            ("Id", s(id)),
            ("CreateAt", Value::Int(1_700_000_000_000)),
            ("FileIds", Value::List(vec![s("f1")])),
            ("Props", props.clone()),
        ]);
        out.extend(enc.encode(&post, &v).unwrap());
    }
    if let Some(path) = std::env::var_os("GOB_OUT") {
        std::fs::write(path, &out).unwrap();
    }
}
