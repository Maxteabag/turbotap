//! On-device Apple Vision OCR via wda-turbo v6 `POST /wda/fastOCR`
//! (cli.py L5572-5680).
use crate::error::Result;
use crate::text::Line;
use crate::wda::WdaClient;

pub const VISION_MODES: [&str; 2] = ["accurate", "fast"];
pub const VISION_DEFAULT_SCALE: f64 = 0.5;
pub const VISION_BOX_TOLERANCE: f64 = 1e-3;

/// One validated line, box normalised to the screen (0..1, top-left origin).
#[derive(Clone, Debug, PartialEq)]
pub struct VisionLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub confidence: f64,
}

/// L5581-5647. Errors: `Error::VisionUnsupported`, `Error::Runtime`, `Error::Timeout`.
pub fn vision_ocr_raw(client: &mut WdaClient, mode: &str, scale: f64, deadline: Option<f64>) -> Result<Vec<VisionLine>> {
    let _ = (client, mode, scale, deadline);
    todo!("port L5581")
}

/// L5650-5658: Vision lines as [`Line`]s in points.
pub fn vision_lines(client: &mut WdaClient, screen_pt: (i64, i64), mode: &str, scale: f64, deadline: Option<f64>) -> Result<Vec<Line>> {
    let _ = (client, screen_pt, mode, scale, deadline);
    todo!("port L5650")
}

/// L5661-5680 (`turbotap vision-ocr`).
pub fn cmd_vision_ocr(client: &mut WdaClient, mode: &str, scale: f64) -> Result<()> {
    let _ = (client, mode, scale);
    todo!("port L5661")
}
