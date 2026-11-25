//! One fresh English LSTM instance per call. Return metrics only.
use crate::Result;
use image::GrayImage;
use serde_json::{Value, json};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    fs::File,
    os::fd::AsRawFd,
    time::Instant,
};

#[link(name = "tesseract")]
unsafe extern "C" {
    fn TessBaseAPICreate() -> *mut c_void;
    fn TessBaseAPIDelete(api: *mut c_void);
    fn TessBaseAPIInit2(
        api: *mut c_void,
        path: *const c_char,
        lang: *const c_char,
        mode: c_int,
    ) -> c_int;
    fn TessBaseAPISetPageSegMode(api: *mut c_void, mode: c_int);
    fn TessBaseAPISetImage(
        api: *mut c_void,
        pixels: *const u8,
        width: c_int,
        height: c_int,
        bpp: c_int,
        stride: c_int,
    );
    fn TessBaseAPIGetUTF8Text(api: *mut c_void) -> *mut c_char;
    fn TessDeleteText(text: *const c_char);
}

pub fn measure(image: &GrayImage) -> Result<Value> {
    let (w, h) = (
        i32::try_from(image.width())?,
        i32::try_from(image.height())?,
    );
    // This foreground-only CLI performs one OCR call. Native library diagnostics
    // can contain input data. Keep them off its diagnostic stream.
    let saved = std::io::stderr().as_fd().try_clone_to_owned()?;
    use std::os::fd::AsFd;
    let null = File::options().write(true).open("/dev/null")?;
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err("stderr redirect failed".into());
    }
    let result = unsafe {
        let start = Instant::now();
        let api = TessBaseAPICreate();
        if api.is_null() {
            Err("OCR unavailable".into())
        } else {
            let result = if TessBaseAPIInit2(api, std::ptr::null(), c"eng".as_ptr(), 1) != 0 {
                Err("OCR init failed".into())
            } else {
                TessBaseAPISetPageSegMode(api, 6);
                let init_us = start.elapsed().as_micros() as u64;
                let read_start = Instant::now();
                // The contiguous L8 buffer remains alive through recognition.
                TessBaseAPISetImage(api, image.as_raw().as_ptr(), w, h, 1, w);
                let text = TessBaseAPIGetUTF8Text(api);
                if text.is_null() {
                    Err("OCR failed".into())
                } else {
                    let words = CStr::from_ptr(text)
                        .to_bytes()
                        .split(|b| b.is_ascii_whitespace())
                        .filter(|s| !s.is_empty())
                        .count();
                    TessDeleteText(text);
                    Ok(
                        json!({"words":words,"init_us":init_us,"recognize_us":read_start.elapsed().as_micros() as u64}),
                    )
                }
            };
            TessBaseAPIDelete(api);
            result
        }
    };
    if unsafe { libc::dup2(saved.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err("stderr restore failed".into());
    }
    result
}
