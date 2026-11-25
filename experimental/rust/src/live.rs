//! Read-only HTTP observations. Discard all upstream error details at the CLI boundary.
use crate::{Result, control_count, screenshot_bytes};
use reqwest::{
    Url,
    blocking::{Client, Response},
    redirect::Policy,
};
use serde_json::{Value, json};
use std::{
    env,
    io::Read,
    time::{Duration, Instant},
};

const CALCULATOR: &str = "com.apple.calculator";
const MAX_BODY: u64 = 32 << 20;

struct Wda {
    base: Url,
    http: Client,
}

fn calculator(bundle: &Value) -> bool {
    bundle
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(CALCULATOR))
}

fn bounded_body(response: Response) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err("http failure".into());
    }
    let mut bytes = Vec::new();
    response.take(MAX_BODY + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BODY {
        return Err("oversize response".into());
    }
    Ok(bytes)
}

impl Wda {
    fn get(&self, path: &str) -> Result<Value> {
        let bytes = bounded_body(self.http.get(self.base.join(path)?).send()?)?;
        let body: Value = serde_json::from_slice(&bytes)?;
        if !body.is_object() || body.get("error").is_some() || body["value"].get("error").is_some()
        {
            return Err("invalid WDA response".into());
        }
        Ok(body)
    }

    fn guard(&self) -> Result<()> {
        if self.get("/wda/locked")?["value"] != false
            || !calculator(&self.get("/wda/activeAppInfo")?["value"]["bundleId"])
        {
            return Err("guard failed".into());
        }
        Ok(())
    }

    fn turbo_frame(&self) -> Result<(image::GrayImage, Value)> {
        self.guard()?;
        let start = Instant::now();
        let response = self
            .http
            .post(self.base.join("/wda/fastScreenshot")?)
            .header("Content-Type", "application/json")
            .body(r#"{"quality":0.6,"scale":0.5,"raw":true}"#)
            .send()?;
        let mime = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(';').next())
            .unwrap_or("")
            .trim();
        if !mime.eq_ignore_ascii_case("image/jpeg") {
            return Err("expected JPEG".into());
        }
        let bytes = bounded_body(response)?;
        let capture_us = start.elapsed().as_micros() as u64;
        self.guard()?;
        let start = Instant::now();
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Jpeg);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 << 20);
        reader.limits(limits);
        let rgb = reader.decode()?.into_rgb8();
        // Pillow convert("L") uses rounded BT.601 fixed-point coefficients.
        let gray = image::GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
            let [r, g, b] = rgb.get_pixel(x, y).0.map(u32::from);
            image::Luma([((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8])
        });
        let metrics = json!({"bytes":bytes.len(),"width":gray.width(),"height":gray.height(),
            "capture_us":capture_us,"decode_us":start.elapsed().as_micros() as u64});
        Ok((gray, metrics))
    }

    fn turbo(&self, op: &str) -> Result<Value> {
        let (first, mut result) = self.turbo_frame()?;
        match op {
            "turbo-diff" => {
                let (second, metrics) = self.turbo_frame()?;
                if first.dimensions() != second.dimensions() {
                    return Err("size changed".into());
                }
                let start = Instant::now();
                let changed = first
                    .iter()
                    .zip(second.iter())
                    .filter(|(a, b)| a != b)
                    .count();
                result["diff_us"] = json!(start.elapsed().as_micros() as u64);
                result["changed_pixels"] = json!(changed);
                result["second"] = metrics;
            }
            #[cfg(feature = "tess")]
            "turbo-ocr" => result["ocr"] = crate::ocr::measure(&first)?,
            _ => {}
        }
        Ok(result)
    }

    fn observe(&self, op: &str) -> Result<Value> {
        self.guard()?;
        let summary = match op {
            "preflight" => {
                let status = self.get("/status")?;
                let id = status["sessionId"].as_str().ok_or("session required")?;
                if id.is_empty()
                    || !id
                        .bytes()
                        .all(|v| v.is_ascii_alphanumeric() || v == b'-' || v == b'_')
                {
                    return Err("invalid session".into());
                }
                let size = self.get(&format!("/session/{id}/window/size"))?;
                let dim = |key: &str| {
                    size["value"][key]
                        .as_u64()
                        .filter(|v| *v > 0)
                        .ok_or("invalid dimensions")
                };
                json!({"ready":u8::from(status["value"]["ready"].as_bool().ok_or("invalid status")?),
                    "locked":0,"width":dim("width")?,"height":dim("height")?})
            }
            "screenshot" => json!({"bytes":screenshot_bytes(&self.get("/screenshot")?)?.len()}),
            "fastUI" => {
                let ui = self.get("/wda/fastUI")?;
                if !calculator(&ui["bundleId"]) {
                    return Err("snapshot provenance".into());
                }
                control_count(&ui)?
            }
            _ => return Err("unsupported operation".into()),
        };
        self.guard()?;
        Ok(summary)
    }
}

pub fn run(op: &str) -> Result<Value> {
    if !matches!(
        op,
        "preflight" | "screenshot" | "fastUI" | "flow" | "turbo" | "turbo-diff"
    ) && !(cfg!(feature = "tess") && op == "turbo-ocr")
    {
        return Err("unsupported operation".into());
    }
    let base = Url::parse(&env::var("WDA_URL")?)?;
    if base.scheme() != "http"
        || !base.has_host()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("invalid HTTP origin".into());
    }
    let wda = Wda {
        base,
        http: Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(8))
            .build()?,
    };
    if op.starts_with("turbo") {
        return wda.turbo(op);
    }
    if op == "flow" {
        let mut output = serde_json::Map::new();
        for step in ["preflight", "screenshot", "fastUI"] {
            output.insert(step.into(), wda.observe(step)?);
        }
        Ok(Value::Object(output))
    } else {
        wda.observe(op)
    }
}
