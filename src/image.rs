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

    /// PIL `crop((x0, y0, x1, y1))`: pixels outside the image read as 0.
    pub fn crop(&self, x0: i64, y0: i64, x1: i64, y1: i64) -> GrayImage {
        let ow = (x1 - x0).max(0) as u32;
        let oh = (y1 - y0).max(0) as u32;
        let mut out = GrayImage::new(ow, oh, 0);
        let (w, h) = (self.w as i64, self.h as i64);
        let sx0 = x0.max(0);
        let sx1 = x1.min(w);
        if sx1 <= sx0 {
            return out;
        }
        for oy in 0..oh as i64 {
            let sy = y0 + oy;
            if sy < 0 || sy >= h {
                continue;
            }
            let src = (sy * w) as usize;
            let dst = (oy * ow as i64) as usize;
            let a = (sx0 - x0) as usize;
            out.data[dst + a..dst + a + (sx1 - sx0) as usize]
                .copy_from_slice(&self.data[src + sx0 as usize..src + sx1 as usize]);
        }
        out
    }

    /// PIL `resize(size, Resampling.BOX)` (area average).
    pub fn resize_box(&self, w: u32, h: u32) -> GrayImage {
        self.resample(w, h, Filter::Box)
    }

    /// PIL `resize(size, Resampling.BILINEAR)`.
    pub fn resize_bilinear(&self, w: u32, h: u32) -> GrayImage {
        self.resample(w, h, Filter::Bilinear)
    }

    /// Pillow `ImagingResample` for 8-bit images: separable convolution, horizontal
    /// pass first, each pass rounded to 8 bits with 22-bit fixed-point coefficients.
    fn resample(&self, w: u32, h: u32, filter: Filter) -> GrayImage {
        if (w, h) == (self.w, self.h) {
            return self.clone(); // Image.resize returns a copy for the same size
        }
        if w == 0 || h == 0 || self.w == 0 || self.h == 0 {
            return GrayImage::new(w, h, 0);
        }
        // Pillow runs both passes in the order that gives the smaller intermediate
        // image (vertical first only when strictly smaller).
        let vertical_first = w != self.w
            && h != self.h
            && (self.w as u64) * (h as u64) < (w as u64) * (self.h as u64);
        if vertical_first {
            self.pass_vertical(h, filter).pass_horizontal(w, filter)
        } else {
            let cur = if w != self.w { self.pass_horizontal(w, filter) } else { self.clone() };
            if h != cur.h {
                cur.pass_vertical(h, filter)
            } else {
                cur
            }
        }
    }

    fn pass_horizontal(&self, w: u32, filter: Filter) -> GrayImage {
        let (bounds, k, ksize) = precompute_coeffs(self.w, w, filter);
        let mut out = GrayImage::new(w, self.h, 0);
        for y in 0..self.h as usize {
            let row = &self.data[y * self.w as usize..(y + 1) * self.w as usize];
            for xx in 0..w as usize {
                let (xmin, xmax) = bounds[xx];
                let kk = &k[xx * ksize..];
                let mut ss: i64 = 1 << (PRECISION_BITS - 1);
                for x in 0..xmax {
                    ss += row[xmin + x] as i64 * kk[x] as i64;
                }
                out.data[y * w as usize + xx] = clip8(ss);
            }
        }
        out
    }

    fn pass_vertical(&self, h: u32, filter: Filter) -> GrayImage {
        let (bounds, k, ksize) = precompute_coeffs(self.h, h, filter);
        let cw = self.w as usize;
        let mut out = GrayImage::new(self.w, h, 0);
        for yy in 0..h as usize {
            let (ymin, ymax) = bounds[yy];
            let kk = &k[yy * ksize..];
            for x in 0..cw {
                let mut ss: i64 = 1 << (PRECISION_BITS - 1);
                for y in 0..ymax {
                    ss += self.data[(ymin + y) * cw + x] as i64 * kk[y] as i64;
                }
                out.data[yy * cw + x] = clip8(ss);
            }
        }
        out
    }

    /// Decode JPEG or PNG bytes to grayscale (PIL `convert("L")`: ITU-R 601-2 luma).
    pub fn decode(bytes: &[u8]) -> Result<GrayImage> {
        use ::image::DynamicImage;
        let img = ::image::load_from_memory(bytes).map_err(|e| Error::Os(format!("cannot identify image file: {e}")))?;
        let (w, h) = (img.width(), img.height());
        let data = match img {
            DynamicImage::ImageLuma8(b) => b.into_raw(),
            DynamicImage::ImageLumaA8(b) => b.into_raw().chunks_exact(2).map(|p| p[0]).collect(),
            DynamicImage::ImageRgb8(b) => b.into_raw().chunks_exact(3).map(l24).collect(),
            DynamicImage::ImageRgba8(b) => b.into_raw().chunks_exact(4).map(l24).collect(),
            DynamicImage::ImageLuma16(b) => b.into_raw().iter().map(|v| (v >> 8) as u8).collect(),
            other => other.to_rgb8().into_raw().chunks_exact(3).map(l24).collect(),
        };
        Ok(GrayImage::from_raw(w, h, data))
    }

    /// Decode, letting JPEG decoders downscale toward `target` first (PIL
    /// `draft("L", out)`); the result may still need a resize. The device already
    /// downscales fastScreenshot frames to the OCR size, so this decodes at full size.
    pub fn decode_draft(bytes: &[u8], target: (u32, u32)) -> Result<GrayImage> {
        let _ = target;
        Self::decode(bytes)
    }

    pub fn encode_png(&self) -> Vec<u8> {
        use ::image::ImageEncoder;
        let mut out = Vec::new();
        let enc = ::image::codecs::png::PngEncoder::new(&mut out);
        // Encoding an in-memory L8 buffer of the declared size cannot fail.
        let _ = enc.write_image(&self.data, self.w, self.h, ::image::ExtendedColorType::L8);
        out
    }

    pub fn encode_jpeg(&self, quality: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
        let _ = enc.encode(&self.data, self.w, self.h, ::image::ExtendedColorType::L8);
        out
    }

    /// 8-bit grayscale BMP with a 256-entry palette (PIL `img.save(buf, "BMP")` for
    /// mode "L"), the format the `tesseract` binary reads from stdin.
    pub fn encode_bmp(&self) -> Vec<u8> {
        let stride = (self.w as usize + 3) & !3;
        let pixels = stride * self.h as usize;
        let offset = 14 + 40 + 256 * 4;
        let size = offset + pixels;
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&(size as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&(offset as u32).to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&(self.w as i32).to_le_bytes());
        b.extend_from_slice(&(self.h as i32).to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        b.extend_from_slice(&(pixels as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // 72 dpi like PIL is optional
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&256u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        for i in 0..=255u8 {
            b.extend_from_slice(&[i, i, i, 0]);
        }
        let pad = stride - self.w as usize;
        for y in (0..self.h as usize).rev() {
            b.extend_from_slice(&self.data[y * self.w as usize..(y + 1) * self.w as usize]);
            b.extend(std::iter::repeat_n(0u8, pad));
        }
        b
    }

    /// `ImageChops.difference(a, b)` (same size).
    pub fn difference(&self, o: &GrayImage) -> GrayImage {
        let data = self.data.iter().zip(&o.data).map(|(a, b)| a.abs_diff(*b)).collect();
        GrayImage { w: self.w, h: self.h, data }
    }

    /// `ImageStat.Stat(img).mean[0]`.
    pub fn mean(&self) -> f64 {
        if self.data.is_empty() {
            return 0.0;
        }
        self.data.iter().map(|&v| v as u64).sum::<u64>() as f64 / self.data.len() as f64
    }

    /// `getextrema()[1]` (0 for an empty image).
    pub fn max_value(&self) -> u8 {
        self.data.iter().copied().max().unwrap_or(0)
    }

    pub fn save_png(&self, path: &std::path::Path) -> Result<()> {
        std::fs::write(path, self.encode_png()).map_err(|e| Error::Os(format!("{}: {e}", path.display())))
    }
}

/// Mean grey difference of two frames in a small box around (x, y) points
/// (Python defaults dx=30, dy=18, min_y=0).
pub fn box_diff(a: Option<&GrayImage>, b: Option<&GrayImage>, x: f64, y: f64, scale: f64, dx: f64, dy: f64, min_y: i64) -> f64 {
    let (Some(a), Some(b)) = (a, b) else { return 255.0 };
    if a.size() != b.size() {
        return 255.0;
    }
    let bx = (
        0i64.max(((x - dx) * scale) as i64),
        min_y.max(((y - dy) * scale) as i64),
        (a.w as i64).min(((x + dx) * scale) as i64),
        (a.h as i64).min(((y + dy) * scale) as i64),
    );
    if bx.2 <= bx.0 || bx.3 <= bx.1 {
        return if min_y > 0 { 0.0 } else { 255.0 };
    }
    a.crop(bx.0, bx.1, bx.2, bx.3).difference(&b.crop(bx.0, bx.1, bx.2, bx.3)).mean()
}

/// Mean absolute grey difference (0-255) on a 1/8 thumbnail.
pub fn thumb_diff(a: Option<&GrayImage>, b: Option<&GrayImage>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else { return 255.0 };
    if a.size() != b.size() {
        return 255.0;
    }
    let (w, h) = ((a.w / 8).max(1), (a.h / 8).max(1));
    a.resize_box(w, h).difference(&b.resize_box(w, h)).mean()
}

/// True when `cur` shows a real UI reaction vs `before` (default scale 1.5).
///
/// Combines:
/// - global mean diff (> 1.0) on a 24x48 area-averaged grid (transitions, modals),
/// - local box diff (> 2.5) around the tap point (button highlight, cursor, toggle),
/// - local 4x4-averaged peak diff (>= 18) in a tap-row band [x-55..x+55, y-18..y+18]
///   clipped below the status bar (tiny radio dots, checkboxes, and focus cursors
///   15-45 pt from a label that dilute below box mean > 2.5 and 24x48 grid >= 14,
///   while rejecting +-3 JPEG noise and 1px edge ringing),
/// - max grid-cell diff (>= 14) anywhere below the status bar (remote localized
///   changes such as a single Calculator digit appearing at the top when tapping a
///   keypad key), while rejecting JPEG quality=0.6 compression noise (max cell <= 1-2).
pub fn frame_reacted(before: Option<&GrayImage>, cur: Option<&GrayImage>, x: Option<f64>, y: Option<f64>, scale: f64) -> bool {
    let (Some(before), Some(cur)) = (before, cur) else { return true };
    if before.size() != cur.size() {
        return true;
    }
    let (w, h) = (before.w as i64, before.h as i64);
    let top = if h >= 200 { (h as f64 * 0.06) as i64 } else { 0 };
    let grid = (24i64.min(w.max(1)) as u32, 48i64.min((h - top).max(1)) as u32);
    let ca = before.crop(0, top, w, h).resize_box(grid.0, grid.1);
    let cb = cur.crop(0, top, w, h).resize_box(grid.0, grid.1);
    let d = ca.difference(&cb);
    if d.max_value() >= 14 {
        return true;
    }
    if d.mean() > 1.0 {
        return true;
    }
    if let (Some(x), Some(y)) = (x, y) {
        if box_diff(Some(before), Some(cur), x, y, scale, 45.0, 25.0, top) > 2.5 {
            return true;
        }
        let x0 = 0i64.max(((x - 55.0) * scale) as i64);
        let x1 = w.min(((x + 55.0) * scale) as i64);
        let y0 = top.max(((y - 18.0) * scale) as i64);
        let y1 = h.min(((y + 18.0) * scale) as i64);
        let (bw, bh) = (x1 - x0, y1 - y0);
        if bw > 0 && bh > 0 {
            let lgrid = ((bw / 4).max(1) as u32, (bh / 4).max(1) as u32);
            let la = before.crop(x0, y0, x1, y1).resize_box(lgrid.0, lgrid.1);
            let lb = cur.crop(x0, y0, x1, y1).resize_box(lgrid.0, lgrid.1);
            if la.difference(&lb).max_value() >= 18 {
                return true;
            }
        }
    }
    false
}

/// True when two frames have no meaningful local pixel change below the status bar
/// (top 6%; default max_diff 6). Unlike whole-screen [`thumb_diff`], catches small
/// local updates like a single digit changing '1' -> '2' or text appearing on an
/// otherwise still background.
pub fn content_pixel_still(a: Option<&GrayImage>, b: Option<&GrayImage>, max_diff: u8) -> bool {
    let (Some(a), Some(b)) = (a, b) else { return false };
    if a.size() != b.size() {
        return false;
    }
    let (w, h) = (a.w as i64, a.h as i64);
    let y0 = (h as f64 * 0.06) as i64;
    let peak = if y0 > 0 && y0 < h {
        a.crop(0, y0, w, h).difference(&b.crop(0, y0, w, h)).max_value()
    } else {
        a.difference(b).max_value()
    };
    peak <= max_diff
}

// ---------------------------------------------------------------------------
// Pillow resampling internals (libImaging/Resample.c)
// ---------------------------------------------------------------------------

const PRECISION_BITS: u32 = 32 - 8 - 2;

#[derive(Clone, Copy)]
enum Filter {
    Box,
    Bilinear,
}

impl Filter {
    fn support(self) -> f64 {
        match self {
            Filter::Box => 0.5,
            Filter::Bilinear => 1.0,
        }
    }
    fn eval(self, x: f64) -> f64 {
        match self {
            Filter::Box => {
                if x > -0.5 && x <= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            Filter::Bilinear => {
                let x = x.abs();
                if x < 1.0 {
                    1.0 - x
                } else {
                    0.0
                }
            }
        }
    }
}

/// `precompute_coeffs` + `normalize_coeffs_8bpc`: per output pixel the first input
/// index, the tap count, and fixed-point weights (row stride `ksize`).
fn precompute_coeffs(in_size: u32, out_size: u32, filter: Filter) -> (Vec<(usize, usize)>, Vec<i32>, usize) {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = filter.support() * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size as usize);
    let mut kk = vec![0i32; out_size as usize * ksize];
    let ss = 1.0 / filterscale;
    let mut pre = vec![0f64; ksize];
    for xx in 0..out_size as usize {
        let center = (xx as f64 + 0.5) * scale;
        let mut ww = 0.0;
        let xmin = ((center - support + 0.5) as i64).max(0);
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64);
        let n = (xmax - xmin).max(0) as usize;
        for (x, p) in pre.iter_mut().enumerate().take(n) {
            let w = filter.eval((x as f64 + xmin as f64 - center + 0.5) * ss);
            *p = w;
            ww += w;
        }
        for (x, p) in pre.iter().enumerate().take(n) {
            let v = if ww != 0.0 { p / ww } else { *p };
            let r = if v < 0.0 { -0.5 } else { 0.5 };
            kk[xx * ksize + x] = (v * (1u64 << PRECISION_BITS) as f64 + r) as i32;
        }
        bounds.push((xmin as usize, n));
    }
    (bounds, kk, ksize)
}

fn clip8(ss: i64) -> u8 {
    (ss >> PRECISION_BITS).clamp(0, 255) as u8
}

/// Pillow `L24` RGB -> L: `(r*19595 + g*38470 + b*7471 + 0x8000) >> 16`.
fn l24(p: &[u8]) -> u8 {
    ((p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16) as u8
}

