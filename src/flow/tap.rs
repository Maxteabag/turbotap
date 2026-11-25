//! Tapping: `tap:`, `tapif:`, `tapel:`, `tapbatch:`, long-press, double-tap, zoom
//! (Python Flow L3134-3919 and L4965-5060).
use super::Flow;
use crate::error::Result;
use crate::ocr::Reader;
use crate::text::Match;
use crate::ui::Control;

impl<R: Reader> Flow<R> {
    /// L3134.
    pub fn tap_point(&mut self, x: i64, y: i64, text: &str, deadline: Option<f64>) -> Result<()> {
        let _ = (x, y, text, deadline);
        todo!("port L3134 tap_point")
    }

    /// L3231.
    pub fn tap_text(&mut self, label: &str) -> Result<Match> {
        let _ = label;
        todo!("port L3231 tap_text")
    }

    /// L3473. Default timeout 1.0. Returns the tapped text or "skipped".
    pub fn tap_if(&mut self, label: &str, timeout: f64) -> Result<String> {
        let _ = (label, timeout);
        todo!("port L3473 tap_if")
    }

    /// L3705.
    pub fn tap_control(&mut self, c: &Control, deadline: Option<f64>) -> Result<Match> {
        let _ = (c, deadline);
        todo!("port L3705 tap_control")
    }

    /// L3713.
    pub fn switch_near(&mut self, label: &str, m: &Match, controls: Option<&[Control]>) -> Result<Option<Control>> {
        let _ = (label, m, controls);
        todo!("port L3713 switch_near")
    }

    /// L3721.
    pub fn input_near(&mut self, label: &str, m: &Match, controls: Option<&[Control]>) -> Result<Option<Control>> {
        let _ = (label, m, controls);
        todo!("port L3721 input_near")
    }

    /// L3742.
    pub fn tap_form_button(&mut self, label: &str, where_: Option<&str>) -> Result<Option<Match>> {
        let _ = (label, where_);
        todo!("port L3742 tap_form_button")
    }

    /// L3760. Returns the step result text.
    pub fn tap_el(&mut self, label: &str, timeout: Option<f64>) -> Result<String> {
        let _ = (label, timeout);
        todo!("port L3760 tap_el")
    }

    /// L3784.
    pub fn find_target(&mut self, label: &str, timeout: Option<f64>) -> Result<Match> {
        let _ = (label, timeout);
        todo!("port L3784 _find_target")
    }

    /// L3845. Default duration 0.65.
    pub fn long_press_point(&mut self, x: i64, y: i64, text: &str, duration: f64) -> Result<()> {
        let _ = (x, y, text, duration);
        todo!("port L3845 long_press_point")
    }

    /// L3854.
    pub fn long_press_text(&mut self, label: &str, duration: f64) -> Result<Match> {
        let _ = (label, duration);
        todo!("port L3854 long_press_text")
    }

    /// L3863.
    pub fn double_tap_point(&mut self, x: i64, y: i64, text: &str, interval: Option<f64>, duration: Option<f64>) -> Result<serde_json::Value> {
        let _ = (x, y, text, interval, duration);
        todo!("port L3863 double_tap_point")
    }

    /// L3873.
    pub fn double_tap_text(&mut self, label: &str) -> Result<Match> {
        let _ = label;
        todo!("port L3873 double_tap_text")
    }

    /// L3882.
    pub fn zoom(&mut self, spec: &str) -> Result<String> {
        let _ = spec;
        todo!("port L3882 zoom")
    }

    /// L4965.
    pub fn tap_batch(&mut self, spec: &str) -> Result<String> {
        let _ = spec;
        todo!("port L4965 tap_batch")
    }
}
