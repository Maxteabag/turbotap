//! A light label on a filled control in a dark screen (a bright button with white text
//! on a black page): one OCR threshold per strip separates the page from the fill and
//! loses the label, so `read:Y0-Y1` came back empty. The label is now read from the
//! control's inside on its own. Synthetic screens only.

use crate::image::GrayImage;
use crate::ocr::{filled_controls, RawWords, TEST_OCR_RAW_HOOK};
use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const PT: (i64, i64) = (402, 874);
const K: i64 = 3; // pixels per point in the synthetic screen
const PAGE: u8 = 0;
const FILL: u8 = 112;
const LABEL: u8 = 240;

/// A dark page with a title line and one wide filled button holding a light label
/// (points: button x 38..364, y 734..778; label x 166..236, y 748..764). Even point
/// coordinates keep every edge on a whole pixel at the 2:1 capture scale.
fn dark_screen() -> GrayImage {
    let mut img = GrayImage::new((PT.0 * K) as u32, (PT.1 * K) as u32, PAGE);
    let r = |img: &mut GrayImage, x0: i64, y0: i64, x1: i64, y1: i64, v: u8| img.fill_rect(x0 * K, y0 * K, x1 * K - 1, y1 * K - 1, v);
    r(&mut img, 40, 130, 300, 150, 235);
    r(&mut img, 60, 734, 342, 778, FILL);
    // Rounded ends: shorter columns toward the edges.
    for (dx, inset) in [(0, 12), (8, 6), (16, 2)] {
        r(&mut img, 38 + dx, 734 + inset, 46 + dx, 778 - inset, FILL);
        r(&mut img, 356 - dx, 733 + inset, 364 - dx, 778 - inset, FILL);
    }
    r(&mut img, 166, 748, 236, 764, LABEL);
    img
}

/// Otsu threshold of a buffer.
fn otsu(raw: &[u8]) -> u8 {
    let mut hist = [0f64; 256];
    for &v in raw {
        hist[v as usize] += 1.0;
    }
    let n = raw.len() as f64;
    let total: f64 = (0..256).map(|i| i as f64 * hist[i]).sum();
    let (mut wb, mut sb, mut best, mut t) = (0f64, 0f64, -1f64, 0u8);
    for i in 0..256 {
        wb += hist[i];
        if wb == 0.0 || wb == n {
            continue;
        }
        sb += i as f64 * hist[i];
        let (mb, mf) = (sb / wb, (total - sb) / (n - wb));
        let between = wb * (n - wb) * (mb - mf).powi(2);
        if between > best {
            best = between;
            t = i as u8;
        }
    }
    t
}

/// Stands in for Tesseract with one global threshold. Labels are the connected regions
/// of the rarest gray level (at least 0.2 % of the buffer); each is read only when the
/// buffer's Otsu threshold puts it on the other side from the pixels around it. A wide
/// bar reads "Title", any other label "Continue".
fn global_threshold_ocr(raw: &[u8], w: u32, h: u32, _sauvola: bool) -> crate::error::Result<Vec<RawWords>> {
    let (w, h) = (w as usize, h as usize);
    let mut bins = [0usize; 8];
    for &v in raw {
        bins[(v >> 5) as usize] += 1;
    }
    let present: Vec<usize> = (0..8).filter(|&b| bins[b] * 500 >= raw.len()).collect();
    if present.len() < 2 {
        return Ok(Vec::new());
    }
    let label_bin = *present.iter().min_by_key(|&&b| bins[b]).unwrap();
    let is_label = |v: u8| (v >> 5) as usize == label_bin;
    let t = otsu(raw);
    let side = |v: u8| v > t;
    let mut seen = vec![false; w * h];
    let mut out: Vec<RawWords> = Vec::new();
    for start in 0..w * h {
        if seen[start] || !is_label(raw[start]) {
            continue;
        }
        // One connected label region (4-neighbours) and its bounding box.
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            let mut push = |j: usize| {
                if !seen[j] && is_label(raw[j]) {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 { push(i - 1); }
            if x + 1 < w { push(i + 1); }
            if y > 0 { push(i - w); }
            if y + 1 < h { push(i + w); }
        }
        if (x1 - x0) * (y1 - y0) * 1000 < raw.len() {
            continue; // specks
        }
        let label_side = side(raw[y0 * w + x0]);
        let (mut same, mut ring) = (0usize, 0usize);
        for y in y0.saturating_sub(3)..(y1 + 3).min(h) {
            for x in x0.saturating_sub(3)..(x1 + 3).min(w) {
                let v = raw[y * w + x];
                if !is_label(v) {
                    ring += 1;
                    same += (side(v) == label_side) as usize;
                }
            }
        }
        if ring == 0 || same * 2 >= ring {
            continue; // merged with its surroundings: nothing to read
        }
        let text = if x1 - x0 > 8 * (y1 - y0) { "Title" } else { "Continue" };
        out.push(vec![(text.to_string(), x0 as i64, y0 as i64, x1 as i64, y1 as i64)]);
    }
    Ok(out)
}

struct OcrHook;
impl OcrHook {
    fn set(hook: crate::ocr::OcrRawHook) -> OcrHook {
        TEST_OCR_RAW_HOOK.with(|h| *h.borrow_mut() = Some(hook));
        OcrHook
    }
}
impl Drop for OcrHook {
    fn drop(&mut self) {
        TEST_OCR_RAW_HOOK.with(|h| *h.borrow_mut() = None);
    }
}

fn mock_with(screen: GrayImage, front: &str) -> MockWda {
    let mut st = MockState::default();
    st.screen_pt = Seq::one(PT);
    st.active_app = Seq::one(json!(front));
    st.screen = Some(screen);
    MockWda::start(st)
}

/// The wrapper-shaped call: `turbotap --device 17 flow --allow-app A --allow-app B STEP`
/// with device 17 configured to the mock, no daemon, no extra flags.
fn wrapper_read(mock: &MockWda, step: &str) -> (i32, Value, String) {
    let home = TempDir::new("tt-filled-home-");
    let cfg = home.path().join("config.json");
    std::fs::write(&cfg, json!({"devices": {"17": {"url": mock.url, "allowed_apps": ["com.example.app"]}}}).to_string()).unwrap();
    let (home_s, cfg_s) = (home.path().to_string_lossy().into_owned(), cfg.to_string_lossy().into_owned());
    let _env = set_env(&[
        ("HOME", Some(home_s.as_str())),
        ("XDG_STATE_HOME", Some(home_s.as_str())),
        ("WDA_CONFIG", Some(cfg_s.as_str())),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
        ("WDA_TURBO", None),
        ("TURBOTAP_DAEMON", None),
        ("TURBOTAP_ALLOWED_APPS", None),
        ("TURBOTAP_STOP_FILE", None),
        ("TURBOTAP_TIMINGS", Some("0")),
    ]);
    let args = ["--device", "17", "flow", "--allow-app", "com.example.updater", "--allow-app", "com.example.sheet", step];
    let cap = Capture::start();
    let code = crate::cli::run_cli(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let (out, err) = (cap.stdout(), cap.stderr());
    let first = out.lines().next().unwrap_or("{}").to_string();
    (code, serde_json::from_str(&first).unwrap_or(Value::Null), err)
}

fn lines_of(v: &Value) -> Vec<(String, i64, i64)> {
    v["reads"].as_array().and_then(|r| r.last()).and_then(|r| r["lines"].as_array()).cloned().unwrap_or_default()
        .iter()
        .map(|l| (l["text"].as_str().unwrap_or("").to_string(), l["x"].as_i64().unwrap_or(-1), l["y"].as_i64().unwrap_or(-1)))
        .collect()
}

#[test]
fn test_band_read_returns_the_light_label_on_a_filled_button_in_a_dark_screen() {
    let _c = isolated_cache();
    let _ocr = OcrHook::set(Arc::new(global_threshold_ocr));
    let mock = mock_with(dark_screen(), "com.example.sheet");
    let (code, v, err) = wrapper_read(&mock, "read:650-850");
    assert_eq!(code, 0, "{err}");
    let lines = lines_of(&v);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (text, x, y) = &lines[0];
    assert_eq!(text, "Continue");
    // Centre of the label (x 166..236, y 748..764 pt), in screen points.
    assert!((x - 201).abs() <= 3 && (y - 756).abs() <= 3, "{lines:?}");
    // The band was captured as a band (crop in points), not as a full screen.
    let shots: Vec<Value> = mock.calls().into_iter().filter(|(_, p, _)| p == "/wda/fastScreenshot").map(|(_, _, d)| d).collect();
    assert!(shots.iter().any(|d| d["y0"] == json!(650) && d["y1"] == json!(850)), "{shots:?}");
}

#[test]
fn test_full_read_also_returns_the_button_label() {
    let _c = isolated_cache();
    let _ocr = OcrHook::set(Arc::new(global_threshold_ocr));
    let mock = mock_with(dark_screen(), "com.example.sheet");
    let (code, v, err) = wrapper_read(&mock, "read:ocr");
    assert_eq!(code, 0, "{err}");
    let lines = lines_of(&v);
    assert!(lines.iter().any(|(t, x, y)| t == "Continue" && (x - 201).abs() <= 3 && (y - 756).abs() <= 3), "{lines:?}");
    // The title bar spans y 130..150 pt; a strip cut may split it, so any line in that band.
    assert!(lines.iter().any(|(t, _, y)| t == "Title" && (128..=152).contains(y)), "the page text is still read: {lines:?}");
}

#[test]
fn test_without_the_control_pass_the_same_strip_reads_nothing() {
    // The fake fails like Tesseract did on the whole strip: this pins that the empty
    // band came from the threshold, and the pass is what reads the label.
    let screen = dark_screen().crop(0, 650 * K, PT.0 * K, 850 * K).resize_bilinear(603, 300);
    let strip = screen.crop(0, 100, 603, 200);
    assert!(global_threshold_ocr(&strip.data, strip.w, strip.h, false).unwrap().is_empty());
    let found = filled_controls(&strip.data, strip.w as usize, strip.h as usize);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ink_brighter);
}

#[test]
fn test_no_extra_ocr_when_the_strip_already_read_the_label() {
    let _c = isolated_cache();
    // A Tesseract that reads every light label in one pass: the control pass must not
    // add an OCR call (no slower on screens that already worked). The pass sends a
    // two-valued (0/255) buffer, so those are counted.
    let extra = Arc::new(AtomicUsize::new(0));
    let e2 = extra.clone();
    let _ocr = OcrHook::set(Arc::new(move |raw: &[u8], w: u32, h: u32, _s: bool| {
        if raw.iter().all(|&v| v == 0 || v == 255) && raw.contains(&0) && raw.contains(&255) {
            e2.fetch_add(1, Ordering::SeqCst);
        }
        let (w, h) = (w as usize, h as usize);
        let hits: Vec<usize> = (0..w * h).filter(|&i| raw[i] >= 200 && raw[i] < 255).collect();
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let (xs, ys): (Vec<usize>, Vec<usize>) = hits.iter().map(|&i| (i % w, i / w)).unzip();
        let bb = (*xs.iter().min().unwrap(), *ys.iter().min().unwrap(), *xs.iter().max().unwrap() + 1, *ys.iter().max().unwrap() + 1);
        Ok(vec![vec![("Continue".to_string(), bb.0 as i64, bb.1 as i64, bb.2 as i64, bb.3 as i64)]])
    }));
    let mock = mock_with(dark_screen(), "com.example.sheet");
    let (code, v, err) = wrapper_read(&mock, "read:650-850");
    assert_eq!(code, 0, "{err}");
    assert_eq!(lines_of(&v).len(), 1, "{v}");
    assert_eq!(extra.load(Ordering::SeqCst), 0, "the control pass ran although the strip read its label");
}

#[test]
fn test_filled_controls_finds_buttons_but_not_photos_or_empty_fills() {
    let w = 600usize;
    let h = 90usize;
    let mut img = GrayImage::new(w as u32, h as u32, 255); // light page
    img.fill_rect(60, 15, 539, 74, 40); // dark filled button
    img.fill_rect(250, 35, 349, 54, 250); // light label
    let found = filled_controls(&img.data, w, h);
    assert_eq!(found.len(), 1, "{found:?}");
    let b = &found[0];
    assert!(b.ink_brighter && b.threshold > 40 && b.threshold < 250, "{b:?}");
    assert!(b.xa > 60 && b.xb < 540 && b.xa < 250 && b.xb > 349, "{b:?}");

    // A dark button with no label: nothing to read.
    let mut empty = GrayImage::new(w as u32, h as u32, 255);
    empty.fill_rect(60, 15, 539, 74, 40);
    assert!(filled_controls(&empty.data, w, h).is_empty());

    // A photo-like block (uneven): not a control.
    let mut photo = GrayImage::new(w as u32, h as u32, 255);
    for y in 15..75 {
        for x in 60..540 {
            photo.data[y * w + x] = ((x * 7 + y * 13) % 200) as u8;
        }
    }
    assert!(filled_controls(&photo.data, w, h).is_empty());

    // A narrow block (an icon, under a quarter of the width): not a control.
    let mut icon = GrayImage::new(w as u32, h as u32, 255);
    icon.fill_rect(60, 15, 159, 74, 40);
    icon.fill_rect(90, 35, 129, 54, 250);
    assert!(filled_controls(&icon.data, w, h).is_empty());
}
