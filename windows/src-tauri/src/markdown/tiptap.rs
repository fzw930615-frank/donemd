//! Tiptap (ProseMirror) node helpers over `serde_json::Value`.
//!
//! The Swift side has a typed `TiptapNode`; the port keeps nodes as plain
//! JSON values — the bridge speaks JSON end-to-end, and the editor validates
//! shape anyway. These constructors keep the conversion/serializer code
//! readable and the field-omission contract (`None` ⇒ key absent, never
//! `null`) in one place.

use serde_json::{json, Map, Value};

pub fn node(kind: &str) -> Value {
    json!({ "type": kind })
}

pub fn node_with_content(kind: &str, content: Vec<Value>) -> Value {
    json!({ "type": kind, "content": content })
}

pub fn text(text: &str, marks: Option<Vec<Value>>) -> Value {
    let mut v = json!({ "type": "text", "text": text });
    if let Some(m) = marks.filter(|m| !m.is_empty()) {
        v["marks"] = Value::Array(m);
    }
    v
}

pub fn mark(kind: &str, attrs: Option<Map<String, Value>>) -> Value {
    let mut m = json!({ "type": kind });
    if let Some(a) = attrs {
        m["attrs"] = Value::Object(a);
    }
    m
}

pub fn set_attrs(node: &mut Value, attrs: Map<String, Value>) {
    if !attrs.is_empty() {
        node["attrs"] = Value::Object(attrs);
    }
}

pub fn attr_str<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get("attrs")?.get(key)?.as_str()
}

pub fn attr_i64(node: &Value, key: &str) -> Option<i64> {
    node.get("attrs")?.get(key)?.as_i64()
}

pub fn attr_bool(node: &Value, key: &str) -> Option<bool> {
    node.get("attrs")?.get(key)?.as_bool()
}

pub fn content(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub fn node_type(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

/// `true` when two mark lists are structurally identical (used by the
/// adjacent-text coalescer; mirrors Swift's `TiptapMark ==`).
pub fn marks_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}
