//! On-device Apple Vision OCR via wda-turbo v6 `POST /wda/fastOCR`
//! (v0.10.0 cli.py L5572-5680).
use crate::clock;
use crate::error::{Error, Result};
use crate::json;
use crate::ocr::{Reader, ScreenReader};
use crate::text::{Line, Word};
use crate::wda::WdaClient;
use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Serialize;
use serde_json::{json as j, Value};
use std::borrow::Cow;
use std::fmt;

pub const VISION_MODES: [&str; 2] = ["accurate", "fast"];
pub const VISION_DEFAULT_SCALE: f64 = 0.5;
pub const VISION_BOX_TOLERANCE: f64 = 1e-3;

/// One validated line, box normalised to the screen (0..1, top-left origin).
#[derive(Clone, Debug, PartialEq)]
pub struct VisionLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub confidence: f64,
}

const UNSUPPORTED: &str = "WDA lacks /wda/fastOCR (stock WebDriverAgent or wda-turbo before v6). \
Run `turbotap deploy` to install wda-turbo v6, or use host OCR (read:ocr / turbotap ocr).";

#[derive(Serialize)]
struct FastOcrReq<'a> {
    mode: &'a str,
    capture: &'static str,
    scale: f64,
}

/// Zero-copy borrowed JSON string key.
struct BorrowedStr<'de>(Cow<'de, str>);

impl<'de> Deserialize<'de> for BorrowedStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BorrowedStr<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> std::result::Result<Self::Value, E> {
                Ok(BorrowedStr(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(BorrowedStr(Cow::Owned(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(BorrowedStr(Cow::Owned(v)))
            }
        }
        deserializer.deserialize_str(V)
    }
}

/// Optional JSON string (non-string JSON values are consumed and yield `None`).
#[derive(Default)]
struct OptStr<'de>(Option<Cow<'de, str>>);

impl<'de> Deserialize<'de> for OptStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptStr<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(Some(Cow::Borrowed(v))))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(Some(Cow::Owned(v.to_string()))))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(Some(Cow::Owned(v))))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptStr(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptStr(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Strict JSON number (int/float, NOT bool/string/null) matching Python `type(n) in (int, float)`.
#[derive(Default, Clone, Copy)]
struct OptNum(Option<f64>);

impl<'de> Deserialize<'de> for OptNum {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptNum;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional number")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptNum(Some(v as f64)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptNum(Some(v as f64)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptNum(Some(v)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptNum(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptNum(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptNum(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptNum(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptNum(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Top-level `status` check: `0`, `0.0`, `false` are ok; any other JSON value is not.
struct StatusOk(bool);

impl<'de> Deserialize<'de> for StatusOk {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StatusOk;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("status field")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StatusOk(v == 0))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StatusOk(v == 0))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(StatusOk(v == 0.0))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(StatusOk(!v))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StatusOk(false))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(StatusOk(false))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(StatusOk(false))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(StatusOk(false))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawVisionLine<'de> {
    is_object: bool,
    text: Option<Cow<'de, str>>,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    confidence: Option<f64>,
}

impl<'de> Deserialize<'de> for RawVisionLine<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawVisionLine<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("vision line")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut ln = RawVisionLine { is_object: true, ..Default::default() };
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "text" => ln.text = map.next_value::<OptStr<'de>>()?.0,
                        "x" => ln.x = map.next_value::<OptNum>()?.0,
                        "y" => ln.y = map.next_value::<OptNum>()?.0,
                        "width" => ln.width = map.next_value::<OptNum>()?.0,
                        "height" => ln.height = map.next_value::<OptNum>()?.0,
                        "confidence" => ln.confidence = map.next_value::<OptNum>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(ln)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawVisionLine::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawVisionLine::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawVisionLine::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawVisionLine::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawVisionLine::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawVisionLine::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawVisionLine::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct OptLines<'de>(Option<Vec<RawVisionLine<'de>>>);

impl<'de> Deserialize<'de> for OptLines<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptLines<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("lines array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(32));
                while let Some(item) = seq.next_element::<RawVisionLine<'de>>()? {
                    out.push(item);
                }
                Ok(OptLines(Some(out)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptLines(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptLines(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptLines(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptLines(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptLines(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptLines(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptLines(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawImageMeta<'de> {
    is_object: bool,
    capture: Option<Cow<'de, str>>,
    scale: Option<f64>,
}

impl<'de> Deserialize<'de> for RawImageMeta<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawImageMeta<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("image meta")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut meta = RawImageMeta { is_object: true, ..Default::default() };
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "capture" => meta.capture = map.next_value::<OptStr<'de>>()?.0,
                        "scale" => meta.scale = map.next_value::<OptNum>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(meta)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawImageMeta::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawImageMeta::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawImageMeta::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawImageMeta::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawImageMeta::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawImageMeta::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawImageMeta::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct FastOcrValue<'de> {
    error: Option<Value>,
    message: Option<Value>,
    mode: Option<Cow<'de, str>>,
    lines: Option<Vec<RawVisionLine<'de>>>,
    image: RawImageMeta<'de>,
}

struct OptFastOcrValue<'de>(Option<FastOcrValue<'de>>);

impl<'de> Deserialize<'de> for OptFastOcrValue<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptFastOcrValue<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fastOCR value object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut val = FastOcrValue::default();
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "error" => val.error = Some(map.next_value()?),
                        "message" => val.message = Some(map.next_value()?),
                        "mode" => val.mode = map.next_value::<OptStr<'de>>()?.0,
                        "lines" => val.lines = map.next_value::<OptLines<'de>>()?.0,
                        "image" => val.image = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(OptFastOcrValue(Some(val)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptFastOcrValue(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptFastOcrValue(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptFastOcrValue(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptFastOcrValue(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptFastOcrValue(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptFastOcrValue(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptFastOcrValue(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct FastOcrEnvelope<'de> {
    status_ok: bool,
    value: Option<FastOcrValue<'de>>,
}

impl<'de> Deserialize<'de> for FastOcrEnvelope<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FastOcrEnvelope<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fastOCR response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut status_ok = true;
                let mut value = None;
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "status" => status_ok = map.next_value::<StatusOk>()?.0,
                        "value" => value = map.next_value::<OptFastOcrValue<'de>>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(FastOcrEnvelope { status_ok, value })
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(FastOcrEnvelope::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(FastOcrEnvelope::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(FastOcrEnvelope::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(FastOcrEnvelope::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(FastOcrEnvelope::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(FastOcrEnvelope::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(FastOcrEnvelope::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Replace bare Python `NaN`, `Infinity`, and `-Infinity` tokens outside JSON strings with `null`.
fn sanitize_py_json_nonfinite(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let mut out = String::with_capacity(text.len());
    let (mut in_str, mut esc) = (false, false);
    let mut i = 0;
    let b = text.as_bytes();
    while i < b.len() {
        let ch = b[i];
        if in_str {
            let start = i;
            while i < b.len() {
                let c = b[i];
                if esc {
                    esc = false;
                    i += 1;
                } else if c == b'\\' {
                    esc = true;
                    i += 1;
                } else if c == b'"' {
                    in_str = false;
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push_str(&text[start..i]);
            continue;
        }
        let rest = &text[i..];
        if let Some(tok) = ["-Infinity", "Infinity", "NaN"].iter().find(|t| rest.starts_with(**t)) {
            out.push_str("null");
            i += tok.len();
            continue;
        }
        if ch == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        let len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&text[i..i + len]);
        i += len;
    }
    Some(out)
}

fn validate_fast_ocr_envelope(env: FastOcrEnvelope<'_>, status: u16, mode: &str, scale: f64) -> Result<Vec<VisionLine>> {
    let FastOcrEnvelope { status_ok, value } = env;
    if let Some(vm) = &value {
        if let Some(err) = &vm.error {
            if let Value::String(e) = err {
                if crate::wda::UNSUPPORTED_ROUTE_ERRORS.contains(&e.as_str()) {
                    return Err(Error::VisionUnsupported(UNSUPPORTED.into()));
                }
            }
            let msg = match &vm.message {
                Some(m) if json::truthy(m) => json::py_str(m),
                _ => json::py_str(err),
            };
            if err == &Value::String("invalid argument".into()) {
                return Err(Error::runtime(format!(
                    "WDA rejected the Vision request (older Vision overlay without scaled CGImage capture?): {msg}"
                )));
            }
            return Err(Error::runtime(format!("Vision OCR failed: {msg}")));
        }
    }
    if !(200..300).contains(&status) {
        return Err(Error::runtime(format!("/wda/fastOCR failed (HTTP {status})")));
    }
    let Some(vm) = value else {
        return Err(Error::runtime("Invalid /wda/fastOCR response: expected the requested mode and a lines array"));
    };
    let Some(lines_v) = (if status_ok && vm.mode.as_deref() == Some(mode) { vm.lines } else { None }) else {
        return Err(Error::runtime("Invalid /wda/fastOCR response: expected the requested mode and a lines array"));
    };
    let confirmed = vm.image.is_object
        && vm.image.capture.as_deref() == Some("cgimage")
        && vm.image.scale == Some(scale);
    if !confirmed {
        return Err(Error::runtime("WDA did not confirm the requested scaled CGImage capture; redeploy wda-turbo v6"));
    }
    let tol = VISION_BOX_TOLERANCE;
    let mut lines = Vec::with_capacity(lines_v.len());
    for (i, line) in lines_v.into_iter().enumerate() {
        let text = match line.text {
            Some(t) if line.is_object && !t.trim().is_empty() => t.into_owned(),
            _ => return Err(Error::runtime(format!("Invalid /wda/fastOCR line {i}: expected recognized text"))),
        };
        let fields = [
            ("x", line.x),
            ("y", line.y),
            ("width", line.width),
            ("height", line.height),
            ("confidence", line.confidence),
        ];
        let mut bx = [0f64; 5];
        for (slot, (field, val)) in fields.into_iter().enumerate() {
            match val {
                Some(n) if !n.is_nan() && (-tol..=1.0 + tol).contains(&n) => bx[slot] = n.clamp(0.0, 1.0),
                _ => {
                    return Err(Error::runtime(format!("Invalid /wda/fastOCR line {i}: {field} must be a number in [0, 1]")))
                }
            }
        }
        let [x, y, mut width, mut height, confidence] = bx;
        if width <= 0.0 || height <= 0.0 || x + width > 1.0 + tol || y + height > 1.0 + tol {
            return Err(Error::runtime(format!("Invalid /wda/fastOCR line {i}: expected a nonempty box within the screen")));
        }
        width = width.min(1.0 - x);
        height = height.min(1.0 - y);
        if width <= 0.0 || height <= 0.0 {
            return Err(Error::runtime(format!("Invalid /wda/fastOCR line {i}: box is empty once clipped to the screen")));
        }
        lines.push(VisionLine { text, x, y, width, height, confidence });
    }
    Ok(lines)
}

/// Python `round(x, n)`: correctly rounded from the exact binary value.
fn py_round(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// On-device Apple Vision text recognition via wda-turbo v6 `POST /wda/fastOCR`
/// (L5581-5647).
///
/// Returns validated lines with the box normalised to the screen (top-left origin,
/// 0..1). An empty list means the runner recognised no text; every failure errors
/// instead of pretending the screen is empty. Errors: `Error::VisionUnsupported`,
/// `Error::Runtime`, `Error::Timeout`.
pub fn vision_ocr_raw(client: &mut WdaClient, mode: &str, scale: f64, deadline: Option<f64>) -> Result<Vec<VisionLine>> {
    if !VISION_MODES.contains(&mode) {
        return Err(Error::runtime(format!(
            "Vision mode must be one of {}, not {}",
            VISION_MODES.join(", "),
            json::py_repr_str(mode)
        )));
    }
    if !(scale > 0.0 && scale <= 1.0) {
        return Err(Error::runtime("Vision --scale must be a number greater than zero and at most one"));
    }
    let payload = serde_json::to_vec(&FastOcrReq { mode, capture: "cgimage", scale }).unwrap_or_default();
    let (status, body) = match deadline {
        Some(d) => {
            let rem = d - clock::now();
            if rem <= 0.0 {
                return Err(Error::timeout("Vision OCR deadline exceeded"));
            }
            client.req_post_bytes("/wda/fastOCR", &payload, rem.max(0.05), Some(d))
        }
        None => client.req_post_bytes("/wda/fastOCR", &payload, crate::wda::DEFAULT_TIMEOUT, None),
    };
    if crate::wda::UNSUPPORTED_ROUTE_STATUS.contains(&(status as i64)) {
        return Err(Error::VisionUnsupported(UNSUPPORTED.into()));
    }
    if status == 0 {
        return Err(Error::runtime(format!("Cannot reach WDA for Vision OCR: {}", String::from_utf8_lossy(&body))));
    }
    if let Ok(env) = serde_json::from_slice::<FastOcrEnvelope<'_>>(&body) {
        return validate_fast_ocr_envelope(env, status, mode, scale);
    }
    let Some(sanitized) = sanitize_py_json_nonfinite(&body) else {
        return Err(Error::runtime(format!("Invalid JSON from /wda/fastOCR (HTTP {status})")));
    };
    let Ok(env) = serde_json::from_str::<FastOcrEnvelope<'_>>(&sanitized) else {
        return Err(Error::runtime(format!("Invalid JSON from /wda/fastOCR (HTTP {status})")));
    };
    validate_fast_ocr_envelope(env, status, mode, scale)
}

/// Vision OCR as [`Line`]s in screen points (same shape as `ScreenReader::read`).
pub fn vision_lines(client: &mut WdaClient, screen_pt: (i64, i64), mode: &str, scale: f64, deadline: Option<f64>) -> Result<Vec<Line>> {
    let (w, h) = (screen_pt.0 as f64, screen_pt.1 as f64);
    let mut out: Vec<Line> = vision_ocr_raw(client, mode, scale, deadline)?
        .into_iter()
        .map(|ln| {
            let (x0, y0) = (ln.x * w, ln.y * h);
            let (x1, y1) = (x0 + ln.width * w, y0 + ln.height * h);
            let word = Word { text: ln.text.clone(), x0, y0, x1, y1 };
            Line { text: ln.text, words: vec![word], x0, y0, x1, y1 }
        })
        .collect();
    out.sort_by(|p, q| (p.y0, p.x0).partial_cmp(&(q.y0, q.x0)).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

/// `turbotap vision-ocr` (L5661-5680).
pub fn cmd_vision_ocr(client: &mut WdaClient, mode: &str, scale: f64) -> Result<()> {
    client.ensure_session(None, true)?;
    let mut reader = ScreenReader::new(1.5, None, 11, false);
    let (w, h) = reader.pt_size(client, false, None)?;
    let t0 = clock::real_now();
    let mut raw = vision_ocr_raw(client, mode, scale, None)?;
    let dt = (clock::real_now() - t0) * 1000.0;
    raw.sort_by(|p, q| (p.y, p.x).partial_cmp(&(q.y, q.x)).unwrap_or(std::cmp::Ordering::Equal));
    let lines: Vec<Value> = raw
        .iter()
        .map(|ln| {
            j!({
                "text": ln.text,
                "x": ((ln.x + ln.width / 2.0) * w as f64) as i64,
                "y": ((ln.y + ln.height / 2.0) * h as f64) as i64,
                "confidence": py_round(ln.confidence, 3),
                "box": [py_round(ln.x, 5), py_round(ln.y, 5), py_round(ln.width, 5), py_round(ln.height, 5)],
            })
        })
        .collect();
    crate::outln!(
        "{}",
        json::dumps_utf8(&j!({"ok": true, "engine": "vision", "mode": mode, "scale": scale,
            "screen_pt": [w, h], "lines": lines, "elapsed_ms": py_round(dt, 1)}))
    );
    Ok(())
}
