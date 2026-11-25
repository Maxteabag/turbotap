//! JSON helpers with Python semantics.
//!
//! WDA responses stay `serde_json::Value` dicts with exactly the Python shapes, so
//! the ported logic reads the same way:
//! - HTTP >= 400: `{"error": <status int>, "body": "<first 400 chars>"}`
//! - transport failure: `{"error": "<message>"}`
//! - deadline: `{"error": "deadline exceeded", "deadline_exceeded": true}`
use serde_json::{Map, Value};

pub static NULL: Value = Value::Null;

/// `d.get(key)` for any value (non-objects and missing keys give `Null`).
pub fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    match v {
        Value::Object(m) => m.get(key).unwrap_or(&NULL),
        _ => &NULL,
    }
}

/// Python truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `isinstance(r, dict) and r.get("error")` (truthy error).
pub fn has_err(v: &Value) -> bool {
    truthy(get(v, "error"))
}

/// `"error" in r` (key present, even if falsy).
pub fn has_err_key(v: &Value) -> bool {
    matches!(v, Value::Object(m) if m.contains_key("error"))
}

/// `r.get("error") == code` for an integer HTTP status.
pub fn err_is(v: &Value, code: i64) -> bool {
    get(v, "error").as_i64() == Some(code)
}

/// The integer HTTP status in `error`, if any.
pub fn err_code(v: &Value) -> Option<i64> {
    get(v, "error").as_i64()
}

/// `r.get("deadline_exceeded")` truthy.
pub fn deadline_exceeded(v: &Value) -> bool {
    truthy(get(v, "deadline_exceeded"))
}

/// `{"error": msg}`.
pub fn error_value(msg: impl Into<Value>) -> Value {
    let mut m = Map::new();
    m.insert("error".into(), msg.into());
    Value::Object(m)
}

/// `{"error": "deadline exceeded", "deadline_exceeded": true}`.
pub fn deadline_value() -> Value {
    serde_json::json!({"error": "deadline exceeded", "deadline_exceeded": true})
}

/// `{"value": null}`.
pub fn ok_null() -> Value {
    serde_json::json!({"value": null})
}

/// Python `str(v)` for scalar JSON values (`None` -> "None", `True` -> "True").
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => py_number(n),
        other => py_repr(other),
    }
}

fn py_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    py_float(f)
}

/// Python `repr(float)`.
pub fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == f.trunc() && f.abs() < 1e16 {
        return format!("{f:.1}");
    }
    let s = format!("{f}");
    if s.contains('e') || s.contains('.') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Python `repr(s)` for a str: single quotes unless the text has a single quote and no
/// double quote.
pub fn py_repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Python `repr()` of a JSON value (dicts/lists as Python literals).
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
        other => py_str(other),
    }
}

/// `json.dumps(v)` with Python's default separators (`", "`, `": "`) and
/// `ensure_ascii=True`.
pub fn dumps(v: &Value) -> String {
    let mut s = String::new();
    write_value(&mut s, v, true);
    s
}

/// `json.dumps(v, ensure_ascii=False)` with Python's default separators.
pub fn dumps_utf8(v: &Value) -> String {
    let mut s = String::new();
    write_value(&mut s, v, false);
    s
}

fn write_value(out: &mut String, v: &Value, ascii: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap_or(0.0);
                out.push_str(&py_float(f).replace("inf", "Infinity").replace("nan", "NaN"));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_str(out, s, ascii),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, x, ascii);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(out, k, ascii);
                out.push_str(": ");
                write_value(out, x, ascii);
            }
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python `round(x, ndigits)` as a JSON number (banker's rounding is not needed for
/// timings; this rounds half away from zero).
pub fn round_to(x: f64, ndigits: i32) -> f64 {
    let p = 10f64.powi(ndigits);
    (x * p).round() / p
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_matches_python_separators_and_ascii() {
        let v = json!({"ok": true, "t": "ø", "n": [1, 2.5, null]});
        assert_eq!(dumps(&v), "{\"ok\": true, \"t\": \"\\u00f8\", \"n\": [1, 2.5, null]}");
        assert_eq!(dumps_utf8(&v), r#"{"ok": true, "t": "ø", "n": [1, 2.5, null]}"#);
    }

    #[test]
    fn py_repr_quotes_like_python() {
        assert_eq!(py_repr_str("Don't"), "\"Don't\"");
        assert_eq!(py_repr_str("abc"), "'abc'");
        assert_eq!(py_float(1.0), "1.0");
        assert_eq!(py_float(0.65), "0.65");
    }
}
