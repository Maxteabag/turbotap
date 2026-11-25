//! Screen reading for any app: device-downscaled JPEG + parallel in-process OCR.
//!
//! Ports cli.py L1017-1927: `_tess_lib` (here: `libloading` dlopen of
//! libtesseract, so neither build nor run needs Tesseract; `WDA_OCR=binary` or a
//! missing library forks the `tesseract` binary instead), `tesseract_tsv`,
//! `_create_tess_api`, `_tess_model_key`, `_WarmOCR`, and `ScreenReader` (`pt_size`,
//! `grab` incl. ETag/304 conditional `/wda/fastScreenshot`, `_cuts`, `_ocr_raw` with
//! dark-mode pre-inversion and uniform-strip skip, `read` with the strip-hash cache).
//!
//! The strip cache keys strips by BLAKE2b digest (Python used SHA-256; the key is
//! internal). Sauvola reads use a separate key prefix.
use crate::error::Result;
use crate::image::Frame;
use crate::text::Line;
use crate::wda::WdaClient;
use std::collections::HashMap;

/// Options for [`Reader::read`] (Python `read(y0=0, y1=None, img=None, sauvola=False,
/// timeout=None, deadline=None)`).
#[derive(Clone, Debug, Default)]
pub struct ReadOpts {
    pub y0: i64,
    pub y1: Option<i64>,
    pub img: Option<Frame>,
    pub sauvola: bool,
    pub timeout: Option<f64>,
    pub deadline: Option<f64>,
}

impl ReadOpts {
    pub fn full() -> ReadOpts {
        ReadOpts::default()
    }
    pub fn img(img: Frame) -> ReadOpts {
        ReadOpts { img: Some(img), ..Default::default() }
    }
    pub fn band(y0: i64, y1: i64) -> ReadOpts {
        ReadOpts { y0, y1: Some(y1), ..Default::default() }
    }
}

/// What `Flow` needs from a screen reader: implemented by [`ScreenReader`] and by
/// the test double `testutil::FakeReader`.
pub trait Reader: Send {
    /// OCR pixels per point.
    fn scale(&self) -> f64;
    /// Screen size in points.
    fn pt_size(&mut self, c: &mut WdaClient, force_refresh: bool, deadline: Option<f64>) -> Result<(i64, i64)>;
    /// A fresh grayscale frame of the screen (or band y0..y1 in points).
    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, timeout: Option<f64>, deadline: Option<f64>) -> Result<Frame>;
    /// OCR lines (grabbing a fresh frame unless `opts.img` is given).
    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>>;
    fn last(&self) -> Option<Frame>;
    fn last_ts(&self) -> f64;
    fn last_lines(&self) -> Option<Vec<Line>>;
    /// `set_last(img, lines=None, ts=None)`; `ts` None = now.
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>);
    fn invalidate_last(&mut self);
}

// ---------------------------------------------------------------------------
// libtesseract via dlopen
// ---------------------------------------------------------------------------

/// The loaded libtesseract C API (function pointers resolved once).
pub struct TessLib {
    _private: (),
}

/// libtesseract, or None (`WDA_OCR=binary` is checked on every call so a warm
/// daemon honours each caller; load failures are cached).
pub fn tess_lib() -> Option<&'static TessLib> {
    todo!("port L1023 _tess_lib with libloading (also silence leptonica: setMsgSeverity(6))")
}

/// One initialised `TessBaseAPI*` (LSTM-only, eng, given PSM, debug_file=/dev/null).
pub struct TessApi {
    _ptr: *mut std::ffi::c_void,
}

// SAFETY: a TessBaseAPI is used by one thread at a time (engines move through queues).
unsafe impl Send for TessApi {}

/// L1393.
pub fn create_tess_api(psm: i32) -> Result<TessApi> {
    let _ = psm;
    todo!("port L1393")
}

/// realpath(TESSDATA_PREFIX) or "" (L1405).
pub fn tess_model_key() -> String {
    match std::env::var("TESSDATA_PREFIX") {
        Ok(p) if !p.is_empty() => std::fs::canonicalize(&p)
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_else(|_| {
                std::env::current_dir().map(|d| d.join(&p).to_string_lossy().into_owned()).unwrap_or(p)
            }),
        _ => String::new(),
    }
}

/// TSV from the `tesseract` binary for an 8-bit grayscale image (L1077 fallback).
pub fn tesseract_tsv_binary(raw: &[u8], w: u32, h: u32, psm: i32, sauvola: bool) -> Result<String> {
    let _ = (raw, w, h, psm, sauvola);
    todo!("port L1077 binary branch (write PGM/BMP to stdin)")
}

/// One OCR line as raw strip-pixel word boxes `(text, x0, y0, x1, y1)`.
pub type RawWords = Vec<(String, i64, i64, i64, i64)>;

/// Parse Tesseract TSV level-5 rows (conf > 30) grouped by (block, par, line).
pub fn parse_tsv(tsv: &str) -> Vec<RawWords> {
    let _ = tsv;
    todo!("port L1844-1854")
}

// ---------------------------------------------------------------------------
// Warm daemon engines
// ---------------------------------------------------------------------------

/// Pre-initialised engines for the warm daemon, partitioned by (PSM, model key).
pub struct WarmOcr {
    _private: (),
}

impl WarmOcr {
    /// Cancel/finish every OCR job a request started; all engines back in their pools.
    pub fn end_request(&self) {
        todo!("port L1446")
    }
    pub fn close(&self) {
        todo!("port L1452")
    }
}

/// Install (daemon start) or remove the process-wide warm engines.
pub fn set_warm_ocr(on: bool) {
    let _ = on;
    todo!("install/remove global WarmOcr")
}

/// The warm engines, when running inside the daemon.
pub fn warm_ocr() -> Option<&'static WarmOcr> {
    None
}

// ---------------------------------------------------------------------------
// ScreenReader
// ---------------------------------------------------------------------------

/// This reader's last raw conditional frame: (request key, ETag, bytes, content type).
#[derive(Clone, Debug)]
pub struct ShotValidator {
    pub key: String,
    pub etag: String,
    pub body: Vec<u8>,
    pub content_type: String,
}

pub struct ScreenReader {
    pub scale: f64,
    pub psm: i32,
    /// OCR workers (`WDA_OCR_WORKERS` or clamp(cpu, 2, 6)).
    pub n: usize,
    pub cache: HashMap<Vec<u8>, Vec<RawWords>>,
    pub last: Option<Frame>,
    pub last_ts: f64,
    pub last_lines: Option<Vec<Line>>,
    pub pt: Option<(i64, i64)>,
    pub pt_from_cache: bool,
    /// Device downscale factor sent to fastScreenshot (scale/3, auto-corrected).
    pub dev: f64,
    pub shot_validator: Option<ShotValidator>,
    pub conditional_hits: u64,
}

impl ScreenReader {
    /// Python `ScreenReader(client, scale=1.5, workers=None, psm=11, warm_ocr=True)`.
    pub fn new(scale: f64, workers: Option<usize>, psm: i32, warm_ocr: bool) -> ScreenReader {
        let _ = (scale, workers, psm, warm_ocr);
        todo!("port L1473")
    }

    /// Default reader (scale 1.5, psm 11, warm engines).
    pub fn default_reader() -> ScreenReader {
        ScreenReader::new(1.5, None, 11, true)
    }

    /// Strip boundaries at the most uniform row near each 1/n of the height.
    pub fn cuts(&self, w: u32, h: u32, data: &[u8]) -> Vec<u32> {
        let _ = (w, h, data);
        todo!("port L1758 _cuts")
    }

    /// OCR one strip (uniform-strip skip, dark-mode pre-inversion, Sauvola variables).
    pub fn ocr_raw(&self, raw: &[u8], w: u32, h: u32, sauvola: bool) -> Result<Vec<RawWords>> {
        let _ = (raw, w, h, sauvola);
        todo!("port L1798 _ocr_raw")
    }

    /// L1701.
    pub fn conditional_ok(&self, c: &WdaClient) -> bool {
        let _ = c;
        todo!("port L1701")
    }

    /// L1709: conditional fastScreenshot; 304 with the matching ETag -> retained bytes.
    pub fn conditional_shot(
        &mut self,
        c: &mut WdaClient,
        payload: &serde_json::Value,
        req_timeout: Option<f64>,
        deadline: Option<f64>,
    ) -> (u16, String, Vec<u8>) {
        let _ = (c, payload, req_timeout, deadline);
        todo!("port L1709")
    }
}

impl Reader for ScreenReader {
    fn scale(&self) -> f64 {
        self.scale
    }
    fn pt_size(&mut self, c: &mut WdaClient, force_refresh: bool, deadline: Option<f64>) -> Result<(i64, i64)> {
        let _ = (c, force_refresh, deadline);
        todo!("port L1533")
    }
    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, timeout: Option<f64>, deadline: Option<f64>) -> Result<Frame> {
        let _ = (c, y0, y1, timeout, deadline);
        todo!("port L1569")
    }
    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>> {
        let _ = (c, opts);
        todo!("port L1856")
    }
    fn last(&self) -> Option<Frame> {
        self.last.clone()
    }
    fn last_ts(&self) -> f64 {
        self.last_ts
    }
    fn last_lines(&self) -> Option<Vec<Line>> {
        self.last_lines.clone()
    }
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>) {
        self.last = img;
        self.last_lines = lines;
        self.last_ts = ts.unwrap_or_else(crate::clock::now);
    }
    fn invalidate_last(&mut self) {
        self.last_ts = 0.0;
        self.last_lines = None;
    }
}

/// `turbotap ocr` (L5559).
pub fn cmd_ocr(client: &mut WdaClient, scale: f64, y0: i64, y1: Option<i64>) -> Result<()> {
    let _ = (client, scale, y0, y1);
    todo!("port L5559 cmd_ocr")
}
