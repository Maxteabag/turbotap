use base64::{Engine, engine::general_purpose::STANDARD};
use image::GrayImage;
use serde_json::{Value, json};
use std::{collections::HashSet, env, error::Error, fs, io::Write, path::Path, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const USAGE: &str = "turbotap-replay cold | <preflight|screenshot|decode|cuts|fastUI|diff|ocr|flow> FIXTURE_DIR [--raw (screenshot only)]";

fn read_json(dir: &Path, name: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(dir.join(name))?)?)
}

fn screenshot(dir: &Path) -> Result<Vec<u8>> {
    let reply = read_json(dir, "screenshot.json")?;
    let bytes = STANDARD.decode(reply["value"].as_str().ok_or("missing screenshot")?)?;
    if bytes.is_empty() {
        return Err("empty screenshot".into());
    }
    Ok(bytes)
}

fn frame(dir: &Path, name: &str) -> Result<GrayImage> {
    Ok(image::load_from_memory(&fs::read(dir.join(name))?)?.into_luma8())
}

// Port of ScreenReader._cuts with four requested strips.
fn cuts(img: &GrayImage) -> Vec<u32> {
    let (w, h) = img.dimensions();
    let n = 4.min(h / 90);
    let mut rows = vec![0];
    for i in 1..n {
        let target = h * i / n;
        let half = 48.min(h / (2 * n));
        let lo = (rows[i as usize - 1] + 24).max(target - half);
        let hi = (h - 24).min(target + half);
        let mut best = (256u16, target);
        'search: for distance in 0..=half {
            for y in [target - distance, target + distance] {
                if !(lo..hi).contains(&y) {
                    continue;
                }
                let start = y as usize * w as usize;
                let row = &img.as_raw()[start..start + w as usize];
                let (mut min, mut max) = (255, 0);
                for &v in row.iter().step_by(if w >= 240 { 4 } else { 2 }) {
                    min = min.min(v);
                    max = max.max(v);
                }
                let score = u16::from(max - min);
                if score < best.0 {
                    best = (score, y);
                }
                if score <= 2 {
                    break 'search;
                }
            }
        }
        rows.push(best.1);
    }
    rows.push(h);
    rows
}

fn operation(dir: &Path, op: &str) -> Result<Value> {
    Ok(match op {
        "preflight" => {
            let state = read_json(dir, "status.json")?;
            let locked = read_json(dir, "locked.json")?;
            let window = read_json(dir, "window.json")?;
            let size = &window["value"];
            let dimension = |k: &str| {
                size[k]
                    .as_u64()
                    .filter(|v| *v > 0)
                    .ok_or("invalid window size")
            };
            json!({"ready":state["value"]["ready"].as_bool().ok_or("invalid status")?,
                   "locked":locked["value"].as_bool().ok_or("invalid lock state")?,
                   "screen_pt":[dimension("width")?,dimension("height")?]})
        }
        "screenshot" => json!({"bytes":screenshot(dir)?.len()}),
        "decode" => {
            let img = frame(dir, "frame.png")?;
            json!({"width":img.width(),"height":img.height(),"gray_sum":img.iter().map(|v|u64::from(*v)).sum::<u64>()})
        }
        "cuts" => json!({"rows":cuts(&frame(dir,"frame.png")?)}),
        "fastUI" => {
            let reply = read_json(dir, "fastui.json")?;
            let ui = &reply;
            let width = ui["screen"][0].as_i64().ok_or("invalid screen")?;
            let height = ui["screen"][1].as_i64().ok_or("invalid screen")?;
            let mut controls = HashSet::new();
            for item in ui["ui"].as_array().ok_or("invalid UI")? {
                let (Some(x), Some(y)) = (item["x"].as_i64(), item["y"].as_i64()) else {
                    continue;
                };
                let kind = item["type"].as_str().unwrap_or("");
                let label = item["label"]
                    .as_str()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if !kind.is_empty()
                    && !label.is_empty()
                    && (1..width).contains(&x)
                    && (1..height).contains(&y)
                {
                    controls.insert((label, x / 4, y / 4));
                }
            }
            json!({"controls":controls.len()})
        }
        "diff" => {
            let a = frame(dir, "frame.png")?;
            let b = frame(dir, "after.png")?;
            if a.dimensions() != b.dimensions() {
                return Err("frame dimensions differ".into());
            }
            let changed = a.iter().zip(b.iter()).filter(|(a, b)| a != b).count();
            json!({"changed_pixels":changed})
        }
        "ocr" => {
            let output = Command::new("tesseract")
                .arg(dir.join("frame.png").canonicalize()?)
                .args(["stdout", "-l", "eng", "--psm", "6"])
                .env("OMP_THREAD_LIMIT", "1")
                .output()?;
            if !output.status.success() {
                return Err("tesseract failed".into());
            }
            json!({"words":std::str::from_utf8(&output.stdout)?.split_whitespace().count()})
        }
        _ => return Err("unknown read-only operation".into()),
    })
}

fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [op] if op == "--help" => println!("{USAGE}"),
        [op] if op == "cold" => println!("{{\"ready\":true}}"),
        [op, dir, flag] if op == "screenshot" && flag == "--raw" => {
            std::io::stdout()
                .lock()
                .write_all(&screenshot(Path::new(dir))?)?;
        }
        [op, dir] => {
            let dir = Path::new(dir);
            let result = if op == "flow" {
                let flow = read_json(dir, "flow.json")?;
                let mut steps = Vec::new();
                for step in flow.as_array().ok_or("expected flow array")? {
                    let op = step["op"].as_str().ok_or("missing step operation")?;
                    let result = operation(dir, op)?;
                    if let Some(expect) = step.get("expect")
                        && expect != &result
                    {
                        return Err(format!("assertion failed at step {}", steps.len() + 1).into());
                    }
                    steps.push(result);
                }
                json!({"steps":steps})
            } else {
                operation(dir, op)?
            };
            println!("{result}");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
