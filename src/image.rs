//! 8-bit grayscale frames and the frame-diff primitives (Python PIL usage).
//!
//! Ports: `_box_diff` (v0.10.0 cli.py L1312), `_thumb_diff` (L1324), `_frame_reacted`
//! (L1335), `_content_pixel_still` (L2488), plus decode/convert/crop/resize that
//! Python did with PIL (`Image.open`, `draft("L")`, `convert("L")`, `crop`,
//! `resize(BOX|BILINEAR)`, `ImageChops.difference`, `ImageStat.mean`, `getextrema`).
use crate::error::{Error, Result};
use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    /// Reusable per-thread RGBA decode buffer for `decode_jpeg_fast`, avoiding repeated
    /// ~3.6 MB mmap/munmap page faults on every 645x1398 screenshot decode.
    static JPEG_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

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
        self.resample_region_box(0, 0, self.w, self.h, w, h)
    }

    /// PIL `resize(size, Resampling.BILINEAR)`.
    pub fn resize_bilinear(&self, w: u32, h: u32) -> GrayImage {
        self.resample(w, h, Filter::Bilinear)
    }

    /// Zero-crop box resample of an in-bounds sub-rectangle `[rx..rx+rw, ry..ry+rh]`
    /// to `(ow, oh)`. Bit-exact with `self.crop(rx, ry, rx+rw, ry+rh).resize_box(ow, oh)`.
    fn resample_region_box(&self, rx: usize, ry: usize, rw: u32, rh: u32, ow: u32, oh: u32) -> GrayImage {
        if (ow, oh) == (rw, rh) {
            return if rx == 0 && ry == 0 && rw == self.w && rh == self.h {
                self.clone()
            } else {
                self.crop(rx as i64, ry as i64, (rx + rw as usize) as i64, (ry + rh as usize) as i64)
            };
        }
        if ow == 0 || oh == 0 || rw == 0 || rh == 0 {
            return GrayImage::new(ow, oh, 0);
        }
        let vertical_first =
            ow != rw && oh != rh && (rw as u64) * (oh as u64) < (ow as u64) * (rh as u64);
        let h_spans = (ow != rw).then(|| precompute_box_spans(rw, ow));
        let v_spans = (oh != rh).then(|| precompute_box_spans(rh, oh));
        self.resample_region_box_with(rx, ry, rw, rh, ow, oh, vertical_first, h_spans.as_deref(), v_spans.as_deref())
    }

    fn resample_region_box_with(
        &self,
        rx: usize,
        ry: usize,
        rw: u32,
        rh: u32,
        ow: u32,
        oh: u32,
        vertical_first: bool,
        h_spans: Option<&[(usize, usize, i32)]>,
        v_spans: Option<&[(usize, usize, i32)]>,
    ) -> GrayImage {
        let stride = self.w as usize;
        if vertical_first {
            let mid = pass_vertical_box_region(&self.data, stride, rx, ry, rw, v_spans.expect("v_spans"));
            pass_horizontal_box_region(&mid.data, mid.w as usize, 0, 0, mid.h, h_spans.expect("h_spans"))
        } else {
            let cur = match h_spans {
                Some(hs) => pass_horizontal_box_region(&self.data, stride, rx, ry, rh, hs),
                None => {
                    if rx == 0 && ry == 0 && rw == self.w && rh == self.h {
                        self.clone()
                    } else {
                        self.crop(rx as i64, ry as i64, (rx + rw as usize) as i64, (ry + rh as usize) as i64)
                    }
                }
            };
            match v_spans {
                Some(vs) => pass_vertical_box_region(&cur.data, cur.w as usize, 0, 0, cur.w, vs),
                None => {
                    debug_assert_eq!((cur.w, cur.h), (ow, oh));
                    cur
                }
            }
        }
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
        let iw = self.w as usize;
        let ow = w as usize;
        let mut out = vec![0u8; ow * self.h as usize];
        // For non-negative Box/Bilinear filters, sum(px * kk) + bias <= ~1.08e9 < i32::MAX
        // whenever ksize <= 1_000_000, enabling 8-wide SIMD i32 multiply-add.
        if ksize <= 1_000_000 {
            let bias: i32 = 1 << (PRECISION_BITS - 1);
            for y in 0..self.h as usize {
                let row = &self.data[y * iw..(y + 1) * iw];
                let dst = &mut out[y * ow..(y + 1) * ow];
                for (xx, d) in dst.iter_mut().enumerate() {
                    let (xmin, xmax) = bounds[xx];
                    let kk = &k[xx * ksize..xx * ksize + xmax];
                    let src = &row[xmin..xmin + xmax];
                    let mut ss = bias;
                    for (&px, &coeff) in src.iter().zip(kk.iter()) {
                        ss += px as i32 * coeff;
                    }
                    *d = clip8_i32(ss);
                }
            }
        } else {
            let bias: i64 = 1 << (PRECISION_BITS - 1);
            for y in 0..self.h as usize {
                let row = &self.data[y * iw..(y + 1) * iw];
                let dst = &mut out[y * ow..(y + 1) * ow];
                for (xx, d) in dst.iter_mut().enumerate() {
                    let (xmin, xmax) = bounds[xx];
                    let kk = &k[xx * ksize..xx * ksize + xmax];
                    let src = &row[xmin..xmin + xmax];
                    let mut ss = bias;
                    for (&px, &coeff) in src.iter().zip(kk.iter()) {
                        ss += px as i64 * coeff as i64;
                    }
                    *d = clip8(ss);
                }
            }
        }
        GrayImage { w, h: self.h, data: out }
    }

    fn pass_vertical(&self, h: u32, filter: Filter) -> GrayImage {
        let (bounds, k, ksize) = precompute_coeffs(self.h, h, filter);
        let cw = self.w as usize;
        let mut out = vec![0u8; cw * h as usize];
        // Row-major accumulation into a single line buffer eliminates strided
        // column-major cache misses and auto-vectorizes across each scanline.
        if ksize <= 1_000_000 {
            let mut acc = vec![0i32; cw];
            let bias: i32 = 1 << (PRECISION_BITS - 1);
            for yy in 0..h as usize {
                let (ymin, ymax) = bounds[yy];
                let kk = &k[yy * ksize..yy * ksize + ymax];
                acc.fill(bias);
                for (y, &coeff) in kk.iter().enumerate() {
                    if coeff == 0 {
                        continue;
                    }
                    let row = &self.data[(ymin + y) * cw..(ymin + y + 1) * cw];
                    for (a, &px) in acc.iter_mut().zip(row.iter()) {
                        *a += px as i32 * coeff;
                    }
                }
                let dst = &mut out[yy * cw..(yy + 1) * cw];
                for (d, &a) in dst.iter_mut().zip(acc.iter()) {
                    *d = clip8_i32(a);
                }
            }
        } else {
            let mut acc = vec![0i64; cw];
            let bias: i64 = 1 << (PRECISION_BITS - 1);
            for yy in 0..h as usize {
                let (ymin, ymax) = bounds[yy];
                let kk = &k[yy * ksize..yy * ksize + ymax];
                acc.fill(bias);
                for (y, &coeff) in kk.iter().enumerate() {
                    if coeff == 0 {
                        continue;
                    }
                    let c = coeff as i64;
                    let row = &self.data[(ymin + y) * cw..(ymin + y + 1) * cw];
                    for (a, &px) in acc.iter_mut().zip(row.iter()) {
                        *a += px as i64 * c;
                    }
                }
                let dst = &mut out[yy * cw..(yy + 1) * cw];
                for (d, &a) in dst.iter_mut().zip(acc.iter()) {
                    *d = clip8(a);
                }
            }
        }
        GrayImage { w: self.w, h, data: out }
    }

    /// Decode JPEG or PNG bytes to grayscale (PIL `convert("L")`: ITU-R 601-2 luma).
    ///
    /// Takes a single-pass `zune-jpeg` fast path for YCbCr and Luma JPEGs (matching
    /// `image::load_from_memory()->to_rgb8()->l24` bit-for-bit, `max_diff == 0`), and
    /// falls back to `image::load_from_memory` for PNG, CMYK/RGB JPEGs, or errors.
    pub fn decode(bytes: &[u8]) -> Result<GrayImage> {
        if let Some(img) = decode_jpeg_fast(bytes) {
            return Ok(img);
        }
        use ::image::DynamicImage;
        let img = ::image::load_from_memory(bytes).map_err(|e| Error::Os(format!("cannot identify image file: {e}")))?;
        let (w, h) = (img.width(), img.height());
        let data = match img {
            DynamicImage::ImageLuma8(b) => b.into_raw(),
            DynamicImage::ImageLumaA8(b) => b.into_raw().chunks_exact(2).map(|p| p[0]).collect(),
            DynamicImage::ImageRgb8(b) => b.into_raw().chunks_exact(3).map(l24).collect(),
            DynamicImage::ImageRgba8(b) => {
                let raw = b.into_raw();
                let mut out = vec![0u8; (w as usize) * (h as usize)];
                rgba_to_l24(&raw, &mut out);
                out
            }
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
        use ::image::codecs::png::{CompressionType, FilterType, PngEncoder};
        use ::image::ImageEncoder;
        let mut out = Vec::with_capacity((self.data.len() / 6).max(256));
        // Lossless PNG using fast fdeflate + Sub scanline filter (~16x faster than
        // default zlib level 6 + Adaptive while keeping compact ~7-8:1 grayscale size).
        let enc = PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Sub);
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
    let total_pixels = (bx.2 - bx.0) as u64 * (bx.3 - bx.1) as u64;
    let sx0 = bx.0 as usize;
    let sx1 = bx.2 as usize;
    let sy0 = bx.1.max(0) as usize;
    let sy1 = bx.3.max(0) as usize;
    let w = a.w as usize;
    let mut sum = 0u64;
    for row_y in sy0..sy1 {
        let base = row_y * w;
        let ra = &a.data[base + sx0..base + sx1];
        let rb = &b.data[base + sx0..base + sx1];
        let mut row_sum = 0u32;
        for (&pa, &pb) in ra.iter().zip(rb.iter()) {
            row_sum += pa.abs_diff(pb) as u32;
        }
        sum += row_sum as u64;
    }
    sum as f64 / total_pixels as f64
}

/// Resample the same in-bounds sub-rectangle of two same-sized images with `Filter::Box`,
/// sharing the precomputed box spans across both images and avoiding crop allocations.
fn resample_pair_box(
    a: &GrayImage,
    b: &GrayImage,
    rx: usize,
    ry: usize,
    rw: u32,
    rh: u32,
    ow: u32,
    oh: u32,
) -> (GrayImage, GrayImage) {
    if (ow, oh) == (rw, rh) || ow == 0 || oh == 0 || rw == 0 || rh == 0 {
        return (
            a.resample_region_box(rx, ry, rw, rh, ow, oh),
            b.resample_region_box(rx, ry, rw, rh, ow, oh),
        );
    }
    let vertical_first =
        ow != rw && oh != rh && (rw as u64) * (oh as u64) < (ow as u64) * (rh as u64);
    let h_spans = (ow != rw).then(|| precompute_box_spans(rw, ow));
    let v_spans = (oh != rh).then(|| precompute_box_spans(rh, oh));
    (
        a.resample_region_box_with(rx, ry, rw, rh, ow, oh, vertical_first, h_spans.as_deref(), v_spans.as_deref()),
        b.resample_region_box_with(rx, ry, rw, rh, ow, oh, vertical_first, h_spans.as_deref(), v_spans.as_deref()),
    )
}

/// Mean absolute grey difference (0-255) on a 1/8 thumbnail.
pub fn thumb_diff(a: Option<&GrayImage>, b: Option<&GrayImage>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else { return 255.0 };
    if a.size() != b.size() {
        return 255.0;
    }
    let (w, h) = ((a.w / 8).max(1), (a.h / 8).max(1));
    let (ta, tb) = resample_pair_box(a, b, 0, 0, a.w, a.h, w, h);
    if ta.data.is_empty() {
        return 0.0;
    }
    let sum: u64 = ta.data.iter().zip(&tb.data).map(|(&pa, &pb)| pa.abs_diff(pb) as u64).sum();
    sum as f64 / ta.data.len() as f64
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
    let (ca, cb) = resample_pair_box(before, cur, 0, top as usize, w as u32, (h - top) as u32, grid.0, grid.1);
    let mut peak = 0u8;
    let mut sum = 0u64;
    for (&pa, &pb) in ca.data.iter().zip(&cb.data) {
        let d = pa.abs_diff(pb);
        peak = peak.max(d);
        sum += d as u64;
    }
    if peak >= 14 {
        return true;
    }
    if !ca.data.is_empty() && (sum as f64 / ca.data.len() as f64) > 1.0 {
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
            let (la, lb) =
                resample_pair_box(before, cur, x0 as usize, y0 as usize, bw as u32, bh as u32, lgrid.0, lgrid.1);
            if la.data.iter().zip(&lb.data).any(|(&pa, &pb)| pa.abs_diff(pb) >= 18) {
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
    let (w, h) = (a.w as usize, a.h as usize);
    let y0 = (h as f64 * 0.06) as usize;
    let start = if y0 > 0 && y0 < h { y0 * w } else { 0 };
    let sa = &a.data[start..];
    let sb = &b.data[start..];
    // Chunked reduction lets LLVM emit SIMD abs-diff/max (vpsubusb + vpmaxub)
    // while still short-circuiting early on the first changed 64-byte block.
    for (ca, cb) in sa.chunks(64).zip(sb.chunks(64)) {
        let mut mx = 0u8;
        for (&pa, &pb) in ca.iter().zip(cb.iter()) {
            mx = mx.max(pa.abs_diff(pb));
        }
        if mx > max_diff {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Pillow resampling internals (libImaging/Resample.c)
// ---------------------------------------------------------------------------

const PRECISION_BITS: u32 = 32 - 8 - 2;

#[derive(Clone, Copy)]
enum Filter {
    #[cfg(test)]
    Box,
    Bilinear,
}

impl Filter {
    fn support(self) -> f64 {
        match self {
            #[cfg(test)]
            Filter::Box => 0.5,
            Filter::Bilinear => 1.0,
        }
    }
    fn eval(self, x: f64) -> f64 {
        match self {
            #[cfg(test)]
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

#[inline]
fn clip8_i32(ss: i32) -> u8 {
    (ss >> PRECISION_BITS).clamp(0, 255) as u8
}

/// Single-pass `zune-jpeg` decode for YCbCr and Luma JPEGs, matching
/// `image::load_from_memory()->to_rgb8()->l24` bit-for-bit (`max_diff == 0`).
///
/// Decodes YCbCr JPEGs straight into `ColorSpace::RGBA` (zune-jpeg's fastest 4-channel
/// AVX2 converter) inside a reusable thread-local scratch buffer, then runs AVX2 `l24`
/// RGB -> Luma conversion. Falls back (`None`) for non-JPEG, CMYK/RGB JPEGs, or errors.
fn decode_jpeg_fast(bytes: &[u8]) -> Option<GrayImage> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let opts = DecoderOptions::default()
        .set_strict_mode(false)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    dec.decode_headers().ok()?;
    let cs = dec.input_colorspace()?;
    let info = dec.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    let n = (w as usize).checked_mul(h as usize)?;
    match cs {
        ColorSpace::YCbCr => {
            let needed = n.checked_mul(4)?;
            let mut out = vec![0u8; n];
            let ok = JPEG_SCRATCH.with(|cell| {
                let mut buf = cell.borrow_mut();
                if buf.len() < needed {
                    buf.resize(needed, 0);
                }
                let ok = dec.decode_into(&mut buf[..needed]).is_ok();
                if ok {
                    rgba_to_l24(&buf[..needed], &mut out);
                }
                // Cap retained scratch at 16 MB so an occasional giant image does not pin RAM.
                const MAX_RETAIN: usize = 16 * 1024 * 1024;
                if buf.capacity() > MAX_RETAIN {
                    *buf = Vec::new();
                }
                ok
            });
            if !ok {
                return None;
            }
            Some(GrayImage { w, h, data: out })
        }
        ColorSpace::Luma => {
            let opts_l = DecoderOptions::default()
                .set_strict_mode(false)
                .jpeg_set_out_colorspace(ColorSpace::Luma);
            let mut dec_l = JpegDecoder::new_with_options(ZCursor::new(bytes), opts_l);
            let mut data = vec![0u8; n];
            dec_l.decode_into(&mut data).ok()?;
            Some(GrayImage { w, h, data })
        }
        _ => None,
    }
}

/// Bit-exact `l24` RGBA -> Luma conversion with AVX2 acceleration on x86_64 (~5x faster
/// than scalar `chunks_exact(3).map(l24)`).
fn rgba_to_l24(rgba: &[u8], out: &mut [u8]) {
    debug_assert_eq!(rgba.len(), out.len() * 4);
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 availability verified at runtime; bounds checked via `chunks = n / 32`.
        unsafe { rgba_to_l24_avx2(rgba, out) };
        return;
    }
    for (p, dst) in rgba.chunks_exact(4).zip(out.iter_mut()) {
        *dst = l24(p);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn rgba_to_l24_avx2(rgba: &[u8], out: &mut [u8]) {
    use std::arch::x86_64::*;

    let n = out.len().min(rgba.len() / 4);
    let chunks = n / 32;
    let mask_ff = _mm256_set1_epi32(0xFF);
    let wr = _mm256_set1_epi32(19595);
    let wg = _mm256_set1_epi32(38470);
    let wb = _mm256_set1_epi32(7471);
    let bias = _mm256_set1_epi32(0x8000);
    let perm = _mm256_setr_epi32(0, 4, 1, 5, 2, 6, 3, 7);

    let mut src = rgba.as_ptr();
    let mut dst = out.as_mut_ptr();

    for _ in 0..chunks {
        let p0 = _mm256_loadu_si256(src as *const __m256i);
        let p1 = _mm256_loadu_si256(src.add(32) as *const __m256i);
        let p2 = _mm256_loadu_si256(src.add(64) as *const __m256i);
        let p3 = _mm256_loadu_si256(src.add(96) as *const __m256i);

        macro_rules! luma8 {
            ($p:expr) => {{
                let r = _mm256_and_si256($p, mask_ff);
                let g = _mm256_and_si256(_mm256_srli_epi32($p, 8), mask_ff);
                let b = _mm256_and_si256(_mm256_srli_epi32($p, 16), mask_ff);
                let s = _mm256_add_epi32(
                    _mm256_add_epi32(_mm256_mullo_epi32(r, wr), _mm256_mullo_epi32(g, wg)),
                    _mm256_add_epi32(_mm256_mullo_epi32(b, wb), bias),
                );
                _mm256_srli_epi32(s, 16)
            }};
        }

        let y0 = luma8!(p0);
        let y1 = luma8!(p1);
        let y2 = luma8!(p2);
        let y3 = luma8!(p3);

        let y01 = _mm256_packus_epi32(y0, y1);
        let y23 = _mm256_packus_epi32(y2, y3);
        let y0123 = _mm256_packus_epi16(y01, y23);
        let ordered = _mm256_permutevar8x32_epi32(y0123, perm);
        _mm256_storeu_si256(dst as *mut __m256i, ordered);

        src = src.add(128);
        dst = dst.add(32);
    }

    let done = chunks * 32;
    for (p, d) in rgba[done * 4..n * 4].chunks_exact(4).zip(&mut out[done..n]) {
        *d = l24(p);
    }
}

/// For `Filter::Box`, every non-zero tap has the same fixed-point weight and forms a
/// single contiguous span `[lo..hi)`. Bit-exact with `precompute_coeffs(..., Filter::Box)`.
fn precompute_box_spans(in_size: u32, out_size: u32) -> Vec<(usize, usize, i32)> {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = 0.5 * filterscale;
    let ss = 1.0 / filterscale;
    let mut spans = Vec::with_capacity(out_size as usize);
    for xx in 0..out_size as usize {
        let center = (xx as f64 + 0.5) * scale;
        let xmin = ((center - support + 0.5) as i64).max(0);
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64);
        let n = (xmax - xmin).max(0) as usize;
        let mut first: Option<usize> = None;
        let mut last = 0usize;
        let mut count = 0usize;
        for x in 0..n {
            let u = (x as f64 + xmin as f64 - center + 0.5) * ss;
            if u > -0.5 && u <= 0.5 {
                let idx = xmin as usize + x;
                if first.is_none() {
                    first = Some(idx);
                }
                last = idx + 1;
                count += 1;
            }
        }
        if let Some(lo) = first {
            let weight = ((1.0 / count as f64) * (1u64 << PRECISION_BITS) as f64 + 0.5) as i32;
            spans.push((lo, last, weight));
        } else {
            spans.push((0, 0, 0));
        }
    }
    spans
}

fn pass_horizontal_box_region(
    data: &[u8],
    stride: usize,
    rx: usize,
    ry: usize,
    rh: u32,
    spans: &[(usize, usize, i32)],
) -> GrayImage {
    let ow = spans.len();
    let mut out = vec![0u8; ow * rh as usize];
    let bias: i64 = 1 << (PRECISION_BITS - 1);
    for y in 0..rh as usize {
        let row = &data[(ry + y) * stride + rx..];
        let dst = &mut out[y * ow..(y + 1) * ow];
        for (d, &(lo, hi, weight)) in dst.iter_mut().zip(spans.iter()) {
            let mut sum = 0u32;
            for &px in &row[lo..hi] {
                sum += px as u32;
            }
            *d = clip8(bias + sum as i64 * weight as i64);
        }
    }
    GrayImage { w: ow as u32, h: rh, data: out }
}

fn pass_vertical_box_region(
    data: &[u8],
    stride: usize,
    rx: usize,
    ry: usize,
    rw: u32,
    spans: &[(usize, usize, i32)],
) -> GrayImage {
    let cw = rw as usize;
    let oh = spans.len();
    let mut out = vec![0u8; cw * oh];
    let mut acc = vec![0u32; cw];
    let bias: i64 = 1 << (PRECISION_BITS - 1);
    for (yy, &(lo, hi, weight)) in spans.iter().enumerate() {
        acc.fill(0);
        for y in lo..hi {
            let start = (ry + y) * stride + rx;
            let row = &data[start..start + cw];
            for (a, &px) in acc.iter_mut().zip(row.iter()) {
                *a += px as u32;
            }
        }
        let w64 = weight as i64;
        let dst = &mut out[yy * cw..(yy + 1) * cw];
        for (d, &a) in dst.iter_mut().zip(acc.iter()) {
            *d = clip8(bias + a as i64 * w64);
        }
    }
    GrayImage { w: rw, h: oh as u32, data: out }
}

/// Pillow `L24` RGB -> L: `(r*19595 + g*38470 + b*7471 + 0x8000) >> 16`.
fn l24(p: &[u8]) -> u8 {
    ((p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16) as u8
}

#[cfg(test)]
mod img_opt_tests {
    use super::*;

    fn make_pattern(w: u32, h: u32) -> GrayImage {
        let mut data = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                data.push(((x.wrapping_mul(37) ^ y.wrapping_mul(73)).wrapping_add(x + y)) as u8);
            }
        }
        GrayImage::from_raw(w, h, data)
    }

    #[test]
    fn box_and_bilinear_resample_bit_exact_vs_reference() {
        // Verify our fast box-span path and row-major vertical pass match the
        // original separable convolution formula bit-for-bit across odd/even/upscale/downscale sizes.
        for (iw, ih, ow, oh) in [
            (645u32, 1398u32, 80u32, 174u32),
            (645, 1315, 24, 48),
            (165, 54, 41, 13),
            (17, 19, 53, 41),
            (100, 100, 100, 50),
            (100, 100, 50, 100),
            (100, 100, 100, 100),
            (7, 9, 1, 1),
        ] {
            let img = make_pattern(iw, ih);
            let fast_box = img.resize_box(ow, oh);
            let ref_box = img.resample(ow, oh, Filter::Box);
            assert_eq!(fast_box, ref_box, "box mismatch for ({iw},{ih})->({ow},{oh})");

            // Check crop + resize_box == resample_region_box for a non-zero sub-rect.
            if iw >= 20 && ih >= 20 {
                let (rx, ry, rw, rh) = (5usize, 7usize, iw - 11, ih - 13);
                let fast_reg = img.resample_region_box(rx, ry, rw, rh, ow.min(rw), oh.min(rh));
                let ref_reg = img
                    .crop(rx as i64, ry as i64, (rx + rw as usize) as i64, (ry + rh as usize) as i64)
                    .resample(ow.min(rw), oh.min(rh), Filter::Box);
                assert_eq!(fast_reg, ref_reg, "region box mismatch for ({iw},{ih})");
            }
        }
    }

    #[test]
    fn color_jpeg_preserves_exact_rgb_l24_on_saturated_chroma_edges_and_gray_uses_fast_path() {
        // Saturated high-chroma 4:2:0 edges (red/cyan, blue/yellow, green/magenta sharp bars)
        // where raw Y plane diverges by >20 gray levels from RGB -> l24 due to chroma upsampling/clipping.
        let (w, h) = (160u32, 240u32);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = if y < 60 {
                    if (x / 4) % 2 == 0 { (255, 0, 0) } else { (0, 255, 255) }
                } else if y < 120 {
                    if ((x + y) / 3) % 2 == 0 { (0, 0, 255) } else { (255, 255, 0) }
                } else if y < 180 {
                    if (y / 2) % 2 == 0 { (0, 255, 0) } else { (255, 0, 255) }
                } else {
                    (255u8.wrapping_sub(x as u8), (y as u8).wrapping_mul(3), (x ^ y) as u8)
                };
                rgb.extend_from_slice(&[r, g, b]);
            }
        }
        let mut jpeg = Vec::new();
        let mut enc = ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75);
        enc.encode(&rgb, w, h, ::image::ExtendedColorType::Rgb8).unwrap();

        // Color JPEG takes single-pass zune-jpeg RGBA + AVX2 l24 and must match image::load_from_memory
        // -> RGB -> l24 bit-for-bit (max_diff == 0) even on saturated 4:2:0 chroma edges.
        let fast = decode_jpeg_fast(&jpeg).expect("color jpeg fast path");
        let decoded = GrayImage::decode(&jpeg).expect("color jpeg decode");
        let ref_rgb = ::image::load_from_memory(&jpeg).unwrap().to_rgb8();
        let ref_luma: Vec<u8> = ref_rgb.into_raw().chunks_exact(3).map(l24).collect();
        assert_eq!((decoded.w, decoded.h), (w, h));
        assert_eq!(fast.data, ref_luma, "single-pass color JPEG must match RGB -> l24 bit-for-bit");
        assert_eq!(decoded.data, ref_luma, "color JPEG grayscale pixels must match RGB -> l24 bit-for-bit");

        // PNG still decodes via fallback and fast lossless PNG round-trips bit-for-bit.
        let png = decoded.encode_png();
        assert!(decode_jpeg_fast(&png).is_none());
        assert_eq!(GrayImage::decode(&png).unwrap(), decoded);

        // Single-channel grayscale JPEG takes the direct zune-jpeg fast path and matches bit-for-bit.
        let gray_jpeg = decoded.encode_jpeg(80);
        let dec_gray = decode_jpeg_fast(&gray_jpeg).expect("gray jpeg fast path");
        let ref_gray = ::image::load_from_memory(&gray_jpeg).unwrap().into_luma8().into_raw();
        assert_eq!((dec_gray.w, dec_gray.h), (w, h));
        assert_eq!(dec_gray.data, ref_gray);

        // Verify rgba_to_l24 AVX2 / tail path matches scalar l24 for every tail length 0..67.
        for len in [0usize, 1, 7, 31, 32, 33, 63, 64, 67] {
            let rgba: Vec<u8> = (0..len * 4).map(|i| ((i * 37 + 13) & 255) as u8).collect();
            let mut got = vec![0u8; len];
            rgba_to_l24(&rgba, &mut got);
            let want: Vec<u8> = rgba.chunks_exact(4).map(l24).collect();
            assert_eq!(got, want, "rgba_to_l24 mismatch at len={len}");
        }
    }

    #[test]
    fn zero_alloc_frame_diffs_match_crop_reference() {
        let a = make_pattern(200, 400);
        let mut b = a.clone();
        assert!(content_pixel_still(Some(&a), Some(&b), 0));
        assert_eq!(box_diff(Some(&a), Some(&b), 100.0, 200.0, 1.0, 30.0, 18.0, 0), 0.0);
        // Status bar mutation (y < 24 = 6% of 400) ignored by content_pixel_still.
        b.put(10, 5, b.get(10, 5).wrapping_add(50));
        assert!(content_pixel_still(Some(&a), Some(&b), 6));
        // Content mutation at y = 24 detected.
        b.put(10, 24, b.get(10, 24).wrapping_add(7));
        assert!(!content_pixel_still(Some(&a), Some(&b), 6));
        assert!(content_pixel_still(Some(&a), Some(&b), 7));

        // Negative min_y box_diff matches crop semantics (out-of-bounds pixels are 0 in both).
        let mut c = a.clone();
        c.fill_rect(10, 0, 30, 10, 255);
        let ref_diff = a.crop(0, -10, 40, 20).difference(&c.crop(0, -10, 40, 20)).mean();
        let got_diff = box_diff(Some(&a), Some(&c), 20.0, 5.0, 1.0, 20.0, 15.0, -10);
        assert!((ref_diff - got_diff).abs() < 1e-12, "ref={ref_diff} got={got_diff}");
    }
}

