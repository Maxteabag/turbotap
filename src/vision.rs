//! On-device Apple Vision OCR via wda-turbo v6 `POST /wda/fastOCR`
//! (v0.10.0 cli.py L5572-5680).
use crate::clock;
use crate::error::{Error, Result};
use crate::json;
use crate::ocr::{Reader, ScreenReader};
use crate::text::{Line, Word};
use crate::wda::WdaClient;
use serde_json::{json as j, Value};

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

/// Python `json.loads`, which also accepts the non-standard `NaN`, `Infinity` and
/// `-Infinity` tokens. serde_json rejects them, so on failure they are re-read as
/// `null`: every field check below rejects NaN and null with the same message.
fn py_json_loads(body: &[u8]) -> Option<Value> {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        return Some(v);
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut out = String::with_capacity(text.len());
    let (mut in_str, mut esc) = (false, false);
    let mut i = 0;
    let b = text.as_bytes();
    while i < b.len() {
        let ch = b[i];
        if in_str {
            out.push(ch as char);
            if esc {
                esc = false;
            } else if ch == b'\\' {
                esc = true;
            } else if ch == b'"' {
                in_str = false;
            }
            i += 1;
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
        }
        // Copy the full UTF-8 sequence for non-ASCII bytes outside strings.
        let len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&text[i..i + len]);
        i += len;
    }
    serde_json::from_str::<Value>(&out).ok()
}

/// A JSON number that is not a bool (Python `type(n) in (int, float)`).
fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
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
    let payload = j!({"mode": mode, "capture": "cgimage", "scale": scale});
    let (status, _, body) = match deadline {
        Some(d) => {
            let rem = d - clock::now();
            if rem <= 0.0 {
                return Err(Error::timeout("Vision OCR deadline exceeded"));
            }
            client.req_raw("POST", "/wda/fastOCR", Some(&payload), rem.max(0.05), Some(d), None)
        }
        None => client.req_raw("POST", "/wda/fastOCR", Some(&payload), crate::wda::DEFAULT_TIMEOUT, None, None),
    };
    if crate::wda::UNSUPPORTED_ROUTE_STATUS.contains(&(status as i64)) {
        return Err(Error::VisionUnsupported(UNSUPPORTED.into()));
    }
    if status == 0 {
        return Err(Error::runtime(format!("Cannot reach WDA for Vision OCR: {}", String::from_utf8_lossy(&body))));
    }
    let Some(response) = py_json_loads(&body) else {
        return Err(Error::runtime(format!("Invalid JSON from /wda/fastOCR (HTTP {status})")));
    };
    let value = match &response {
        Value::Object(m) => m.get("value").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    if let Value::Object(vm) = &value {
        if let Some(err) = vm.get("error") {
            if let Value::String(e) = err {
                if crate::wda::UNSUPPORTED_ROUTE_ERRORS.contains(&e.as_str()) {
                    return Err(Error::VisionUnsupported(UNSUPPORTED.into()));
                }
            }
            let msg = match vm.get("message") {
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
    // response.get("status", 0) != 0 (False == 0 in Python)
    let status_ok = match response.get("status") {
        None => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(false)) => true,
        Some(_) => false,
    };
    let lines_v = match &value {
        Value::Object(vm) if status_ok && vm.get("mode").and_then(|m| m.as_str()) == Some(mode) => match vm.get("lines") {
            Some(Value::Array(a)) => Some(a.clone()),
            _ => None,
        },
        _ => None,
    };
    let Some(lines_v) = lines_v else {
        return Err(Error::runtime("Invalid /wda/fastOCR response: expected the requested mode and a lines array"));
    };
    let image = json::get(&value, "image");
    let confirmed = image.is_object()
        && json::get(image, "capture").as_str() == Some("cgimage")
        && num(image.get("scale")) == Some(scale);
    if !confirmed {
        return Err(Error::runtime("WDA did not confirm the requested scaled CGImage capture; redeploy wda-turbo v6"));
    }
    let tol = VISION_BOX_TOLERANCE;
    let mut lines = Vec::with_capacity(lines_v.len());
    for (i, line) in lines_v.iter().enumerate() {
        let text = match line.get("text") {
            Some(Value::String(t)) if line.is_object() && !t.trim().is_empty() => t.clone(),
            _ => return Err(Error::runtime(format!("Invalid /wda/fastOCR line {i}: expected recognized text"))),
        };
        let mut bx = [0f64; 5];
        for (slot, field) in ["x", "y", "width", "height", "confidence"].iter().enumerate() {
            match num(line.get(*field)) {
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

/// Vision OCR as [`Line`]s in screen points (same shape as `ScreenReader::read`).
pub fn vision_lines(client: &mut WdaClient, screen_pt: (i64, i64), mode: &str, scale: f64, deadline: Option<f64>) -> Result<Vec<Line>> {
    let (w, h) = (screen_pt.0 as f64, screen_pt.1 as f64);
    let mut out: Vec<Line> = vision_ocr_raw(client, mode, scale, deadline)?
        .into_iter()
        .map(|ln| {
            let (x0, y0) = (ln.x * w, ln.y * h);
            Line::new(vec![Word::new(&ln.text, x0, y0, x0 + ln.width * w, y0 + ln.height * h)])
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
