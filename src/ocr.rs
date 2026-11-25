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
use std::ffi::{c_char, c_int, c_void, CStr};
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
type FnDeleteText = unsafe extern "C" fn(*mut c_char);
type FnDelete = unsafe extern "C" fn(*mut c_void);

/// The loaded libtesseract C API (function pointers resolved once).
pub struct TessLib {
    create: FnCreate,
    init2: FnInit2,
    set_psm: FnSetPsm,
    set_var: FnSetVar,
    set_image: FnSetImage,
    get_tsv: FnGetTsv,
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
    // Leptonica prints "Error in boxClipToRectangle" for harmless empty boxes.
    let lept = LEPT_NAMES.iter().find_map(|n| unsafe { libloading::Library::new(n) }.ok());
    if let Some(l) = lept.as_ref() {
        // SAFETY: `l_int32 setMsgSeverity(l_int32)`.
        if let Ok(f) = unsafe { l.get::<unsafe extern "C" fn(c_int) -> c_int>(b"setMsgSeverity\0") } {
            unsafe { f(6) };
        }
    }
    let (create, init2, set_psm, set_var, set_image, get_tsv, delete_text, delete) = api;
    Some(TessLib { create, init2, set_psm, set_var, set_image, get_tsv, delete_text, delete, _lib: lib, _lept: lept })
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
    /// Identity of a fake engine (empty for real ones).
    pub tag: String,
}

// SAFETY: a TessBaseAPI is used by one thread at a time (engines move through pools).
unsafe impl Send for TessApi {}

impl TessApi {
    /// A stand-in engine for tests of the pooling logic (Python
    /// `mock.patch.object(cli, "_create_tess_api", side_effect=...)`).
    pub fn fake(tag: impl Into<String>) -> TessApi {
        TessApi { ptr: std::ptr::null_mut(), lib: None, tag: tag.into() }
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

    /// `_tesseract_tsv_raw`: TSV for an 8-bit grayscale buffer.
    fn tsv(&mut self, raw: &[u8], w: u32, h: u32) -> Result<String> {
        let Some(lib) = self.lib.filter(|_| !self.ptr.is_null()) else {
            return Err(Error::runtime("fake Tesseract engine cannot OCR"));
        };
        if raw.len() < (w as usize) * (h as usize) {
            return Err(Error::value("OCR buffer smaller than its size"));
        }
        // SAFETY: `raw` outlives the call (SetImage copies into a Pix) and holds w*h
        // bytes at 1 byte per pixel; the TSV string is freed with TessDeleteText.
        unsafe {
            (lib.set_image)(self.ptr, raw.as_ptr(), w as c_int, h as c_int, 1, w as c_int);
            let p = (lib.get_tsv)(self.ptr, 0);
            if p.is_null() {
                return Ok(String::new());
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (lib.delete_text)(p);
            Ok(s)
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
        let api = TessApi { ptr, lib: Some(lib), tag: String::new() };
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

/// Parse Tesseract TSV level-5 rows (conf > 30) grouped by (block, par, line), in
/// first-seen order.
pub fn parse_tsv(tsv: &str) -> Vec<RawWords> {
    let mut order: Vec<(String, String, String)> = Vec::new();
    let mut lines: HashMap<(String, String, String), RawWords> = HashMap::new();
    for row in tsv.lines() {
        if !row.starts_with("5\t") {
            continue;
        }
        let p: Vec<&str> = row.split('\t').collect();
        if p.len() != 12 {
            continue;
        }
        let txt = p[11].trim();
        if txt.is_empty() {
            continue;
        }
        let Ok(conf) = p[10].trim().parse::<f64>() else { continue };
        if conf <= 30.0 {
            continue;
        }
        let ints: Vec<i64> = p[6..10].iter().filter_map(|v| v.trim().parse::<i64>().ok()).collect();
        let [x0, y0, ww, hh] = ints[..] else { continue };
        let key = (p[2].to_string(), p[3].to_string(), p[4].to_string());
        lines
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push((txt.to_string(), x0, y0, x0 + ww, y0 + hh));
    }
    order.into_iter().filter_map(|k| lines.remove(&k)).collect()
}

/// `WDA_OCR_WORKERS` or clamp(cpu count, 2, 6).
pub fn default_workers() -> usize {
    crate::config::env("WDA_OCR_WORKERS")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map(|v| v as usize)
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

    /// Create engines until `n` exist (Python `while count < workers: apis.put(...)`).
    pub fn fill(&self, n: usize) -> Result<()> {
        {
            let mut st = self.lock();
            st.max = st.max.max(n);
        }
        loop {
            {
                let mut st = self.lock();
                if st.count >= n {
                    return Ok(());
                }
                st.count += 1;
            }
            match (self.factory)(self.psm) {
                Ok(api) => self.put(api),
                Err(e) => {
                    self.lock().count -= 1;
                    self.cv.notify_all();
                    return Err(e);
                }
            }
        }
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

/// Test replacement for the `tesseract` call: `(raw, w, h, psm, sauvola) -> TSV`
/// (Python `mock.patch("turbotap.cli.tesseract_tsv")` together with
/// `_tess_lib -> None`: setting it forces the binary path).
pub type TsvHook = Arc<dyn Fn(&[u8], u32, u32, i32, bool) -> Result<String> + Send + Sync>;
/// Test replacement for `ocr_raw` (Python `mock.patch.object(reader, "_ocr_raw")`).
pub type OcrRawHook = Arc<dyn Fn(&[u8], u32, u32, bool) -> Result<Vec<RawWords>> + Send + Sync>;

const INVERT: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = 255 - i as u8;
        i += 1;
    }
    t
};

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
    pub conditional_hits: u64,
    pub tsv_hook: Option<TsvHook>,
    pub ocr_raw_hook: Option<OcrRawHook>,
    pool: Mutex<Option<Arc<EnginePool>>>,
    warming: Mutex<Vec<JoinHandle<()>>>,
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
        let r = ScreenReader {
            scale,
            psm,
            n: workers.filter(|w| *w > 0).unwrap_or_else(default_workers),
            cache: HashMap::new(),
            last: None,
            last_ts: 0.0,
            last_lines: None,
            pt: None,
            pt_from_cache: false,
            dev: scale / 3.0, // typical 3x iOS Retina; auto-corrected after 1st grab
            shot_validator: None,
            conditional_hits: 0,
            tsv_hook: None,
            ocr_raw_hook: None,
            pool: Mutex::new(None),
            warming: Mutex::new(Vec::new()),
        };
        if warm_ocr {
            // Warm the engines while the first frame is in flight; a failure here
            // surfaces from the first read instead.
            let _ = r.ensure_ocr();
        }
        r
    }

    /// Default reader (scale 1.5, psm 11, warm engines).
    pub fn default_reader() -> ScreenReader {
        ScreenReader::new(1.5, None, 11, true)
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
            // Warm daemon: borrow pre-initialised engines; strip cache stays per reader.
            warm.lease(self.psm, self.n)?
        } else {
            let pool = Arc::new(EnginePool::new(self.psm, self.n, real_factory()));
            let mut warming = self.warming.lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..self.n {
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
                    for &v in row.iter().step_by(step) {
                        mn = mn.min(v);
                        mx = mx.max(v);
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

    /// OCR one strip (uniform-strip skip, dark-mode pre-inversion, Sauvola variables).
    pub fn ocr_raw(&self, raw: &[u8], w: u32, h: u32, sauvola: bool) -> Result<Vec<RawWords>> {
        if let Some(hook) = self.ocr_raw_hook.as_ref() {
            return hook(raw, w, h, sauvola);
        }
        let mut inverted: Option<Vec<u8>> = None;
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
                inverted = Some(raw.iter().map(|&v| INVERT[v as usize]).collect());
            }
        }
        let raw: &[u8] = inverted.as_deref().unwrap_or(raw);
        let tsv = match self.ensure_ocr()? {
            None => match self.tsv_hook.as_ref() {
                Some(hook) => hook(raw, w, h, self.psm, sauvola)?,
                None => tesseract_tsv_binary(raw, w, h, self.psm, sauvola)?,
            },
            Some(pool) => {
                let mut lease = Lease { pool: &pool, api: Some(pool.get()?) };
                let Some(api) = lease.api.as_mut() else { return Err(Error::runtime("no Tesseract engine")) };
                if sauvola {
                    api.set_variable(c"thresholding_method", c"2");
                    api.set_variable(c"thresholding_kfactor", c"0.15");
                    api.set_variable(c"thresholding_window_size", c"0.80");
                }
                let res = api.tsv(raw, w, h);
                if sauvola {
                    api.set_variable(c"thresholding_method", c"0");
                }
                drop(lease);
                res?
            }
        };
        Ok(parse_tsv(&tsv))
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
        let key = format!("{}\u{1f}{}\u{1f}{}", c.base_url, c.sid_str(), json::dumps(&sorted_keys(payload)));
        if self.shot_validator.as_ref().is_some_and(|v| v.key != key) {
            self.shot_validator = None;
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
                        return (200, v.content_type.clone(), v.body.clone());
                    }
                }
                // A 304 we cannot pair with our own retained bytes: never guess a frame.
                validator = None;
                self.shot_validator = None;
                if attempt == 0 {
                    if let Some(d) = deadline {
                        if clock::now() >= d {
                            return (0, String::new(), b"deadline exceeded".to_vec());
                        }
                        req_timeout = Some((d - clock::now()).max(0.05));
                    }
                    continue;
                }
                return (0, String::new(), b"unpaired 304 Not Modified".to_vec());
            }
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
            return (status, ct, body);
        }
        (0, String::new(), b"conditional screenshot failed".to_vec())
    }
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
                let req_timeout = rem()?;
                if use_raw {
                    if let Value::Object(m) = &mut payload {
                        m.insert("raw".into(), Value::Bool(true));
                    }
                    let (status, ct, body) = if self.conditional_ok(c) {
                        self.conditional_shot(c, &payload, req_timeout, eff_deadline)
                    } else {
                        c.req_raw(
                            "POST",
                            "/wda/fastScreenshot",
                            Some(&payload),
                            req_timeout.unwrap_or(crate::wda::DEFAULT_TIMEOUT),
                            eff_deadline,
                            None,
                        )
                    };
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
                        _ => return Err(Error::runtime("Empty screenshot response")),
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
            return Ok(Arc::new(img));
        }
        Err(Error::runtime("Empty screenshot response"))
    }

    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>> {
        use blake2::{digest::consts::U32, Blake2b, Digest};
        self.ensure_ocr()?;
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
            let digest = Blake2b::<U32>::digest(&data[a * w..b * w]);
            let mut k = Vec::with_capacity(33);
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
                vec![self.ocr_raw(uniq[0].0, w as u32, uniq[0].1 as u32, sauvola)]
            } else {
                let this: &ScreenReader = self;
                std::thread::scope(|s| {
                    let handles: Vec<_> = uniq
                        .iter()
                        .map(|(raw, sh, _)| {
                            let (raw, sh) = (*raw, *sh);
                            clock::spawn_scoped(s, move || this.ocr_raw(raw, w as u32, sh as u32, sauvola))
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
    client.ensure_session(None, true)?;
    let mut reader = ScreenReader::new(scale, None, 11, true);
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
