//! Sensing: active-app provenance, waiting, reading, settling, controls,
//! `read_full`, system alerts (Python Flow L2680-3133 and L4328-4893).
use super::Flow;
use crate::error::Result;
use crate::image::Frame;
use crate::ocr::Reader;
use crate::text::{Line, Match};
use crate::ui::Control;
use crate::wda::WdaClient;

/// Result of `GET /alert/text`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertStatus {
    Present,
    Absent,
    Error,
}

/// Python `_snapshot_ui_expectation` tuple.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiExpectation {
    pub expected_bundle: Option<String>,
    pub switched_from: Option<String>,
    pub enforce_bundle: bool,
    pub enforce_app_name: bool,
}

impl<R: Reader> Flow<R> {
    /// L2680. Defaults: max_wait 1.5, poll 0.03.
    pub fn wait_active_app(&mut self, max_wait: f64, poll: f64, deadline: Option<f64>) -> bool {
        let _ = (max_wait, poll, deadline);
        todo!("port L2680 _wait_active_app")
    }

    /// L2733.
    pub fn lines(&mut self, timeout: Option<f64>, deadline: Option<f64>) -> Result<Vec<Line>> {
        let _ = (timeout, deadline);
        todo!("port L2733 lines")
    }

    /// L2746. Default max_age 0.15.
    pub fn fresh_lines(&mut self, max_age: f64) -> Result<Option<Vec<Line>>> {
        let _ = max_age;
        todo!("port L2746 _fresh_lines")
    }

    /// L2756.
    pub fn read_sauvola(&mut self, img: Option<Frame>) -> Result<Option<Vec<Line>>> {
        let _ = img;
        todo!("port L2756 _read_sauvola")
    }

    /// L2766. Python defaults: timeout None, stable False, initial None, allow_disabled True.
    pub fn wait(&mut self, label: &str, timeout: Option<f64>, stable: bool, initial: Option<Match>, allow_disabled: bool) -> Result<Match> {
        let _ = (label, timeout, stable, initial, allow_disabled);
        todo!("port L2766 wait")
    }

    /// L2895.
    pub fn wait_any(&mut self, spec: &str, timeout: Option<f64>) -> Result<Match> {
        let _ = (spec, timeout);
        todo!("port L2895 wait_any")
    }

    /// L2976.
    pub fn gone(&mut self, label: &str, timeout: Option<f64>) -> Result<()> {
        let _ = (label, timeout);
        todo!("port L2976 gone")
    }

    /// L3023.
    pub fn assert_text(&mut self, arg: &str) -> Result<String> {
        let _ = arg;
        todo!("port L3023 assert_text")
    }

    /// L3042.
    pub fn assert_not_text(&mut self, arg: &str) -> Result<String> {
        let _ = arg;
        todo!("port L3042 assert_not_text")
    }

    /// L3061. Python defaults: timeout 2.0, start None, still 0.6.
    pub fn settle(&mut self, timeout: f64, start: Option<Frame>, still: f64) -> Result<bool> {
        let _ = (timeout, start, still);
        todo!("port L3061 settle")
    }

    /// L3084. Default timeout 0.45.
    pub fn changed_since(&mut self, before: Option<Frame>, timeout: f64, x: Option<i64>, y: Option<i64>) -> Result<bool> {
        let _ = (before, timeout, x, y);
        todo!("port L3084 changed_since")
    }

    /// L3097. Defaults: max_wait 0.5, still 1.2. Returns (prev, cur).
    pub fn settle_transition(&mut self, max_wait: f64, still: f64) -> Result<(Option<Frame>, Option<Frame>)> {
        let _ = (max_wait, still);
        todo!("port L3097 _settle_transition")
    }

    /// L4328.
    pub fn snapshot_ui_expectation(&self, now: Option<f64>) -> UiExpectation {
        let _ = now;
        todo!("port L4328 _snapshot_ui_expectation")
    }

    /// L4346.
    pub fn is_stale_ui(&self, app_name: &str, bundle_id: &str, expectation: Option<&UiExpectation>) -> bool {
        let _ = (app_name, bundle_id, expectation);
        todo!("port L4346 _is_stale_ui")
    }

    /// L4361 (`client` = main or side client).
    pub fn query_alert_text(client: &mut WdaClient, deadline: Option<f64>) -> (AlertStatus, Option<String>) {
        let _ = (client, deadline);
        todo!("port L4361 _query_alert_text")
    }

    /// L4405.
    pub fn active_alert_text(client: &mut WdaClient, deadline: Option<f64>) -> Option<String> {
        match Self::query_alert_text(client, deadline) {
            (AlertStatus::Present, t) => t,
            _ => None,
        }
    }

    /// L4410.
    pub fn active_alert_buttons(client: &mut WdaClient, deadline: Option<f64>) -> Vec<String> {
        let _ = (client, deadline);
        todo!("port L4410 _active_alert_buttons")
    }

    /// L4462. Python defaults: text False, timeout None, deadline None, fast_only False,
    /// allow_alert False.
    pub fn controls(&mut self, text: bool, timeout: Option<f64>, deadline: Option<f64>, fast_only: bool, allow_alert: bool) -> Result<Vec<Control>> {
        let _ = (text, timeout, deadline, fast_only, allow_alert);
        todo!("port L4462 controls")
    }

    /// L4667. Defaults: ui True, timeout 2.5. Returns (lines, ui or None when ui=false).
    pub fn read_full(&mut self, ui: bool, timeout: f64) -> Result<(Vec<Line>, Option<Vec<Control>>)> {
        let _ = (ui, timeout);
        todo!("port L4667 read_full")
    }
}
