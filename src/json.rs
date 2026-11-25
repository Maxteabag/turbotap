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
    let mut out = String::with_capacity(24);
    if n.is_f64() {
        write_py_float(&mut out, n.as_f64().unwrap_or(0.0), false);
    } else if let Some(i) = n.as_i64() {
        push_i64(&mut out, i);
    } else if let Some(u) = n.as_u64() {
        push_u64(&mut out, u);
    } else {
        write_py_float(&mut out, n.as_f64().unwrap_or(0.0), false);
    }
    out
}

#[inline]
fn push_u64(out: &mut String, mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    // SAFETY: buf[i..] contains only ASCII digits '0'..='9'.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

#[inline]
fn push_i64(out: &mut String, n: i64) {
    if n < 0 {
        out.push('-');
        push_u64(out, n.wrapping_neg() as u64);
    } else {
        push_u64(out, n as u64);
    }
}

#[inline]
fn write_u_escape(out: &mut String, u: u16) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let buf = [
        b'\\',
        b'u',
        HEX[((u >> 12) & 0xf) as usize],
        HEX[((u >> 8) & 0xf) as usize],
        HEX[((u >> 4) & 0xf) as usize],
        HEX[(u & 0xf) as usize],
    ];
    // SAFETY: all bytes are pure ASCII.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf) });
}

#[inline]
fn write_x_escape(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let buf = [b'\\', b'x', HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]];
    // SAFETY: all bytes are pure ASCII.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf) });
}

fn write_py_float(out: &mut String, f: f64, json_specials: bool) {
    use std::fmt::Write;
    if f.is_nan() {
        out.push_str(if json_specials { "NaN" } else { "nan" });
        return;
    }
    if f.is_infinite() {
        out.push_str(match (f > 0.0, json_specials) {
            (true, true) => "Infinity",
            (false, true) => "-Infinity",
            (true, false) => "inf",
            (false, false) => "-inf",
        });
        return;
    }
    if f == f.trunc() && f.abs() < 1e16 {
        let _ = write!(out, "{f:.1}");
        return;
    }
    let start = out.len();
    let _ = write!(out, "{f}");
    if !out.as_bytes()[start..].iter().any(|&b| b == b'e' || b == b'.') {
        out.push_str(".0");
    }
}

/// Python `repr(float)`.
pub fn py_float(f: f64) -> String {
    let mut out = String::with_capacity(24);
    write_py_float(&mut out, f, false);
    out
}

/// Python `repr(s)` for a str: single quotes unless the text has a single quote and no
/// double quote.
pub fn py_repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { b'"' } else { b'\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q as char);
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b >= 0x20 && b != 0x7f && b != b'\\' && b != q {
            i += 1;
            continue;
        }
        if start < i {
            out.push_str(&s[start..i]);
        }
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(q as char);
            }
            c => write_x_escape(&mut out, c),
        }
        i += 1;
        start = i;
    }
    if start < bytes.len() {
        out.push_str(&s[start..]);
    }
    out.push(q as char);
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

fn estimate_json_cap(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 8,
        Value::Number(_) => 16,
        Value::String(s) => s.len() + 2,
        Value::Array(a) => {
            let per = a.first().map_or(8, |x| match x {
                Value::Object(m) => m.len() * 24 + 8,
                Value::String(s) => s.len() + 4,
                _ => 16,
            });
            2 + a.len().saturating_mul(per)
        }
        Value::Object(m) => {
            let mut cap = 2 + m.len() * 24;
            for val in m.values() {
                match val {
                    Value::String(s) => cap = cap.saturating_add(s.len()),
                    Value::Array(a) => {
                        let per = a.first().map_or(8, |x| match x {
                            Value::Object(im) => im.len() * 24 + 8,
                            Value::String(s) => s.len() + 4,
                            _ => 16,
                        });
                        cap = cap.saturating_add(a.len().saturating_mul(per));
                    }
                    Value::Object(im) => cap = cap.saturating_add(im.len() * 24),
                    _ => {}
                }
            }
            cap
        }
    }
}

/// `json.dumps(v)` with Python's default separators (`", "`, `": "`) and
/// `ensure_ascii=True`.
pub fn dumps(v: &Value) -> String {
    let mut s = String::with_capacity(estimate_json_cap(v));
    write_value::<true>(&mut s, v);
    s
}

/// `json.dumps(m)` for an object map without cloning it into `Value::Object`.
pub fn dumps_map(m: &Map<String, Value>) -> String {
    let mut cap = 2 + m.len() * 24;
    for val in m.values() {
        if let Value::String(s) = val {
            cap = cap.saturating_add(s.len());
        }
    }
    let mut s = String::with_capacity(cap);
    write_map::<true>(&mut s, m);
    s
}

/// `json.dumps(v, ensure_ascii=False)` with Python's default separators.
pub fn dumps_utf8(v: &Value) -> String {
    let mut s = String::with_capacity(estimate_json_cap(v));
    write_value::<false>(&mut s, v);
    s
}

fn write_map<const ASCII: bool>(out: &mut String, m: &Map<String, Value>) {
    out.push('{');
    for (i, (k, x)) in m.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_str::<ASCII>(out, k);
        out.push_str(": ");
        write_value::<ASCII>(out, x);
    }
    out.push('}');
}

fn write_value<const ASCII: bool>(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                write_py_float(out, n.as_f64().unwrap_or(0.0), true);
            } else if let Some(i) = n.as_i64() {
                push_i64(out, i);
            } else if let Some(u) = n.as_u64() {
                push_u64(out, u);
            } else {
                write_py_float(out, n.as_f64().unwrap_or(0.0), true);
            }
        }
        Value::String(s) => write_str::<ASCII>(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value::<ASCII>(out, x);
            }
            out.push(']');
        }
        Value::Object(m) => write_map::<ASCII>(out, m),
    }
}

fn write_str<const ASCII: bool>(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let safe = if ASCII {
            (0x20..=0x7e).contains(&b) && b != b'"' && b != b'\\'
        } else {
            b >= 0x20 && b != b'"' && b != b'\\'
        };
        if safe {
            i += 1;
            continue;
        }
        if start < i {
            out.push_str(&s[start..i]);
        }
        match b {
            b'"' => {
                out.push_str("\\\"");
                i += 1;
            }
            b'\\' => {
                out.push_str("\\\\");
                i += 1;
            }
            b'\n' => {
                out.push_str("\\n");
                i += 1;
            }
            b'\r' => {
                out.push_str("\\r");
                i += 1;
            }
            b'\t' => {
                out.push_str("\\t");
                i += 1;
            }
            0x08 => {
                out.push_str("\\b");
                i += 1;
            }
            0x0c => {
                out.push_str("\\f");
                i += 1;
            }
            0x00..=0x1f | 0x7f => {
                write_u_escape(out, b as u16);
                i += 1;
            }
            _ => {
                let ch = s[i..].chars().next().unwrap();
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    write_u_escape(out, *unit);
                }
                i += ch.len_utf8();
            }
        }
        start = i;
    }
    if start < bytes.len() {
        out.push_str(&s[start..]);
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

    #[test]
    fn dumps_handles_escapes_surrogates_and_numbers() {
        let v = json!({
            "ctrl": "\u{0000}\u{0008}\u{000c}\n\r\t\u{001f}\u{007f}",
            "quote": "\"\\",
            "emoji": "🚀",
            "ints": [-9223372036854775808i64, -1, 0, 42, 18446744073709551615u64],
            "floats": [0.0, 1.0, -2.5, 1e20],
        });
        let ascii = dumps(&v);
        assert!(ascii.contains(r#""ctrl": "\u0000\b\f\n\r\t\u001f\u007f""#), "{ascii}");
        assert!(ascii.contains(r#""quote": "\"\\""#), "{ascii}");
        assert!(ascii.contains(r#""emoji": "\ud83d\ude80""#), "{ascii}");
        assert!(ascii.contains("-9223372036854775808"), "{ascii}");
        assert!(ascii.contains("18446744073709551615"), "{ascii}");
        assert!(ascii.contains("0.0, 1.0, -2.5"), "{ascii}");
        // dumps_utf8 keeps UTF-8 and 0x7f unescaped like Python ensure_ascii=False.
        let utf8 = dumps_utf8(&v);
        assert!(utf8.contains("\"emoji\": \"🚀\""), "{utf8}");
        assert!(utf8.contains("\u{007f}"), "{utf8}");
        // Round-trips cleanly via serde_json.
        let rt: Value = serde_json::from_str(&ascii).unwrap();
        assert_eq!(rt["emoji"], json!("🚀"));
        assert_eq!(rt["quote"], json!("\"\\"));
    }
}
