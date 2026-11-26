//! turbotap: fast, generic WebDriverAgent (WDA) controller for physical iOS devices.
//!
//! One standalone binary bundles multi-step UI flows in a persistent keep-alive
//! process with device-cropped fast screenshots, in-process parallel libtesseract
//! OCR (loaded at runtime with `dlopen`), lightweight accessibility-tree queries,
//! screen-size-agnostic passcode unlock, verified form/PIN input and an opt-in warm
//! Unix-socket daemon.
//!
//! Module map (ported 1:1 from the Python v0.10.0 `cli.py` / `daemon.py`):
//! - [`config`]  config.json, env overrides, cache dir, passcode source
//! - [`guard`]   foreground app guard (`allowed_apps`, `--allow-app`)
//! - [`stop`]    stop file kill switch (`stop_file`, `TURBOTAP_STOP_FILE`, exit 6)
//! - [`wda`]     keep-alive HTTP/1.1 WDA client, fast routes + stock fallbacks, unlock, preflight
//! - [`image`]   8-bit grayscale frames, decode, frame-diff primitives
//! - [`ocr`]     libtesseract (dlopen) / binary fallback, `ScreenReader`, warm engines
//! - [`text`]    OCR `Line`/`Match`, `find_text`, regions, diacritic folding
//! - [`ui`]      `/wda/fastUI` + `/source` parsing, control matching
//! - [`vision`]  on-device Apple Vision OCR (`/wda/fastOCR`)
//! - [`apps`]    app display name -> bundle ID resolution
//! - [`flow`]    the multi-step `Flow` engine and every flow verb
//! - [`daemon`]  warm local daemon (`turbotap daemon start|stop|status`)
//! - [`deploy`] / [`skills`]  embedded WDA patch + deploy script, agent skills
//! - [`cli`]     argument parsing and subcommands
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

pub mod apps;
pub mod cli;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod deploy;
pub mod devicectl;
pub mod error;
pub mod flow;
pub mod guard;
pub mod image;
pub mod json;
pub mod ocr;
pub mod out;
pub mod recipes;
pub mod skills;
pub mod stop;
pub mod text;
pub mod timings;
pub mod ui;
pub mod vision;
pub mod wda;

#[cfg(test)]
pub mod testutil;
#[cfg(test)]
mod tests;

pub use error::{Error, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
