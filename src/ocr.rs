//! Screen reading for any app: device-downscaled JPEG + parallel in-process OCR.
//!
//! Ports v0.10.0 cli.py L1017-1927: `_tess_lib` (here: `libloading` dlopen of
//! libtesseract, so neither build nor run needs Tesseract; `WDA_OCR=binary` or a
//! missing library forks the `tesseract` binary instead), `tesseract_tsv`,
//! `_create_tess_api`, `_tess_model_key`, `_WarmOCR`, and `ScreenReader` (`pt_size`,
//! `grab` incl. ETag/304 conditional `/wda/fastScreenshot`, `_cuts`, `_ocr_raw` with
//! dark-mode pre-inversion and uniform-strip skip, `read` with the strip-hash cache).
//!
//! The strip cache keys strips by a fast 128-bit streaming hash (Python used SHA-256;
//! the key is internal). Sauvola reads use a separate key prefix.
//!
//! Strips of one `read` run on scoped threads, so no OCR work ever outlives the
//! call that started it (Python needed per-request pool draining for that). Engines
//! live in [`EnginePool`]s: one per reader, or leased per (PSM, model key) from the
//! daemon's [`WarmOcr`].
use crate::clock;
use crate::error::{Error, Result};
use crate::image::{Frame, GrayImage};
use crate::json;
use crate::text::{Line, Word};
use crate::wda::WdaClient;
use serde_json::{json as j, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_float, c_int, c_void, CStr};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;

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

type FnCreate = unsafe extern "C" fn() -> *mut c_void;
type FnInit2 = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int;
type FnSetPsm = unsafe extern "C" fn(*mut c_void, c_int);
type FnSetVar = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnSetImage = unsafe extern "C" fn(*mut c_void, *const u8, c_int, c_int, c_int, c_int);
type FnGetTsv = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_char;
type FnRecognize = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type FnGetIterator = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnRiPageIter = unsafe extern "C" fn(*const c_void) -> *const c_void;
type FnPiAtBeginning = unsafe extern "C" fn(*const c_void, c_int) -> c_int;
type FnPiBoundingBox = unsafe extern "C" fn(*const c_void, c_int, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> c_int;
type FnRiGetText = unsafe extern "C" fn(*const c_void, c_int) -> *mut c_char;
type FnRiConfidence = unsafe extern "C" fn(*const c_void, c_int) -> c_float;
type FnRiNext = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnRiDelete = unsafe extern "C" fn(*mut c_void);
type FnDeleteText = unsafe extern "C" fn(*mut c_char);
type FnDelete = unsafe extern "C" fn(*mut c_void);

const RIL_TEXTLINE: c_int = 2;
const RIL_WORD: c_int = 3;

/// Optional direct `TessResultIterator` C-API symbols (skips TSV formatting + `parse_tsv`).
struct TessIterApi {
    recognize: FnRecognize,
    get_iter: FnGetIterator,
    ri_page_iter: FnRiPageIter,
    pi_at_beg: FnPiAtBeginning,
    pi_bbox: FnPiBoundingBox,
    ri_text: FnRiGetText,
    ri_conf: FnRiConfidence,
    ri_next: FnRiNext,
    ri_delete: FnRiDelete,
}

/// The loaded libtesseract C API (function pointers resolved once).
pub struct TessLib {
    create: FnCreate,
    init2: FnInit2,
    set_psm: FnSetPsm,
    set_var: FnSetVar,
    set_image: FnSetImage,
    get_tsv: FnGetTsv,
    iter: Option<TessIterApi>,
    delete_text: FnDeleteText,
    delete: FnDelete,
    _lib: libloading::Library,
    _lept: Option<libloading::Library>,
}

const TESS_NAMES: &[&str] = &[
    "libtesseract.so.5",
    "libtesseract.so",
    "libtesseract.so.4",
    "libtesseract.5.dylib",
    "libtesseract.dylib",
    "/opt/homebrew/lib/libtesseract.dylib",
    "/usr/local/lib/libtesseract.dylib",
];

const LEPT_NAMES: &[&str] = &[
    "libleptonica.so.6",
    "libleptonica.so",
    "liblept.so.5",
    "liblept.so",
    "libleptonica.6.dylib",
    "libleptonica.dylib",
    "liblept.5.dylib",
    "/opt/homebrew/lib/libleptonica.dylib",
    "/usr/local/lib/libleptonica.dylib",
];

static TESS: OnceLock<Option<TessLib>> = OnceLock::new();

/// Python sets `OMP_THREAD_LIMIT=1` at import: Tesseract's own OpenMP threads made a
/// full-screen pass 312 ms vs 181 ms single-threaded, and they fight the strip
/// threads. It must be in the environment before libtesseract (libgomp) loads.
pub fn ensure_omp_thread_limit() {
    if std::env::var_os("OMP_THREAD_LIMIT").is_none() {
        std::env::set_var("OMP_THREAD_LIMIT", "1");
    }
}

fn load_tess() -> Option<TessLib> {
    ensure_omp_thread_limit();
    // SAFETY: loading a system library by name; its initialisers are libtesseract's.
    let lib = TESS_NAMES.iter().find_map(|n| unsafe { libloading::Library::new(n) }.ok())?;
    // SAFETY: the symbol types match the libtesseract C API (capi.h).
    let api = unsafe {
        (|| -> std::result::Result<_, libloading::Error> {
            Ok((
                *lib.get::<FnCreate>(b"TessBaseAPICreate\0")?,
                *lib.get::<FnInit2>(b"TessBaseAPIInit2\0")?,
                *lib.get::<FnSetPsm>(b"TessBaseAPISetPageSegMode\0")?,
                *lib.get::<FnSetVar>(b"TessBaseAPISetVariable\0")?,
                *lib.get::<FnSetImage>(b"TessBaseAPISetImage\0")?,
                *lib.get::<FnGetTsv>(b"TessBaseAPIGetTsvText\0")?,
                *lib.get::<FnDeleteText>(b"TessDeleteText\0")?,
                *lib.get::<FnDelete>(b"TessBaseAPIDelete\0")?,
            ))
        })()
    }
    .ok()?;
    // Optional direct ResultIterator API (present in libtesseract 4/5; falls back to GetTsvText if missing).
    let iter = unsafe {
        (|| -> std::result::Result<TessIterApi, libloading::Error> {
            Ok(TessIterApi {
                recognize: *lib.get::<FnRecognize>(b"TessBaseAPIRecognize\0")?,
                get_iter: *lib.get::<FnGetIterator>(b"TessBaseAPIGetIterator\0")?,
                ri_page_iter: *lib.get::<FnRiPageIter>(b"TessResultIteratorGetPageIteratorConst\0")?,
                pi_at_beg: *lib.get::<FnPiAtBeginning>(b"TessPageIteratorIsAtBeginningOf\0")?,
                pi_bbox: *lib.get::<FnPiBoundingBox>(b"TessPageIteratorBoundingBox\0")?,
                ri_text: *lib.get::<FnRiGetText>(b"TessResultIteratorGetUTF8Text\0")?,
                ri_conf: *lib.get::<FnRiConfidence>(b"TessResultIteratorConfidence\0")?,
                ri_next: *lib.get::<FnRiNext>(b"TessResultIteratorNext\0")?,
                ri_delete: *lib.get::<FnRiDelete>(b"TessResultIteratorDelete\0")?,
            })
        })()
    }
    .ok();
    // Leptonica prints "Error in boxClipToRectangle" for harmless empty boxes.
    let lept = LEPT_NAMES.iter().find_map(|n| unsafe { libloading::Library::new(n) }.ok());
    if let Some(l) = lept.as_ref() {
        // SAFETY: `l_int32 setMsgSeverity(l_int32)`.
        if let Ok(f) = unsafe { l.get::<unsafe extern "C" fn(c_int) -> c_int>(b"setMsgSeverity\0") } {
            unsafe { f(6) };
        }
    }
    let (create, init2, set_psm, set_var, set_image, get_tsv, delete_text, delete) = api;
    Some(TessLib { create, init2, set_psm, set_var, set_image, get_tsv, iter, delete_text, delete, _lib: lib, _lept: lept })
}

/// libtesseract, or None (callers then fork the `tesseract` binary).
///
/// `WDA_OCR=binary` is the benchmark baseline: checked on every call, before the
/// native cache, so a warm daemon honours each caller's choice and the flag never
/// poisons the cache. Load failures are cached.
pub fn tess_lib() -> Option<&'static TessLib> {
    if crate::config::env("WDA_OCR").as_deref() == Some("binary") {
        return None;
    }
    TESS.get_or_init(load_tess).as_ref()
}

/// True once libtesseract was loaded (or found missing) in this process, without
/// loading it (Python `"lib" in _TESS`).
pub fn tess_lib_probed() -> bool {
    TESS.get().is_some()
}

/// One initialised `TessBaseAPI*` (LSTM-only, eng, given PSM, debug_file=/dev/null),
/// deleted on drop. Test engines ([`TessApi::fake`]) carry only a tag.
pub struct TessApi {
    ptr: *mut c_void,
    lib: Option<&'static TessLib>,
    /// Reusable dark-mode inversion buffer (owned per engine; eliminates ~150 KB
    /// per-strip heap allocs/page faults on warm dark-mode OCR).
    inv_buf: Vec<u8>,
    /// Identity of a fake engine (empty for real ones).
    pub tag: String,
}

// SAFETY: a TessBaseAPI is used by one thread at a time (engines move through pools).
unsafe impl Send for TessApi {}

impl TessApi {
    /// A stand-in engine for tests of the pooling logic (Python
    /// `mock.patch.object(cli, "_create_tess_api", side_effect=...)`).
    pub fn fake(tag: impl Into<String>) -> TessApi {
        TessApi { ptr: std::ptr::null_mut(), lib: None, inv_buf: Vec::new(), tag: tag.into() }
    }

    pub fn is_fake(&self) -> bool {
        self.lib.is_none() || self.ptr.is_null()
    }

    fn set_variable(&mut self, name: &CStr, value: &CStr) {
        if let Some(lib) = self.lib.filter(|_| !self.ptr.is_null()) {
            // SAFETY: valid engine pointer and NUL-terminated strings.
            unsafe { (lib.set_var)(self.ptr, name.as_ptr(), value.as_ptr()) };
        }
    }

    /// Recognise from this engine's reusable `inv_buf[..len]` without extra allocations.
    fn words_strided_inv(&mut self, len: usize, w: u32, h: u32, stride: u32) -> Result<Vec<RawWords>> {
        let inv = std::mem::take(&mut self.inv_buf);
        let res = self.words_strided(&inv[..len], w, h, stride);
        self.inv_buf = inv;
        res
    }

    /// Recognise an 8-bit grayscale sub-image `(w x h)` with row stride `stride` bytes
    /// and return level-5 words (`conf > 30`) grouped by textline, using the direct
    /// `TessResultIterator` C API when available (falling back to `GetTsvText` + `parse_tsv`).
    fn words_strided(&mut self, raw: &[u8], w: u32, h: u32, stride: u32) -> Result<Vec<RawWords>> {
        let Some(lib) = self.lib.filter(|_| !self.ptr.is_null()) else {
            return Err(Error::runtime("fake Tesseract engine cannot OCR"));
        };
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        let min_len = (h as usize - 1).saturating_mul(stride as usize).saturating_add(w as usize);
        if stride < w || raw.len() < min_len {
            return Err(Error::value("OCR buffer smaller than its size"));
        }
        // SAFETY: `raw` outlives `SetImage` (which copies `w x h` bytes with `stride`
        // into an internal Pix); iterator and UTF-8 strings are freed via C API destructors.
        unsafe {
            (lib.set_image)(self.ptr, raw.as_ptr(), w as c_int, h as c_int, 1, stride as c_int);
            if let Some(it) = lib.iter.as_ref() {
                if (it.recognize)(self.ptr, std::ptr::null_mut()) != 0 {
                    return Ok(Vec::new());
                }
                let ri = (it.get_iter)(self.ptr);
                if ri.is_null() {
                    return Ok(Vec::new());
                }
                struct IterGuard(FnRiDelete, *mut c_void);
                impl Drop for IterGuard {
                    fn drop(&mut self) {
                        unsafe { (self.0)(self.1) };
                    }
                }
                let _guard = IterGuard(it.ri_delete, ri);
                let pi = (it.ri_page_iter)(ri);
                let mut out: Vec<RawWords> = Vec::new();
                let mut cur_line: RawWords = Vec::new();
                loop {
                    if (it.pi_at_beg)(pi, RIL_TEXTLINE) != 0 && !cur_line.is_empty() {
                        out.push(std::mem::take(&mut cur_line));
                    }
                    let conf = (it.ri_conf)(ri, RIL_WORD) as f64;
                    if conf > 30.0 {
                        let tp = (it.ri_text)(ri, RIL_WORD);
                        if !tp.is_null() {
                            let txt = CStr::from_ptr(tp).to_string_lossy();
                            let trimmed = txt.trim();
                            if !trimmed.is_empty() {
                                let (mut x0, mut y0, mut x1, mut y1) = (0, 0, 0, 0);
                                if (it.pi_bbox)(pi, RIL_WORD, &mut x0, &mut y0, &mut x1, &mut y1) != 0 {
                                    cur_line.push((trimmed.to_string(), x0 as i64, y0 as i64, x1 as i64, y1 as i64));
                                }
                            }
                            (lib.delete_text)(tp);
                        }
                    }
                    if (it.ri_next)(ri, RIL_WORD) == 0 {
                        break;
                    }
                }
                if !cur_line.is_empty() {
                    out.push(cur_line);
                }
                return Ok(out);
            }
            let p = (lib.get_tsv)(self.ptr, 0);
            if p.is_null() {
                return Ok(Vec::new());
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (lib.delete_text)(p);
            Ok(parse_tsv(&s))
        }
    }
}

impl Drop for TessApi {
    fn drop(&mut self) {
        if let Some(lib) = self.lib {
            if !self.ptr.is_null() {
                // SAFETY: we own this engine and nobody else holds it.
                unsafe { (lib.delete)(self.ptr) };
            }
        }
    }
}

/// `_create_tess_api(psm)`: OEM_LSTM_ONLY (1) matches the CLI `--oem 1` fallback and
/// avoids loading legacy engine tables from combined eng.traineddata.
pub fn create_tess_api(psm: i32) -> Result<TessApi> {
    let Some(lib) = tess_lib() else {
        return Err(Error::runtime("libtesseract is not available"));
    };
    // SAFETY: C API calls on a freshly created engine.
    unsafe {
        let ptr = (lib.create)();
        if ptr.is_null() {
            return Err(Error::runtime("TessBaseAPICreate failed"));
        }
        let api = TessApi { ptr, lib: Some(lib), inv_buf: Vec::new(), tag: String::new() };
        if (lib.init2)(ptr, std::ptr::null(), c"eng".as_ptr(), 1) != 0 {
            return Err(Error::runtime("TessBaseAPIInit2 failed (eng.traineddata missing?)"));
        }
        (lib.set_psm)(ptr, psm as c_int);
        (lib.set_var)(ptr, c"debug_file".as_ptr(), c"/dev/null".as_ptr());
        Ok(api)
    }
}

/// The model directory `TessBaseAPIInit2(datapath=NULL)` will load, resolved now
/// (Python `os.path.realpath(TESSDATA_PREFIX)` or "").
///
/// TESSDATA_PREFIX may be relative; Tesseract resolves it against the process cwd at
/// init, and the warm daemon switches cwd per request. Keying by the resolved path
/// means the same relative prefix from two directories selects two engines.
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

/// TSV from the `tesseract` binary for an 8-bit grayscale image (Python
/// `tesseract_tsv` without an engine: BMP on stdin).
pub fn tesseract_tsv_binary(raw: &[u8], w: u32, h: u32, psm: i32, sauvola: bool) -> Result<String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let n = (w as usize) * (h as usize);
    let img = GrayImage::from_raw(w, h, raw.get(..n).ok_or_else(|| Error::value("OCR buffer smaller than its size"))?.to_vec());
    let bmp = img.encode_bmp();
    let mut cmd = Command::new("tesseract");
    cmd.args(["stdin", "stdout", "--oem", "1", "--psm", &psm.to_string()]);
    if sauvola {
        cmd.args(["-c", "thresholding_method=2", "-c", "thresholding_kfactor=0.15", "-c", "thresholding_window_size=0.80"]);
    }
    cmd.arg("tsv");
    if std::env::var_os("OMP_THREAD_LIMIT").is_none() {
        cmd.env("OMP_THREAD_LIMIT", "1");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Os(format!("{e}: 'tesseract'")))?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let out = std::thread::scope(|s| {
        let writer = s.spawn(move || {
            if let Some(mut i) = stdin.take() {
                let _ = i.write_all(&bmp);
            }
        });
        let mut buf = Vec::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.read_to_end(&mut buf);
        }
        let _ = writer.join();
        buf
    });
    let status = child.wait()?;
    if !status.success() {
        return Err(Error::Subprocess { cmd: "tesseract".into(), code: status.code().unwrap_or(1) });
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// One OCR line as raw strip-pixel word boxes `(text, x0, y0, x1, y1)`.
pub type RawWords = Vec<(String, i64, i64, i64, i64)>;

/// The inside of a filled control in one OCR strip (strip pixels, end-exclusive), and
/// how to binarize its label: ink is brighter (or darker) than `threshold`.
#[derive(Clone, Debug, PartialEq)]
pub struct FilledControl {
    pub xa: usize,
    pub xb: usize,
    pub ya: usize,
    pub yb: usize,
    pub threshold: i32,
    pub ink_brighter: bool,
}

/// Filled controls in a strip: a block at least a quarter of the strip wide and 10 px
/// tall whose fill differs from the background (the level at the strip's left and right
/// edges), is even (most of it within 16 levels of its own level, so not a photo) and
/// holds a label (pixels far from the fill, 0.3 to 40 % of it). At most four per strip;
/// the rounded ends are left out.
pub fn filled_controls(raw: &[u8], w: usize, h: usize) -> Vec<FilledControl> {
    const DIFF: i32 = 40;
    let mut out = Vec::new();
    if w < 40 || h < 10 || raw.len() < w * h {
        return out;
    }
    let mode = |vals: &mut dyn Iterator<Item = u8>| -> i32 {
        let mut hist = [0u32; 32];
        for v in vals {
            hist[(v >> 3) as usize] += 1;
        }
        let bin = (0..32).max_by_key(|&i| hist[i]).unwrap_or(0);
        bin as i32 * 8 + 4
    };
    // The page is what touches the strip's sides (a wide control can outnumber it).
    let bg = mode(&mut (0..h).flat_map(|y| [0, 1, w - 2, w - 1].map(|x| raw[y * w + x])));
    let far = |v: u8, from: i32| (v as i32 - from).abs() > DIFF;
    let wide: Vec<bool> = (0..h)
        .map(|y| {
            let row = &raw[y * w..y * w + w];
            let n = row.iter().step_by(2).filter(|&&v| far(v, bg)).count();
            n * 4 >= w.div_ceil(2)
        })
        .collect();
    let mut y = 0;
    while y < h && out.len() < 4 {
        if !wide[y] {
            y += 1;
            continue;
        }
        let ya = y;
        while y < h && wide[y] {
            y += 1;
        }
        let yb = y;
        let rh = yb - ya;
        if rh < 10 {
            continue;
        }
        // Columns filled in at least 90 % of the run's rows.
        let full: Vec<bool> = (0..w)
            .map(|x| (ya..yb).filter(|&r| far(raw[r * w + x], bg)).count() * 10 >= rh * 9)
            .collect();
        let mut x = 0;
        while x < w && out.len() < 4 {
            if !full[x] {
                x += 1;
                continue;
            }
            let xa0 = x;
            while x < w && full[x] {
                x += 1;
            }
            let xb0 = x;
            if (xb0 - xa0) * 4 < w {
                continue;
            }
            let trim = (rh / 2).min((xb0 - xa0) / 4);
            let (xa, xb, iya, iyb) = (xa0 + trim, xb0 - trim, ya + 1, yb - 1);
            if xb <= xa + 8 || iyb <= iya + 6 {
                continue;
            }
            let px = || (iya..iyb).flat_map(move |r| raw[r * w + xa..r * w + xb].iter().copied());
            let fill = mode(&mut px());
            if (fill - bg).abs() <= DIFF {
                continue;
            }
            let area = (xb - xa) * (iyb - iya);
            let even = px().filter(|&v| (v as i32 - fill).abs() <= 16).count();
            let (mut ink_n, mut ink_sum) = (0usize, 0i64);
            for v in px() {
                if (v as i32 - fill).abs() > 60 {
                    ink_n += 1;
                    ink_sum += v as i64;
                }
            }
            if even * 10 < area * 6 || ink_n * 1000 < area * 3 || ink_n * 10 > area * 4 {
                continue;
            }
            let ink = (ink_sum / ink_n as i64) as i32;
            out.push(FilledControl { xa, xb, ya: iya, yb: iyb, threshold: (fill + ink) / 2, ink_brighter: ink > fill });
        }
    }
    out
}

/// Parse Tesseract TSV level-5 rows (conf > 30) grouped by (block, par, line), in
/// first-seen order.
pub fn parse_tsv(tsv: &str) -> Vec<RawWords> {
    let mut out: Vec<RawWords> = Vec::new();
    let mut index: HashMap<(i32, i32, i32), usize> = HashMap::new();
    let mut last_key: Option<(i32, i32, i32)> = None;
    let mut last_idx: usize = 0;
    for row in tsv.lines() {
        if !row.starts_with("5\t") {
            continue;
        }
        let mut it = row.split('\t');
        let _level = it.next();
        let Some(_page) = it.next() else { continue };
        let Some(block_s) = it.next() else { continue };
        let Some(par_s) = it.next() else { continue };
        let Some(line_s) = it.next() else { continue };
        let Some(_word) = it.next() else { continue };
        let Some(left_s) = it.next() else { continue };
        let Some(top_s) = it.next() else { continue };
        let Some(width_s) = it.next() else { continue };
        let Some(height_s) = it.next() else { continue };
        let Some(conf_s) = it.next() else { continue };
        let Some(text_s) = it.next() else { continue };
        if it.next().is_some() {
            continue;
        }
        let txt = text_s.trim();
        if txt.is_empty() {
            continue;
        }
        let Ok(conf) = conf_s.trim().parse::<f64>() else { continue };
        if conf <= 30.0 {
            continue;
        }
        let (Ok(x0), Ok(y0), Ok(ww), Ok(hh)) = (
            left_s.trim().parse::<i64>(),
            top_s.trim().parse::<i64>(),
            width_s.trim().parse::<i64>(),
            height_s.trim().parse::<i64>(),
        ) else {
            continue;
        };
        let (Ok(blk), Ok(par), Ok(ln)) = (
            block_s.trim().parse::<i32>(),
            par_s.trim().parse::<i32>(),
            line_s.trim().parse::<i32>(),
        ) else {
            continue;
        };
        let key = (blk, par, ln);
        let idx = if last_key == Some(key) {
            last_idx
        } else {
            let i = *index.entry(key).or_insert_with(|| {
                let i = out.len();
                out.push(Vec::new());
                i
            });
            last_key = Some(key);
            last_idx = i;
            i
        };
        out[idx].push((txt.to_string(), x0, y0, x0 + ww, y0 + hh));
    }
    out
}

/// Initial engine cap for an un-daemonised cold `ScreenReader` when the caller did
/// not explicitly set `workers` or `WDA_OCR_WORKERS`. Avoids paying 6x LSTM model init
/// + RSS (~33% faster cold CLI, ~53% lower RSS) while keeping `self.n` strip cuts for
/// fine-grained strip-hash caching and better load balance across the 2 engines.
const COLD_INITIAL_WORKERS: usize = 2;

fn env_workers() -> Option<usize> {
    crate::config::env("WDA_OCR_WORKERS")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map(|v| v as usize)
}

/// `WDA_OCR_WORKERS` or clamp(cpu count, 2, 6).
pub fn default_workers() -> usize {
    env_workers()
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 6))
}

// ---------------------------------------------------------------------------
// Engine pools
// ---------------------------------------------------------------------------

/// Creates one engine for a PSM (Python `_create_tess_api`; tests swap in fakes).
pub type EngineFactory = Arc<dyn Fn(i32) -> Result<TessApi> + Send + Sync>;

fn real_factory() -> EngineFactory {
    Arc::new(create_tess_api)
}

struct PoolState {
    free: Vec<TessApi>,
    /// Engines created (free + leased + being created).
    count: usize,
    max: usize,
}

/// A blocking pool of initialised engines for one PSM and model directory (Python
/// `queue.Queue` of engines plus its count).
pub struct EnginePool {
    pub psm: i32,
    factory: EngineFactory,
    st: Mutex<PoolState>,
    cv: Condvar,
}

impl EnginePool {
    fn new(psm: i32, max: usize, factory: EngineFactory) -> EnginePool {
        EnginePool { psm, factory, st: Mutex::new(PoolState { free: Vec::new(), count: 0, max }), cv: Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a free engine, create one while below the pool size, else wait.
    pub fn get(&self) -> Result<TessApi> {
        let mut st = self.lock();
        loop {
            if let Some(api) = st.free.pop() {
                return Ok(api);
            }
            if st.count < st.max.max(1) {
                st.count += 1;
                drop(st);
                return match (self.factory)(self.psm) {
                    Ok(api) => Ok(api),
                    Err(e) => {
                        self.lock().count -= 1;
                        self.cv.notify_all();
                        Err(e)
                    }
                };
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Return an engine.
    pub fn put(&self, api: TessApi) {
        self.lock().free.push(api);
        self.cv.notify_all();
    }

    /// Create engines until `n` exist, initialising missing engines concurrently.
    pub fn fill(&self, n: usize) -> Result<()> {
        let missing = {
            let mut st = self.lock();
            st.max = st.max.max(n);
            if st.count >= n {
                return Ok(());
            }
            let m = n - st.count;
            st.count = n;
            m
        };
        let create_one = || -> Result<()> {
            struct CountGuard<'a>(&'a EnginePool, bool);
            impl Drop for CountGuard<'_> {
                fn drop(&mut self) {
                    if self.1 {
                        self.0.lock().count -= 1;
                        self.0.cv.notify_all();
                    }
                }
            }
            let mut guard = CountGuard(self, true);
            let api = (self.factory)(self.psm)?;
            guard.1 = false;
            self.put(api);
            Ok(())
        };
        if missing == 1 {
            return create_one();
        }
        std::thread::scope(|s| {
            let handles: Vec<_> = (1..missing).map(|_| s.spawn(&create_one)).collect();
            let mut first_err = create_one().err();
            for h in handles {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some(Error::runtime("OCR engine init panicked"));
                        }
                    }
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
    }

    /// Create one more engine when below the pool size (background warm-up).
    fn grow_one(&self) {
        {
            let mut st = self.lock();
            if st.count >= st.max {
                return;
            }
            st.count += 1;
        }
        match (self.factory)(self.psm) {
            Ok(api) => self.put(api),
            Err(_) => {
                self.lock().count -= 1;
                self.cv.notify_all();
            }
        }
    }

    /// Engines created so far.
    pub fn count(&self) -> usize {
        self.lock().count
    }

    /// Maximum number of engines this pool will create.
    pub fn max_count(&self) -> usize {
        self.lock().max
    }

    /// Engines currently free (Python `apis.qsize()`).
    pub fn free_count(&self) -> usize {
        self.lock().free.len()
    }

    /// Tags of the free engines (test fakes).
    pub fn free_tags(&self) -> Vec<String> {
        self.lock().free.iter().map(|a| a.tag.clone()).collect()
    }

    /// Block until every engine is back in the pool.
    pub fn wait_idle(&self) {
        let mut st = self.lock();
        while st.free.len() < st.count {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Delete every free engine.
    pub fn clear(&self) {
        let freed: Vec<TessApi> = {
            let mut st = self.lock();
            let v = std::mem::take(&mut st.free);
            st.count -= v.len();
            v
        };
        drop(freed);
    }
}

/// Returns a leased engine to its pool even when OCR panics.
struct Lease<'a> {
    pool: &'a EnginePool,
    api: Option<TessApi>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(api) = self.api.take() {
            self.pool.put(api);
        }
    }
}

// ---------------------------------------------------------------------------
// Warm daemon engines
// ---------------------------------------------------------------------------

/// Pre-initialised libtesseract engines for the warm daemon.
///
/// Engines are partitioned by (page segmentation mode, effective model data), so a
/// request never gets an engine initialised under another caller's TESSDATA_PREFIX.
/// LSTM-only engines carry no per-image state between calls and Sauvola variables
/// are reset after every use. Strip threads are scoped to each `read`, so
/// `end_request` only has to wait until every leased engine is back in its pool.
pub struct WarmOcr {
    engines: Mutex<HashMap<(i32, String), Arc<EnginePool>>>,
    factory: EngineFactory,
}

impl Default for WarmOcr {
    fn default() -> Self {
        WarmOcr::new()
    }
}

impl WarmOcr {
    pub fn new() -> WarmOcr {
        WarmOcr { engines: Mutex::new(HashMap::new()), factory: real_factory() }
    }

    /// Warm engines created by `factory` (tests).
    pub fn with_factory(factory: impl Fn(i32) -> Result<TessApi> + Send + Sync + 'static) -> WarmOcr {
        WarmOcr { engines: Mutex::new(HashMap::new()), factory: Arc::new(factory) }
    }

    /// The pool for (psm, current model key), holding at least `workers` engines.
    pub fn lease(&self, psm: i32, workers: usize) -> Result<Arc<EnginePool>> {
        let key = (psm, tess_model_key());
        let mut map = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        let pool = map.entry(key).or_insert_with(|| Arc::new(EnginePool::new(psm, workers, self.factory.clone()))).clone();
        pool.fill(workers)?;
        Ok(pool)
    }

    /// Cancel/finish every OCR job a request started; all engines back in their pools.
    pub fn end_request(&self) {
        let pools: Vec<Arc<EnginePool>> =
            self.engines.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        for p in pools {
            p.wait_idle();
        }
    }

    /// `end_request` then delete every engine.
    pub fn close(&self) {
        self.end_request();
        let pools: Vec<Arc<EnginePool>> =
            self.engines.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, p)| p).collect();
        for p in pools {
            p.clear();
        }
    }
}

static WARM: RwLock<Option<&'static WarmOcr>> = RwLock::new(None);

/// Install (daemon start) or remove the process-wide warm engines.
///
/// `true` is the OCR half of Python `Server.warm`: when libtesseract loads, create the
/// engines for PSM 11 and the default worker count; if that fails (no model data)
/// the daemon runs without warm engines. `false` closes and removes them.
pub fn set_warm_ocr(on: bool) {
    let mut g = WARM.write().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = g.take() {
        old.close(); // the struct itself stays leaked: earlier `&'static` borrows remain valid
    }
    if on && tess_lib().is_some() {
        let warm: &'static WarmOcr = Box::leak(Box::new(WarmOcr::new()));
        if warm.lease(11, default_workers()).is_ok() {
            warm.end_request();
            *g = Some(warm);
        } else {
            warm.close();
        }
    }
}

/// The warm engines, when running inside the daemon.
pub fn warm_ocr() -> Option<&'static WarmOcr> {
    *WARM.read().unwrap_or_else(|e| e.into_inner())
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

/// Cached decoded+resized grayscale frame paired with a [`ShotValidator`] so HTTP 304
/// hits skip redundant JPEG decoding and bilinear resizing.
#[derive(Clone, Debug)]
struct DecodedShot {
    key: String,
    etag: String,
    out: (u32, u32),
    raw_size: (u32, u32),
    img: Frame,
}

enum ConditionalShot {
    /// HTTP 304 matched `self.shot_validator` (already incremented `conditional_hits`).
    NotModified,
    /// Fresh HTTP response `(status, content_type, body)`.
    Fresh(u16, String, Vec<u8>),
}

/// Test replacement for the `tesseract` call: `(raw, w, h, psm, sauvola) -> TSV`
/// (Python `mock.patch("turbotap.cli.tesseract_tsv")` together with
/// `_tess_lib -> None`: setting it forces the binary path).
pub type TsvHook = Arc<dyn Fn(&[u8], u32, u32, i32, bool) -> Result<String> + Send + Sync>;
/// Test replacement for `ocr_raw` (Python `mock.patch.object(reader, "_ocr_raw")`).
pub type OcrRawHook = Arc<dyn Fn(&[u8], u32, u32, bool) -> Result<Vec<RawWords>> + Send + Sync>;

#[cfg(test)]
thread_local! {
    /// Tests: the `ocr_raw_hook` every [`ScreenReader`] made on this thread starts with,
    /// so CLI-level tests (which build their own reader) can stand in for Tesseract.
    pub static TEST_OCR_RAW_HOOK: std::cell::RefCell<Option<OcrRawHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_ocr_raw_hook() -> Option<OcrRawHook> {
    TEST_OCR_RAW_HOOK.with(|h| h.borrow().clone())
}

#[cfg(not(test))]
fn test_ocr_raw_hook() -> Option<OcrRawHook> {
    None
}

/// Full-screen OCR for any app, no coordinates known in advance.
///
/// - grab: wda-turbo /wda/fastScreenshot downscaled on the phone to OCR size
///   (~50-60 ms for the full screen) or stock /screenshot (~125-245 ms).
/// - read: the frame is cut into strips at blank rows and the strips are OCR'd in
///   parallel, one TessBaseAPI per worker (full screen ~95 ms vs ~310 ms for one
///   pass). Strips are cached by content hash, so polling an unchanged or partly
///   changed screen only OCRs what changed.
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
    shot_decoded: Option<DecodedShot>,
    pub conditional_hits: u64,
    pub tsv_hook: Option<TsvHook>,
    pub ocr_raw_hook: Option<OcrRawHook>,
    pool: Mutex<Option<Arc<EnginePool>>>,
    warming: Mutex<Vec<JoinHandle<()>>>,
    explicit_workers: bool,
}

impl Drop for ScreenReader {
    fn drop(&mut self) {
        // Engine warm-up threads read TESSDATA_PREFIX and the cwd: never let them
        // outlive the reader (the daemon request that made it).
        let handles = std::mem::take(&mut *self.warming.lock().unwrap_or_else(|e| e.into_inner()));
        for h in handles {
            let _ = h.join();
        }
    }
}

impl ScreenReader {
    /// Python `ScreenReader(client, scale=1.5, workers=None, psm=11, warm_ocr=True)`.
    pub fn new(scale: f64, workers: Option<usize>, psm: i32, warm_ocr: bool) -> ScreenReader {
        let arg_w = workers.filter(|w| *w > 0);
        let env_w = env_workers();
        let explicit_workers = arg_w.is_some() || env_w.is_some();
        let n = arg_w.or(env_w).unwrap_or_else(default_workers);
        let r = ScreenReader {
            scale,
            psm,
            n,
            cache: HashMap::new(),
            last: None,
            last_ts: 0.0,
            last_lines: None,
            pt: None,
            pt_from_cache: false,
            dev: scale / 3.0, // typical 3x iOS Retina; auto-corrected after 1st grab
            shot_validator: None,
            shot_decoded: None,
            conditional_hits: 0,
            tsv_hook: None,
            ocr_raw_hook: test_ocr_raw_hook(),
            pool: Mutex::new(None),
            warming: Mutex::new(Vec::new()),
            explicit_workers,
        };
        if warm_ocr {
            // Warm libtesseract (including dlopen) and engines in the background while
            // session setup and the first screenshot are in flight; failures surface on read().
            let _ = r.start_warm_ocr();
        }
        r
    }

    /// Default reader (scale 1.5, psm 11, warm engines).
    pub fn default_reader() -> ScreenReader {
        ScreenReader::new(1.5, None, 11, true)
    }

    /// Engine count for a cold (non-daemon) reader: capped at [`COLD_INITIAL_WORKERS`]
    /// unless the caller set `workers` or `WDA_OCR_WORKERS` explicitly.
    fn cold_engine_cap(&self) -> usize {
        if self.explicit_workers {
            self.n
        } else {
            self.n.min(COLD_INITIAL_WORKERS)
        }
    }

    /// Kick off engine warm-up without blocking the caller on `dlopen("libtesseract.so.5")`
    /// in cold CLI mode, so dynamic library loading and LSTM init overlap with `/wda/fastScreenshot`.
    fn start_warm_ocr(&self) -> Result<()> {
        if self.tsv_hook.is_some()
            || self.ocr_raw_hook.is_some()
            || crate::config::env("WDA_OCR").as_deref() == Some("binary")
        {
            return Ok(());
        }
        let mut slot = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Ok(());
        }
        let pool = if let Some(warm) = warm_ocr() {
            warm.lease(self.psm, self.n)?
        } else {
            ensure_omp_thread_limit();
            let init = self.cold_engine_cap();
            let pool = Arc::new(EnginePool::new(self.psm, init, real_factory()));
            let mut warming = self.warming.lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..init {
                let p = pool.clone();
                warming.push(std::thread::spawn(move || p.grow_one()));
            }
            pool
        };
        *slot = Some(pool);
        Ok(())
    }

    /// Python `_ensure_ocr`: lease warm daemon engines, or make this reader's own pool
    /// and start creating its engines in the background.
    fn ensure_ocr(&self) -> Result<Option<Arc<EnginePool>>> {
        if self.tsv_hook.is_some() || tess_lib().is_none() {
            return Ok(None);
        }
        let mut slot = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = slot.as_ref() {
            return Ok(Some(p.clone()));
        }
        let pool = if let Some(warm) = warm_ocr() {
            // Warm daemon: borrow pre-initialised engines (full `self.n`); strip cache stays per reader.
            warm.lease(self.psm, self.n)?
        } else {
            // Cold CLI (no warm daemon): cap engine creation at COLD_INITIAL_WORKERS unless
            // explicitly overridden, while keeping `self.n` strip cuts for fine-grained
            // strip-hash caching and better load balance across the 2 engines.
            let init = self.cold_engine_cap();
            let pool = Arc::new(EnginePool::new(self.psm, init, real_factory()));
            let mut warming = self.warming.lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..init {
                let p = pool.clone();
                warming.push(std::thread::spawn(move || p.grow_one()));
            }
            pool
        };
        *slot = Some(pool.clone());
        Ok(Some(pool))
    }

    /// Strip boundaries at the most uniform row near each 1/n of the height.
    pub fn cuts(&self, w: u32, h: u32, data: &[u8]) -> Vec<u32> {
        let (w, h) = (w as usize, h as usize);
        if h < 180 || self.n <= 1 {
            return vec![0, h as u32];
        }
        let n = self.n.min((h / 90).max(1));
        if n <= 1 {
            return vec![0, h as u32];
        }
        let step = if w >= 240 { 4 } else { 2 };
        let mut cuts: Vec<i64> = vec![0];
        let (hi_h, n_i) = (h as i64, n as i64);
        for i in 1..n_i {
            let t = hi_h * i / n_i;
            let half = 48.min(hi_h / (2 * n_i));
            let last = *cuts.last().unwrap_or(&0);
            let (lo, hi) = ((last + 24).max(t - half), (hi_h - 24).min(t + half));
            if lo >= hi {
                continue;
            }
            // Search outward from target row t with strided sampling and early exit on
            // blank/uniform rows (range <= 2), avoiding scanning every pixel of every row.
            let (mut best_y, mut best_score) = (t, (256i64, 0i64));
            let max_off = (t - lo).max(hi - 1 - t);
            'outer: for off in 0..=max_off {
                let cands: &[i64] = if off == 0 { &[t, i64::MIN] } else { &[t - off, t + off] };
                for &y in cands {
                    if y == i64::MIN || !(lo <= y && y < hi) {
                        continue;
                    }
                    let start = y as usize * w;
                    let end = (start + w).min(data.len());
                    let row = data.get(start..end).unwrap_or(&[]);
                    let (mut mn, mut mx) = (255u8, 0u8);
                    let mut aborted = false;
                    for &v in row.iter().step_by(step) {
                        mn = mn.min(v);
                        mx = mx.max(v);
                        if (mx - mn) as i64 >= best_score.0 {
                            aborted = true;
                            break;
                        }
                    }
                    if aborted {
                        continue;
                    }
                    let rng = if row.is_empty() { 0 } else { (mx - mn) as i64 };
                    let sc = (rng, off);
                    if sc < best_score {
                        best_score = sc;
                        best_y = y;
                        if rng <= 2 {
                            break;
                        }
                    }
                }
                if best_score.0 <= 2 {
                    break 'outer;
                }
            }
            cuts.push(best_y);
        }
        cuts.push(hi_h);
        cuts.into_iter().map(|v| v as u32).collect()
    }

    /// OCR one strip (uniform-strip skip, dark-mode pre-inversion, direct
    /// `TessResultIterator` recognition, Sauvola variables).
    /// OCR one strip, then read the label of any filled control (a button) the strip
    /// OCR found no word in. One threshold for the whole strip separates the page from
    /// the control's fill and loses a label that differs from that fill (white text on a
    /// bright button on a dark page): the label is read again from the control's inside
    /// on its own, binarized between the fill and the label.
    pub fn ocr_strip(&self, raw: &[u8], w: u32, h: u32, sauvola: bool) -> Result<Vec<RawWords>> {
        let mut lines = self.ocr_raw(raw, w, h, sauvola)?;
        for b in filled_controls(raw, w as usize, h as usize) {
            let inside = |(_, x0, y0, x1, y1): &(String, i64, i64, i64, i64)| {
                let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
                b.xa as i64 <= cx && cx < b.xb as i64 && b.ya as i64 <= cy && cy < b.yb as i64
            };
            if lines.iter().flatten().any(inside) {
                continue;
            }
            let (cw, ch) = (b.xb - b.xa, b.yb - b.ya);
            let mut buf = Vec::with_capacity(cw * ch);
            for y in b.ya..b.yb {
                for &v in &raw[y * w as usize + b.xa..y * w as usize + b.xb] {
                    let is_ink = if b.ink_brighter { v as i32 > b.threshold } else { (v as i32) < b.threshold };
                    buf.push(if is_ink { 0 } else { 255 });
                }
            }
            for line in self.ocr_raw(&buf, cw as u32, ch as u32, false)? {
                let line: RawWords = line
                    .into_iter()
                    .filter(|(t, ..)| t.chars().filter(|c| c.is_alphanumeric()).count() >= 2)
                    .map(|(t, x0, y0, x1, y1)| {
                        let (dx, dy) = (b.xa as i64, b.ya as i64);
                        (t, x0 + dx, y0 + dy, x1 + dx, y1 + dy)
                    })
                    .collect();
                if !line.is_empty() {
                    lines.push(line);
                }
            }
        }
        Ok(lines)
    }

    pub fn ocr_raw(&self, raw: &[u8], w: u32, h: u32, sauvola: bool) -> Result<Vec<RawWords>> {
        if let Some(hook) = self.ocr_raw_hook.as_ref() {
            return hook(raw, w, h, sauvola);
        }
        let mut need_invert = false;
        let n_samp = raw.len().div_ceil(19);
        if n_samp > 0 {
            // Skip only genuinely uniform flat strips; preserve low-contrast text
            // (e.g. ~12 grayscale levels) for both Otsu and Sauvola binarization.
            let min_contrast = if sauvola { 1 } else { 4 };
            let (mut lo, mut hi, mut sum) = (255u8, 0u8, 0u64);
            for &v in raw.iter().step_by(19) {
                lo = lo.min(v);
                hi = hi.max(v);
                sum += v as u64;
            }
            if (hi as i32 - lo as i32) < min_contrast {
                return Ok(Vec::new()); // uniform blank strip
            }
            let mean = sum as f64 / n_samp as f64;
            if mean < 120.0 {
                // Pre-invert dark-mode strips so LSTM succeeds on pass 1 instead of
                // retrying via tessedit_do_invert (kept enabled for mixed-polarity UI).
                need_invert = true;
            }
        }
        match self.ensure_ocr()? {
            None => {
                // Bitwise `!v == 255 - v` auto-vectorizes cleanly to AVX2/SSE2 XOR.
                let inverted = need_invert.then(|| raw.iter().map(|&v| !v).collect::<Vec<u8>>());
                let buf: &[u8] = inverted.as_deref().unwrap_or(raw);
                let tsv = match self.tsv_hook.as_ref() {
                    Some(hook) => hook(buf, w, h, self.psm, sauvola)?,
                    None => tesseract_tsv_binary(buf, w, h, self.psm, sauvola)?,
                };
                Ok(parse_tsv(&tsv))
            }
            Some(pool) => {
                let mut lease = Lease { pool: &pool, api: Some(pool.get()?) };
                let Some(api) = lease.api.as_mut() else { return Err(Error::runtime("no Tesseract engine")) };
                if sauvola {
                    api.set_variable(c"thresholding_method", c"2");
                    api.set_variable(c"thresholding_kfactor", c"0.15");
                    api.set_variable(c"thresholding_window_size", c"0.80");
                }
                let res = if need_invert {
                    if api.inv_buf.len() < raw.len() {
                        api.inv_buf.resize(raw.len(), 0);
                    }
                    let inv = &mut api.inv_buf[..raw.len()];
                    for (d, &v) in inv.iter_mut().zip(raw.iter()) {
                        *d = !v;
                    }
                    // Split borrow via local slice is avoided by passing a temporary method on TessApi;
                    // call `words_strided_inv` using `self.inv_buf`.
                    api.words_strided_inv(raw.len(), w, h, w)
                } else {
                    api.words_strided(raw, w, h, w)
                };
                if sauvola {
                    api.set_variable(c"thresholding_method", c"0");
                }
                drop(lease);
                res
            }
        }
    }

    /// Conditional fastScreenshot needs a runner not already known to ignore the
    /// "etag" request (pre-v6 wda-turbo) and no `WDA_CONDITIONAL_SHOT=0` opt-out.
    /// (Python also excluded test doubles without the real transport; every Rust
    /// client is the real transport.)
    pub fn conditional_ok(&self, c: &WdaClient) -> bool {
        !(c.etag_shot == Some(false) || crate::config::env("WDA_CONDITIONAL_SHOT").as_deref() == Some("0"))
    }

    /// POST /wda/fastScreenshot with this reader's last validator for the identical
    /// request (endpoint, session, crop, scale, quality, format). HTTP 304 reuses the
    /// retained bytes; any other answer replaces or drops the validator. Returns the
    /// same (status, content_type, bytes) triple as req_raw, with 304 mapped to 200.
    pub fn conditional_shot(
        &mut self,
        c: &mut WdaClient,
        payload: &serde_json::Value,
        req_timeout: Option<f64>,
        deadline: Option<f64>,
    ) -> (u16, String, Vec<u8>) {
        match self.conditional_shot_inner(c, payload, req_timeout, deadline) {
            ConditionalShot::NotModified => {
                if let Some(v) = self.shot_validator.as_ref() {
                    (200, v.content_type.clone(), v.body.clone())
                } else {
                    (0, String::new(), b"unpaired 304 Not Modified".to_vec())
                }
            }
            ConditionalShot::Fresh(status, ct, body) => (status, ct, body),
        }
    }

    fn conditional_shot_inner(
        &mut self,
        c: &mut WdaClient,
        payload: &serde_json::Value,
        req_timeout: Option<f64>,
        deadline: Option<f64>,
    ) -> ConditionalShot {
        let key = format!("{}\u{1f}{}\u{1f}{}", c.base_url, c.sid_str(), json::dumps(&sorted_keys(payload)));
        if self.shot_validator.as_ref().is_some_and(|v| v.key != key) {
            self.shot_validator = None;
            self.shot_decoded = None;
        }
        let mut validator = self.shot_validator.clone();
        let mut req_timeout = req_timeout;
        for attempt in 0..2 {
            let mut data = payload.clone();
            if let Value::Object(m) = &mut data {
                m.insert("etag".into(), Value::Bool(true));
                if let Some(v) = validator.as_ref() {
                    m.insert("ifNoneMatch".into(), Value::String(v.etag.clone()));
                }
            }
            let headers: Vec<(String, String)> =
                validator.as_ref().map(|v| vec![("If-None-Match".to_string(), v.etag.clone())]).unwrap_or_default();
            let (status, ct, body) = match deadline {
                Some(d) => c.req_raw(
                    "POST",
                    "/wda/fastScreenshot",
                    Some(&data),
                    req_timeout.unwrap_or(crate::wda::DEFAULT_TIMEOUT),
                    Some(d),
                    Some(&headers),
                ),
                None => c.req_raw("POST", "/wda/fastScreenshot", Some(&data), crate::wda::DEFAULT_TIMEOUT, None, Some(&headers)),
            };
            let etag = c.last_response_headers.as_ref().and_then(|h| h.get("etag").cloned());
            if status == 304 {
                // The v6 runner names the matched validator on every 304; anything else
                // (missing or different tag) is refetched unconditionally.
                if let Some(v) = validator.as_ref() {
                    if etag.as_deref() == Some(v.etag.as_str()) {
                        self.conditional_hits += 1;
                        return ConditionalShot::NotModified;
                    }
                }
                // A 304 we cannot pair with our own retained bytes: never guess a frame.
                validator = None;
                self.shot_validator = None;
                self.shot_decoded = None;
                if attempt == 0 {
                    if let Some(d) = deadline {
                        if clock::now() >= d {
                            return ConditionalShot::Fresh(0, String::new(), b"deadline exceeded".to_vec());
                        }
                        req_timeout = Some((d - clock::now()).max(0.05));
                    }
                    continue;
                }
                return ConditionalShot::Fresh(0, String::new(), b"unpaired 304 Not Modified".to_vec());
            }
            self.shot_decoded = None;
            if status == 200 && ct.to_lowercase().starts_with("image/") && !body.is_empty() {
                match etag {
                    Some(t) if t.len() >= 3 && t.starts_with('"') && t.ends_with('"') => {
                        self.shot_validator =
                            Some(ShotValidator { key: key.clone(), etag: t, body: body.clone(), content_type: ct.clone() });
                        c.etag_shot = Some(true);
                    }
                    _ => {
                        self.shot_validator = None;
                        if c.etag_shot.is_none() {
                            c.etag_shot = Some(false); // pre-v6 runner ignores "etag"
                        }
                    }
                }
            } else {
                self.shot_validator = None;
            }
            return ConditionalShot::Fresh(status, ct, body);
        }
        ConditionalShot::Fresh(0, String::new(), b"conditional screenshot failed".to_vec())
    }
}

/// Fast 128-bit streaming strip hash for the internal 512-entry strip OCR cache
/// (~9.4x faster than Blake2b-256 on 900 KB frames, saving ~0.67 ms per `read()`).
fn strip_hash128(bytes: &[u8]) -> [u8; 16] {
    let mut h1: u64 = 0x9e37_79b9_7f4a_7c15 ^ (bytes.len() as u64);
    let mut h2: u64 = 0xbf58_476d_1ce4_e5b9 ^ (bytes.len() as u64).rotate_left(32);
    let chunks = bytes.chunks_exact(16);
    let rem = chunks.remainder();
    for c in chunks {
        let a = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        let b = u64::from_le_bytes([c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15]]);
        h1 = (h1 ^ a).wrapping_mul(0xa076_1d64_78bd_642f);
        h2 = (h2 ^ b).wrapping_mul(0xe703_7ed1_a0b4_28db);
        h1 = h1.rotate_left(27) ^ h2;
        h2 = h2.rotate_left(31).wrapping_add(h1);
    }
    for &b in rem {
        h1 = (h1 ^ b as u64).wrapping_mul(0xa076_1d64_78bd_642f);
        h2 = h2.rotate_left(13) ^ h1;
    }
    h1 ^= h1 >> 30;
    h1 = h1.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h1 ^= h1 >> 27;
    h2 ^= h2 >> 30;
    h2 = h2.wrapping_mul(0x94d0_49bb_1331_11eb);
    h2 ^= h2 >> 31;
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&h1.to_le_bytes());
    out[8..16].copy_from_slice(&h2.to_le_bytes());
    out
}

/// `json.dumps(v, sort_keys=True)` input: objects rebuilt with sorted keys.
fn sorted_keys(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), sorted_keys(&m[k]))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

/// Python `round()` for a positive size (half to even).
fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python `int(v or default)` for a JSON number.
fn int_or(v: &Value, default: i64) -> i64 {
    if !json::truthy(v) {
        return default;
    }
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().map(|f| f.trunc() as i64).unwrap_or(default)),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(default),
        Value::Bool(true) => 1,
        _ => default,
    }
}

impl Reader for ScreenReader {
    fn scale(&self) -> f64 {
        self.scale
    }

    fn pt_size(&mut self, c: &mut WdaClient, force_refresh: bool, deadline: Option<f64>) -> Result<(i64, i64)> {
        if !force_refresh {
            if let Some(pt) = self.pt {
                return Ok(pt);
            }
            let cached = c.load_session_cache();
            let pt = cached.get("screen_pt");
            let sid_ok = match c.sid.as_deref().filter(|s| !s.is_empty()) {
                None => true,
                Some(sid) => cached.get("sid").and_then(|v| v.as_str()) == Some(sid),
            };
            if cached.get("base_url").and_then(|v| v.as_str()) == Some(c.base_url.as_str()) && sid_ok {
                if let Some(Value::Array(a)) = pt {
                    let vals: Vec<f64> = a.iter().filter_map(|v| if v.is_number() { v.as_f64() } else { None }).collect();
                    if a.len() == 2 && vals.len() == 2 && vals.iter().all(|v| (100.0..=3000.0).contains(v)) {
                        self.pt = Some((vals[0].trunc() as i64, vals[1].trunc() as i64));
                        self.pt_from_cache = true;
                        return Ok((vals[0].trunc() as i64, vals[1].trunc() as i64));
                    }
                }
            }
        }
        let path = c.spath("/window/size");
        let res = match deadline {
            Some(d) => {
                let rem = d - clock::now();
                if rem <= 0.0 {
                    return Err(Error::timeout("geometry deadline exceeded"));
                }
                let res = c.req_with_deadline("GET", &path, None, rem.max(0.05), Some(d));
                if json::deadline_exceeded(&res) || json::has_err(&res) || clock::now() >= d {
                    return Err(Error::timeout("geometry deadline exceeded"));
                }
                res
            }
            None => c.get(&path),
        };
        let v = match res.get("value") {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => j!({}),
        };
        let (w, h) = (int_or(json::get(&v, "width"), 430), int_or(json::get(&v, "height"), 932));
        self.pt = Some((w, h));
        self.pt_from_cache = false;
        if json::truthy(json::get(&v, "width")) && json::truthy(json::get(&v, "height")) {
            let mut m = serde_json::Map::new();
            m.insert("screen_pt".into(), j!([w, h]));
            c.save_session_cache(m);
        }
        Ok((w, h))
    }

    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, timeout: Option<f64>, deadline: Option<f64>) -> Result<Frame> {
        use base64::Engine;
        let full = y0 == 0 && y1.is_none();
        let eff_deadline = deadline.or_else(|| timeout.map(|t| clock::now() + t.max(0.05)));
        let rem = || -> Result<Option<f64>> {
            match eff_deadline {
                None => Ok(None),
                Some(d) => {
                    let r = d - clock::now();
                    if r <= 0.0 {
                        return Err(Error::timeout("screenshot deadline exceeded"));
                    }
                    Ok(Some(r.max(0.05)))
                }
            }
        };
        macro_rules! get_pt {
            ($force:expr) => {{
                if eff_deadline.is_some() {
                    rem()?;
                }
                self.pt_size(c, $force, eff_deadline)?
            }};
        }
        let size_for = |w: i64, h: i64, scale: f64| -> (i64, (u32, u32)) {
            let y1_eff = y1.unwrap_or(h);
            let out = (py_round(w as f64 * scale).max(0) as u32, py_round((y1_eff - y0) as f64 * scale).max(0) as u32);
            (y1_eff, out)
        };
        for attempt in 0..2 {
            let (mut w, mut h) = get_pt!(false);
            let (mut y1_eff, mut out) = size_for(w, h, self.scale);
            let mut img: Option<GrayImage> = None;
            let mut cacheable_raw_size: Option<(u32, u32)> = None;
            if c.turbo != Some(false) {
                let mut payload = j!({"quality": 0.6, "scale": self.dev.min(1.0)});
                if !full {
                    if let Value::Object(m) = &mut payload {
                        m.insert("x0".into(), j!(0));
                        m.insert("x1".into(), j!(w));
                        m.insert("y0".into(), j!(y0));
                        m.insert("y1".into(), j!(y1_eff));
                    }
                }
                let use_raw = c.raw_shot != Some(false);
                let mut raw_bytes: Option<Vec<u8>> = None;
                let mut r: Option<Value> = None;
                let mut used_conditional = false;
                let req_timeout = rem()?;
                if use_raw {
                    if let Value::Object(m) = &mut payload {
                        m.insert("raw".into(), Value::Bool(true));
                    }
                    let resp = if self.conditional_ok(c) {
                        used_conditional = true;
                        self.conditional_shot_inner(c, &payload, req_timeout, eff_deadline)
                    } else {
                        let (s, ct, b) = c.req_raw(
                            "POST",
                            "/wda/fastScreenshot",
                            Some(&payload),
                            req_timeout.unwrap_or(crate::wda::DEFAULT_TIMEOUT),
                            eff_deadline,
                            None,
                        );
                        ConditionalShot::Fresh(s, ct, b)
                    };
                    match resp {
                        ConditionalShot::NotModified => {
                            c.turbo = Some(true);
                            c.raw_shot = Some(true);
                            if let (Some(v), Some(d)) = (self.shot_validator.as_ref(), self.shot_decoded.as_ref()) {
                                let ratio_ok = !self.pt_from_cache
                                    || d.raw_size.0 == 0
                                    || d.raw_size.1 == 0
                                    || ((d.raw_size.0 as f64 / d.raw_size.1 as f64)
                                        - (out.0 as f64 / out.1 as f64))
                                        .abs()
                                        <= 0.03;
                                if v.key == d.key && v.etag == d.etag && d.out == out && ratio_ok {
                                    if d.raw_size.0 > 0 && (d.raw_size.0 as i64 - out.0 as i64).abs() > 2 {
                                        self.dev *= out.0 as f64 / d.raw_size.0 as f64;
                                    }
                                    // Always return a fresh Arc address (!Arc::ptr_eq with previous
                                    // grabs) because sense.rs compares Arc::ptr_eq to detect distinct grabs.
                                    return Ok(Arc::new((*d.img).clone()));
                                }
                            }
                            if let Some(v) = self.shot_validator.as_ref() {
                                if v.content_type.to_lowercase().starts_with("image/") && !v.body.is_empty() {
                                    raw_bytes = Some(v.body.clone());
                                }
                            }
                        }
                        ConditionalShot::Fresh(status, ct, body) => {
                            if status == 200 && ct.to_lowercase().starts_with("image/") && !body.is_empty() {
                                c.turbo = Some(true);
                                c.raw_shot = Some(true);
                                raw_bytes = Some(body);
                            } else if status == 200 && !body.is_empty() {
                                // Older wda-turbo ignored "raw": true and returned JSON base64.
                                c.raw_shot = Some(false);
                                r = Some(serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| j!({"error": "invalid json"})));
                            } else if status == 404 {
                                r = Some(j!({"error": 404}));
                            } else if status == 0 {
                                r = Some(j!({"error": "connection failed"}));
                            } else {
                                r = Some(j!({"error": status}));
                            }
                        }
                    }
                } else {
                    r = Some(match eff_deadline {
                        Some(d) => c.req_with_deadline(
                            "POST",
                            "/wda/fastScreenshot",
                            Some(&payload),
                            req_timeout.unwrap_or(crate::wda::DEFAULT_TIMEOUT),
                            Some(d),
                        ),
                        None => c.post("/wda/fastScreenshot", &payload),
                    });
                }
                if raw_bytes.is_none() {
                    if let Some(r @ Value::Object(_)) = r.as_ref() {
                        match r.get("value") {
                            Some(Value::String(v)) if !v.is_empty() => {
                                c.turbo = Some(true);
                                raw_bytes = Some(
                                    base64::engine::general_purpose::STANDARD
                                        .decode(v.as_bytes())
                                        .map_err(|e| Error::value(format!("Invalid base64-encoded string: {e}")))?,
                                );
                            }
                            _ => {
                                if json::err_is(r, 404) {
                                    c.turbo = Some(false);
                                } else if json::has_err(r) && self.pt_from_cache && attempt == 0 {
                                    if eff_deadline.is_some_and(|d| clock::now() >= d) {
                                        return Err(Error::timeout("screenshot deadline exceeded"));
                                    }
                                    c.invalidate_session_cache(Some("screen_pt"));
                                    get_pt!(true);
                                    continue;
                                }
                            }
                        }
                    }
                }
                if let Some(bytes) = raw_bytes {
                    let decoded = GrayImage::decode_draft(&bytes, out)?;
                    let raw_size = (decoded.w, decoded.h);
                    if decoded.w > 0 && decoded.h > 0 {
                        let got_ratio = decoded.w as f64 / decoded.h as f64;
                        let exp_ratio = out.0 as f64 / out.1 as f64;
                        if self.pt_from_cache && (got_ratio - exp_ratio).abs() > 0.03 {
                            c.invalidate_session_cache(Some("screen_pt"));
                            (w, h) = get_pt!(true);
                            (y1_eff, out) = size_for(w, h, self.scale);
                            if !full && attempt == 0 {
                                continue;
                            }
                        }
                        if (decoded.w as i64 - out.0 as i64).abs() > 2 {
                            self.dev *= out.0 as f64 / decoded.w as f64;
                        }
                    }
                    if used_conditional {
                        cacheable_raw_size = Some(raw_size);
                    }
                    img = Some(decoded);
                }
            }
            let img = match img {
                Some(i) => i,
                None => {
                    let req_timeout = rem()?;
                    let res = match eff_deadline {
                        Some(d) => c.req_with_deadline("GET", "/screenshot", None, req_timeout.unwrap_or(crate::wda::DEFAULT_TIMEOUT), Some(d)),
                        None => c.get("/screenshot"),
                    };
                    let b64 = match res.get("value") {
                        Some(Value::String(s)) if !s.is_empty() => s.clone(),
                        _ => return Err(crate::wda::empty_shot_error(c)),
                    };
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64.as_bytes())
                        .map_err(|e| Error::value(format!("Invalid base64-encoded string: {e}")))?;
                    let full_img = GrayImage::decode(&bytes)?;
                    if self.pt_from_cache && full_img.w > 0 && full_img.h > 0 {
                        let got_ratio = full_img.w as f64 / full_img.h as f64;
                        let exp_ratio = w as f64 / h as f64;
                        if (got_ratio - exp_ratio).abs() > 0.03 {
                            c.invalidate_session_cache(Some("screen_pt"));
                            (w, h) = get_pt!(true);
                            (y1_eff, out) = size_for(w, h, self.scale);
                        }
                    }
                    let k = full_img.w as f64 / w as f64;
                    full_img.crop(0, (y0 as f64 * k) as i64, full_img.w as i64, (y1_eff as f64 * k) as i64)
                }
            };
            let img = if img.size() != out { img.resize_bilinear(out.0, out.1) } else { img };
            let frame = Arc::new(img);
            if let (Some(v), Some(raw_size)) = (self.shot_validator.as_ref(), cacheable_raw_size) {
                self.shot_decoded = Some(DecodedShot {
                    key: v.key.clone(),
                    etag: v.etag.clone(),
                    out,
                    raw_size,
                    img: Arc::clone(&frame),
                });
            }
            return Ok(frame);
        }
        Err(crate::wda::empty_shot_error(c))
    }

    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>> {
        self.start_warm_ocr()?;
        let ReadOpts { y0, y1, img, sauvola, timeout, deadline } = opts;
        let full = y0 == 0 && y1.is_none();
        let fresh_grab = img.is_none();
        let img = match img {
            Some(i) => i,
            None => self.grab(c, y0, y1, timeout, deadline)?,
        };
        let w = img.w as usize;
        let data: &[u8] = &img.data;
        let cuts = self.cuts(img.w, img.h, data);
        let mut jobs: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        for pair in cuts.windows(2) {
            let (a, b) = (pair[0] as usize, pair[1] as usize);
            let digest = strip_hash128(&data[a * w..b * w]);
            let mut k = Vec::with_capacity(17);
            if sauvola {
                k.push(b'S');
            }
            k.extend_from_slice(&digest);
            jobs.push((a, b, k));
        }
        if self.cache.len() > 512 {
            self.cache.clear(); // before `todo`, or strips counted as cached vanish
        }
        let mut uniq: Vec<(&[u8], usize, &Vec<u8>)> = Vec::new();
        for (a, b, k) in &jobs {
            if !self.cache.contains_key(k) && !uniq.iter().any(|(_, _, uk)| *uk == k) {
                uniq.push((&data[a * w..b * w], b - a, k));
            }
        }
        if !uniq.is_empty() {
            let results: Vec<Result<Vec<RawWords>>> = if uniq.len() == 1 {
                vec![self.ocr_strip(uniq[0].0, w as u32, uniq[0].1 as u32, sauvola)]
            } else {
                let this: &ScreenReader = self;
                std::thread::scope(|s| {
                    let handles: Vec<_> = uniq
                        .iter()
                        .map(|(raw, sh, _)| {
                            let (raw, sh) = (*raw, *sh);
                            clock::spawn_scoped(s, move || this.ocr_strip(raw, w as u32, sh as u32, sauvola))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|hd| hd.join().unwrap_or_else(|_| Err(Error::runtime("OCR worker panicked"))))
                        .collect()
                })
            };
            let keys: Vec<Vec<u8>> = uniq.iter().map(|(_, _, k)| (*k).clone()).collect();
            for (k, res) in keys.into_iter().zip(results) {
                self.cache.insert(k, res?);
            }
        }
        let s = self.scale;
        let mut out: Vec<Line> = Vec::new();
        for (a, _, k) in &jobs {
            for words in self.cache.get(k).map(|v| v.as_slice()).unwrap_or(&[]) {
                if words.is_empty() {
                    continue;
                }
                out.push(Line::new(
                    words
                        .iter()
                        .map(|(t, x0, ya, x1, yb)| {
                            Word::new(
                                t,
                                *x0 as f64 / s,
                                (*ya + *a as i64) as f64 / s + y0 as f64,
                                *x1 as f64 / s,
                                (*yb + *a as i64) as f64 / s + y0 as f64,
                            )
                        })
                        .collect(),
                ));
            }
        }
        out.sort_by(|p, q| (p.y0, p.x0).partial_cmp(&(q.y0, q.x0)).unwrap_or(std::cmp::Ordering::Equal));
        if !sauvola {
            if full {
                self.last = Some(img);
                self.last_lines = Some(out.clone());
                if fresh_grab {
                    self.last_ts = clock::now();
                }
            } else {
                self.last = Some(img);
                self.last_lines = None;
                self.last_ts = 0.0;
            }
        } else if fresh_grab {
            self.last = Some(img);
            self.last_lines = None;
            self.last_ts = 0.0;
        }
        Ok(out)
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
    // Start async cold libtesseract warm-up before ensure_session so dlopen + LSTM init
    // overlap with both session setup and the screenshot round-trip.
    let mut reader = ScreenReader::new(scale, None, 11, true);
    client.ensure_session(None, true)?;
    let t0 = clock::real_now();
    let lines = reader.read(client, ReadOpts { y0, y1, ..Default::default() })?;
    let dt = (clock::real_now() - t0) * 1000.0;
    crate::outln!("# DOM-free OCR ({} lines, {:.0} ms incl. engine warm-up)", lines.len(), dt);
    for ln in &lines {
        let (x, y) = ln.center();
        crate::outln!("({:3}, {:3})  {}", x, y, ln.text);
    }
    Ok(())
}

#[cfg(test)]
mod ocr_opt_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn parse_tsv_groups_lines_in_first_seen_order_and_filters_conf() {
        let tsv = concat!(
            "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n",
            "4\t1\t2\t1\t1\t0\t0\t0\t100\t20\t-1\t\n",
            "5\t1\t2\t1\t1\t1\t10\t20\t30\t12\t92.5\tHello\n",
            "5\t1\t2\t1\t1\t2\t45\t20\t35\t12\t30.0\tLowConf\n",
            "5\t1\t2\t1\t1\t3\t85\t20\t25\t12\t30.1\tWorld\n",
            "5\t1\t1\t1\t1\t1\t5\t5\t20\t10\t88\tFirst\n",
            "5\t1\t2\t1\t1\t4\t120\t20\t15\t12\t99\t!\n",
            "5\t1\t3\t1\t1\t1\t10\t40\t20\t10\t95\t   \n",
            "5\t1\t3\t1\t1\t2\tbad\t40\t20\t10\t95\tBadCoord\n",
            "5\t1\t3\t1\t1\t3\t10\t40\t20\t10\t95\tExtra\tCol\n",
        );
        let parsed = parse_tsv(tsv);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            vec![
                ("Hello".into(), 10, 20, 40, 32),
                ("World".into(), 85, 20, 110, 32),
                ("!".into(), 120, 20, 135, 32),
            ]
        );
        assert_eq!(parsed[1], vec![("First".into(), 5, 5, 25, 15)]);
    }

    #[test]
    fn engine_pool_fill_runs_concurrently_and_accounts_for_errors() {
        // 4 engines each sleeping 45 ms must finish well under 4 * 45 = 180 ms.
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let warm = WarmOcr::with_factory(move |psm| {
            let id = c2.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(45));
            Ok(TessApi::fake(format!("e-{psm}-{id}")))
        });
        let t0 = Instant::now();
        let pool = warm.lease(11, 4).unwrap();
        let dt = t0.elapsed();
        assert_eq!(pool.count(), 4);
        assert_eq!(pool.free_count(), 4);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(dt < Duration::from_millis(140), "fill(4) took {dt:?}; expected parallel execution");

        // Partial failure decrements count for failed slots while keeping succeeded engines.
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let pool_err = EnginePool::new(
            11,
            4,
            Arc::new(move |_| {
                let k = n2.fetch_add(1, Ordering::SeqCst);
                if k % 2 == 0 {
                    Ok(TessApi::fake(format!("ok-{k}")))
                } else {
                    Err(Error::runtime("init failed"))
                }
            }),
        );
        assert!(pool_err.fill(4).is_err());
        assert_eq!(pool_err.count(), 2);
        assert_eq!(pool_err.free_count(), 2);
    }

    #[test]
    fn conditional_304_reuses_decoded_frame_without_redecoding_and_returns_fresh_arc() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::AtomicBool;

        let _env = crate::testutil::set_env(&[]);
        let _c = crate::testutil::isolated_cache();
        let body = crate::testutil::encode_image_bytes(&crate::testutil::solid_img(180, (430, 932)), "JPEG", 80);
        let etag = "\"test-304-cache\"".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let b2 = body.clone();
        let e2 = etag.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = conn {
                    let b3 = b2.clone();
                    let e3 = e2.clone();
                    std::thread::spawn(move || {
                        let Ok(mut w) = stream.try_clone() else { return };
                        let mut r = BufReader::new(stream);
                        loop {
                            let mut line = String::new();
                            if r.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let mut it = line.split_whitespace();
                            let method = it.next().unwrap_or("").to_string();
                            let path = it.next().unwrap_or("").to_string();
                            let mut hdrs = HashMap::new();
                            loop {
                                let mut h = String::new();
                                if r.read_line(&mut h).unwrap_or(0) == 0 {
                                    return;
                                }
                                let h = h.trim_end();
                                if h.is_empty() {
                                    break;
                                }
                                if let Some((k, v)) = h.split_once(':') {
                                    hdrs.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                                }
                            }
                            let len: usize = hdrs.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                            let mut req_body = vec![0u8; len];
                            if r.read_exact(&mut req_body).is_err() {
                                return;
                            }
                            let ok = if method == "GET" && path.ends_with("/window/size") {
                                let resp = br#"{"value":{"width":430,"height":932}}"#;
                                write!(
                                    w,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                    resp.len()
                                )
                                .and_then(|_| w.write_all(resp))
                            } else if hdrs.get("if-none-match").map(|s| s.as_str()) == Some(e3.as_str()) {
                                write!(
                                    w,
                                    "HTTP/1.1 304 Not Modified\r\nContent-Type: image/jpeg\r\nETag: {e3}\r\nContent-Length: 0\r\n\r\n"
                                )
                            } else {
                                write!(
                                    w,
                                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nETag: {e3}\r\nContent-Length: {}\r\n\r\n",
                                    b3.len()
                                )
                                .and_then(|_| w.write_all(&b3))
                            };
                            if ok.is_err() {
                                return;
                            }
                        }
                    });
                }
            }
        });

        let mut client = WdaClient::new(&format!("http://127.0.0.1:{port}"), crate::testutil::TEST_UDID, None);
        client.sid = Some("sid-cache".into());
        let mut reader = ScreenReader::new(1.0, None, 11, false);
        let first = reader.grab(&mut client, 0, None, None, None).unwrap();
        assert!(reader.shot_decoded.is_some());
        // Corrupt raw bytes in validator to prove the 304 path uses the decoded cache
        // instead of re-decoding JPEG bytes.
        reader.shot_validator.as_mut().unwrap().body = b"corrupted-jpeg-bytes".to_vec();
        let second = reader.grab(&mut client, 0, None, None, None).unwrap();
        let third = reader.grab(&mut client, 0, None, None, None).unwrap();
        assert_eq!(reader.conditional_hits, 2);
        assert_eq!(first.data, second.data);
        assert_eq!(second.data, third.data);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&second, &third));

        stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", port));
    }

    #[test]
    fn cold_reader_caps_engines_at_two_while_preserving_strip_cuts_and_explicit_overrides() {
        let _env = crate::testutil::set_env(&[("WDA_OCR_WORKERS", None)]);
        // Un-daemonised default reader keeps full `default_workers()` strip cuts
        // while capping cold engine creation at COLD_INITIAL_WORKERS (2).
        let r = ScreenReader::new(1.5, None, 11, false);
        assert!(!r.explicit_workers);
        assert_eq!(r.n, default_workers());
        assert_eq!(r.cold_engine_cap(), r.n.min(COLD_INITIAL_WORKERS));

        // Explicit workers arg overrides the cold cap.
        let r_arg = ScreenReader::new(1.5, Some(5), 11, false);
        assert!(r_arg.explicit_workers);
        assert_eq!(r_arg.n, 5);
        assert_eq!(r_arg.cold_engine_cap(), 5);

        // Explicit WDA_OCR_WORKERS env var overrides the cold cap.
        drop(_env);
        let _env2 = crate::testutil::set_env(&[("WDA_OCR_WORKERS", Some("4"))]);
        let r_env = ScreenReader::new(1.5, None, 11, false);
        assert!(r_env.explicit_workers);
        assert_eq!(r_env.n, 4);
        assert_eq!(r_env.cold_engine_cap(), 4);
    }

    #[test]
    fn strip_hash128_is_deterministic_and_sensitive() {
        let mut a = vec![240u8; 4096];
        assert_eq!(strip_hash128(&a), strip_hash128(&a));
        let h_a = strip_hash128(&a);
        a[2047] ^= 1;
        assert_ne!(h_a, strip_hash128(&a));
        let b = vec![240u8; 4097];
        assert_ne!(h_a, strip_hash128(&b));
    }
}
