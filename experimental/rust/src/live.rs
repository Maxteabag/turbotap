//! Read-only HTTP observations. Discard all upstream error details at the CLI boundary.
use crate::{Result, control_count, screenshot_bytes};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde_json::{Value, json};
use std::{env, io::Read, time::Duration};

const CALCULATOR: &str = "com.apple.calculator";
const MAX_BODY: u64 = 32 << 20;

struct Wda {
    base: Url,
    http: Client,
}

impl Wda {
    fn get(&self, path: &str) -> Result<Value> {
        let response = self.http.get(self.base.join(path)?).send()?;
        if !response.status().is_success() {
            return Err("http failure".into());
        }
        let mut bytes = Vec::new();
        response.take(MAX_BODY + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BODY {
            return Err("oversize response".into());
        }
        let body: Value = serde_json::from_slice(&bytes)?;
        if !body.is_object() || body.get("error").is_some() || body["value"].get("error").is_some()
        {
            return Err("invalid WDA response".into());
        }
        Ok(body)
    }

    fn guard(&self) -> Result<()> {
        if self.get("/wda/locked")?["value"] != false
            || !self.get("/wda/activeAppInfo")?["value"]["bundleId"].as_str().unwrap_or("").eq_ignore_ascii_case(CALCULATOR)
        {
            return Err("guard failed".into());
        }
        Ok(())
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
                if !ui["bundleId"].as_str().unwrap_or("").eq_ignore_ascii_case(CALCULATOR) {
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
    if !matches!(op, "preflight" | "screenshot" | "fastUI" | "flow") {
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
