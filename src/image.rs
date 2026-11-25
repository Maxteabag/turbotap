//! 8-bit grayscale frames and the frame-diff primitives (Python PIL usage).
//!
//! Ports: `_box_diff` (cli.py L1312), `_thumb_diff` (L1324), `_frame_reacted`
//! (L1335), `_content_pixel_still` (L2488), plus decode/convert/crop/resize that
//! Python did with PIL (`Image.open`, `draft("L")`, `convert("L")`, `crop`,
//! `resize(BOX|BILINEAR)`, `ImageChops.difference`, `ImageStat.mean`, `getextrema`).
use crate::error::{Error, Result};
use std::sync::Arc;

/// An 8-bit grayscale image (PIL mode "L"), row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrayImage {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// Frames are shared and compared by identity in tests (`Arc::ptr_eq`).
pub type Frame = Arc<GrayImage>;

impl GrayImage {
    pub fn new(w: u32, h: u32, fill: u8) -> GrayImage {
        GrayImage { w, h, data: vec![fill; (w as usize) * (h as usize)] }
    }

    pub fn from_raw(w: u32, h: u32, data: Vec<u8>) -> GrayImage {
        assert_eq!(data.len(), (w as usize) * (h as usize));
        GrayImage { w, h, data }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    pub fn get(&self, x: u32, y: u32) -> u8 {
        self.data[(y as usize) * (self.w as usize) + x as usize]
    }

    pub fn put(&mut self, x: u32, y: u32, v: u8) {
        let w = self.w as usize;
        self.data[(y as usize) * w + x as usize] = v;
    }

    /// Fill the inclusive rectangle like PIL `ImageDraw.rectangle(rect, fill=v)`.
    pub fn fill_rect(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, v: u8) {
        let (w, h) = (self.w as i64, self.h as i64);
        for y in y0.max(0)..=y1.min(h - 1) {
            for x in x0.max(0)..=x1.min(w - 1) {
                self.put(x as u32, y as u32, v);
            }
        }
    }

    /// PIL `crop((x0, y0, x1, y1))` with clamping to the image (PIL pads with 0
    /// outside; callers here always pass in-bounds boxes).
    pub fn crop(&self, x0: i64, y0: i64, x1: i64, y1: i64) -> GrayImage {
        let _ = (x0, y0, x1, y1);
        todo!("port PIL crop")
    }

    /// PIL `resize(size, Resampling.BOX)` (area average).
    pub fn resize_box(&self, w: u32, h: u32) -> GrayImage {
        let _ = (w, h);
        todo!("port PIL resize BOX")
    }

    /// PIL `resize(size, Resampling.BILINEAR)`.
    pub fn resize_bilinear(&self, w: u32, h: u32) -> GrayImage {
        let _ = (w, h);
        todo!("port PIL resize BILINEAR")
    }

    /// Decode JPEG or PNG bytes to grayscale (PIL `convert("L")`: ITU-R 601-2 luma).
    pub fn decode(bytes: &[u8]) -> Result<GrayImage> {
        let _ = bytes;
        todo!("decode JPEG/PNG to L")
    }

    /// Decode, letting JPEG decoders downscale toward `target` first (PIL
    /// `draft("L", out)`); the result may still need a resize.
    pub fn decode_draft(bytes: &[u8], target: (u32, u32)) -> Result<GrayImage> {
        let _ = target;
        Self::decode(bytes)
    }

    pub fn encode_png(&self) -> Vec<u8> {
        todo!("encode PNG")
    }

    pub fn encode_jpeg(&self, quality: u8) -> Vec<u8> {
        let _ = quality;
        todo!("encode JPEG")
    }

    pub fn save_png(&self, path: &std::path::Path) -> Result<()> {
        std::fs::write(path, self.encode_png()).map_err(|e| Error::Os(format!("{}: {e}", path.display())))
    }
}

/// Mean grey difference of two frames in a small box around (x, y) points.
pub fn box_diff(a: Option<&GrayImage>, b: Option<&GrayImage>, x: f64, y: f64, scale: f64, dx: f64, dy: f64, min_y: i64) -> f64 {
    let _ = (a, b, x, y, scale, dx, dy, min_y);
    todo!("port cli.py L1312 _box_diff (defaults dx=30, dy=18, min_y=0)")
}

/// Mean absolute grey difference (0-255) on a 1/8 thumbnail.
pub fn thumb_diff(a: Option<&GrayImage>, b: Option<&GrayImage>) -> f64 {
    let _ = (a, b);
    todo!("port cli.py L1324 _thumb_diff")
}

/// True when `cur` shows a real UI reaction vs `before` (default scale 1.5).
pub fn frame_reacted(before: Option<&GrayImage>, cur: Option<&GrayImage>, x: Option<f64>, y: Option<f64>, scale: f64) -> bool {
    let _ = (before, cur, x, y, scale);
    todo!("port cli.py L1335 _frame_reacted")
}

/// True when two frames have no local pixel change > max_diff below the status bar
/// (default max_diff 6).
pub fn content_pixel_still(a: Option<&GrayImage>, b: Option<&GrayImage>, max_diff: u8) -> bool {
    let _ = (a, b, max_diff);
    todo!("port cli.py L2488 _content_pixel_still")
}
