use super::{parse_coord, tap::exact_control_at, AlertStatus, Flow};
use crate::error::{Error, Result};
use crate::ocr::Reader;
use crate::{clock, json, ui};
use serde_json::{json as j, Value};

const APP_STORE: &str = "com.apple.AppStore";

impl<R: Reader> Flow<R> {
    pub(super) fn appinstall(&mut self, arg: &str) -> Result<Value> {
        let usage = || Error::value("appinstall expects X,Y=INSTALL|READY with two distinct, nonempty exact labels");
        let (point, labels) = arg.split_once('=').ok_or_else(usage)?;
        let (x, y) = point.split_once(',').ok_or_else(usage)?;
        let (x, y) = (parse_coord(x)?, parse_coord(y)?);
        let (install, ready) = labels.split_once('|').ok_or_else(usage)?;
        if install.trim().is_empty() || ready.trim().is_empty() || ready.contains('|') || install == ready {
            return Err(usage());
        }
        if !self.timeout.is_finite() || self.timeout <= 0.0 {
            return Err(Error::value("appinstall needs a finite, positive --timeout"));
        }

        let end = clock::now() + self.timeout;
        self.last_alert_text = None;
        self.last_alert_buttons = None;
        self.c.stop_check().map_err(Error::flow)?;
        let screen = self
            .r
            .pt_size(&mut self.c, false, Some(end))
            .map_err(|e| Error::flow(format!("appinstall: could not determine screen size ({e}); nothing pressed")))?;
        let controls = self
            .appinstall_controls(screen, end)
            .map_err(|e| Error::flow(format!("{e}; nothing pressed")))?;
        let control = exact_control_at(&controls, x, y, install)?
            .filter(|c| !c.is_disabled())
            .ok_or_else(|| {
                Error::flow(format!(
                    "appinstall: no enabled control labelled exactly {} near {x},{y}; nothing pressed",
                    json::py_repr_str(install)
                ))
            })?;
        if exact_control_at(&controls, x, y, ready)?.is_some_and(|c| !c.is_disabled()) {
            return Err(Error::flow(format!(
                "appinstall: {} is already enabled near {x},{y}; nothing pressed",
                json::py_repr_str(ready)
            )));
        }

        self.guard_with(&control.label, "tap", true)?;
        self.mark_action(false, false);
        self.c.tap_once_in_app(control.x, control.y, APP_STORE, end).map_err(|e| {
            Error::flow(format!(
                "appinstall: tap unconfirmed ({e}); inspect the screen before taking another action"
            ))
        })?;
        self.last_action_ts = clock::now();
        self.last_tap_ts = self.last_action_ts;
        self.bundle = Some(APP_STORE.into());
        self.pending_bundle = None;
        self.open_confirmed = None;
        self.switched_from_app_name = None;

        let timeout = || {
            Error::flow(format!(
                "appinstall: tap acknowledged; timed out waiting for {} near {x},{y}",
                json::py_repr_str(ready)
            ))
        };
        while clock::now() < end {
            let next_poll = clock::now() + 1.0;
            let current = match self.appinstall_controls(screen, end) {
                Ok(controls) => controls,
                Err(Error::DeadlineExceeded(_)) => return Err(timeout()),
                Err(e) => return Err(Error::flow(format!("appinstall: tap acknowledged; {e}"))),
            };
            let hit = exact_control_at(&current, x, y, ready).map_err(|e| Error::flow(format!("appinstall: tap acknowledged; {e}")))?;
            if let Some(hit) = hit.filter(|c| !c.is_disabled()) {
                return Ok(j!({
                    "status": "ready",
                    "bundle": APP_STORE,
                    "pressed": {"label": control.label, "x": control.x, "y": control.y},
                    "ready": {"label": hit.label, "x": hit.x, "y": hit.y},
                }));
            }
            clock::sleep((end.min(next_poll) - clock::now()).max(0.0));
        }
        self.c
            .stop_check()
            .map_err(|e| Error::flow(format!("appinstall: tap acknowledged; {e}")))?;
        Err(timeout())
    }

    fn appinstall_controls(&mut self, screen: (i64, i64), end: f64) -> Result<Vec<ui::Control>> {
        self.c.stop_check().map_err(Error::flow)?;
        let remaining = end - clock::now();
        if remaining <= 0.0 {
            return Err(Error::DeadlineExceeded("appinstall: deadline exceeded".into()));
        }
        let (controls, _, bundle) = ui::fetch_ui_controls_full(&mut self.c, screen, false, None, 0, false, Some(remaining));
        // Check after the snapshot so an alert takes precedence over a control
        // left visible behind it, or a snapshot from SpringBoard.
        let alert = Self::query_alert_text(&mut self.c, Some(end));
        self.c.stop_check().map_err(Error::flow)?;
        match alert {
            (AlertStatus::Present, title) => {
                let buttons = Self::active_alert_buttons(&mut self.c, Some(end));
                self.last_alert_text = title;
                let detail = if buttons.is_empty() { " (live actions unavailable)" } else { "" };
                self.last_alert_buttons = Some(buttons);
                return Err(Error::flow(format!("appinstall: active iOS alert needs caller attention{detail}")));
            }
            (AlertStatus::Error, _) if clock::now() >= end => {
                return Err(Error::DeadlineExceeded("appinstall: deadline exceeded".into()));
            }
            (AlertStatus::Error, reason) => {
                return Err(Error::flow(format!(
                    "appinstall: could not verify iOS alert state ({})",
                    reason.unwrap_or_default()
                )))
            }
            (AlertStatus::Absent, _) => {}
        }
        if clock::now() >= end {
            return Err(Error::DeadlineExceeded("appinstall: deadline exceeded".into()));
        }
        let controls = controls.filter(|_| bundle == APP_STORE).ok_or_else(|| {
            Error::flow(format!(
                "appinstall: expected a live {APP_STORE} accessibility snapshot (observed {})",
                json::py_repr_str(&bundle)
            ))
        })?;
        if controls.iter().any(|c| c.in_alert) {
            return Err(Error::flow(
                "appinstall: accessibility snapshot shows an alert; live title/actions unavailable",
            ));
        }
        Ok(controls)
    }
}
