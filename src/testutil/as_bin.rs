//! Lets the test executable act as the `turbotap` binary, for daemon end-to-end
//! tests (Python ran `python -m turbotap` in a subprocess).
//!
//! `cargo test --lib` does not build `src/main.rs`, and the daemon re-executes
//! `current_exe()` to start itself. So the test executable carries an ELF
//! `.init_array` (Mach-O `__mod_init_func`) entry that runs before libtest's `main`:
//! when [`AS_BIN_ENV`] is `1` it runs `cli::main_with(argv[1..])` and exits with its
//! code; otherwise it returns and the tests run as usual. A daemon started this way
//! re-executes the same test executable with the same environment, so it, too, runs
//! as `turbotap`.
use std::ffi::{c_char, c_int, CStr};

pub const AS_BIN_ENV: &str = "TURBOTAP_TEST_AS_BIN";

/// The current test executable (run it with `AS_BIN_ENV=1` to get `turbotap`).
pub fn exe() -> std::path::PathBuf {
    std::env::current_exe().unwrap()
}

extern "C" fn as_bin_init(argc: c_int, argv: *const *const c_char, _envp: *const *const c_char) {
    // SAFETY: getenv on a NUL-terminated literal; argv holds argc valid C strings
    // (the loader passes main's arguments to init functions).
    unsafe {
        let v = libc::getenv(c"TURBOTAP_TEST_AS_BIN".as_ptr());
        if v.is_null() || CStr::from_ptr(v).to_bytes() != b"1" {
            return;
        }
        // Rust's runtime normally ignores SIGPIPE in main; we run before it.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        let mut args = Vec::new();
        for i in 1..argc.max(0) as isize {
            let p = *argv.offset(i);
            if p.is_null() {
                break;
            }
            args.push(CStr::from_ptr(p).to_string_lossy().into_owned());
        }
        if let Some(path) = std::env::var_os("TURBOTAP_TEST_TIMINGS_FILE") {
            crate::timings::TEST_PATH.with(|p| p.replace(Some(path.into())));
        }
        let code = std::panic::catch_unwind(|| crate::cli::main_with(&args)).unwrap_or(101);
        std::process::exit(code);
    }
}

#[cfg(target_os = "linux")]
#[used]
#[link_section = ".init_array"]
static AS_BIN_INIT: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = as_bin_init;

#[cfg(target_os = "macos")]
#[used]
#[link_section = "__DATA,__mod_init_func"]
static AS_BIN_INIT: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = as_bin_init;
