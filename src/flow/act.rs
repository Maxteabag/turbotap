//! Gestures and forms: drag/swipe/scroll/back, picker, url, toggle, slider,
//! clipboard, search, keyboard dismissal (Python Flow L3920-4327, L4894-4964,
//! L5061-5209).
use super::Flow;
use crate::error::Result;
use crate::ocr::Reader;
use crate::text::Match;

impl<R: Reader> Flow<R> {
    /// L3920. Defaults: duration 0.12, release 0.02.
    pub fn drag(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, duration: f64, release: f64) -> Result<()> {
        let _ = (x0, y0, x1, y1, duration, release);
        todo!("port L3920 drag")
    }

    /// L3935.
    pub fn swipe(&mut self, direction: &str) -> Result<()> {
        let _ = direction;
        todo!("port L3935 swipe")
    }

    /// L3948.
    pub fn swipe_gesture(&mut self, spec: &str) -> Result<String> {
        let _ = spec;
        todo!("port L3948 swipe_gesture")
    }

    /// L3982.
    pub fn set_picker(&mut self, spec: &str) -> Result<String> {
        let _ = spec;
        todo!("port L3982 set_picker")
    }

    /// L4009.
    pub fn open_url(&mut self, url: &str) -> Result<String> {
        let _ = url;
        todo!("port L4009 open_url")
    }

    /// L4028.
    pub fn toggle(&mut self, spec: &str) -> Result<String> {
        let _ = spec;
        todo!("port L4028 toggle")
    }

    /// L4116.
    pub fn set_slider(&mut self, spec: &str) -> Result<String> {
        let _ = spec;
        todo!("port L4116 set_slider")
    }

    /// L4243.
    pub fn clipboard(&mut self, content_type: &str) -> Result<String> {
        let _ = content_type;
        todo!("port L4243 clipboard")
    }

    /// L4252.
    pub fn set_clipboard(&mut self, arg: &str, content_type: &str) -> Result<String> {
        let _ = (arg, content_type);
        todo!("port L4252 set_clipboard")
    }

    /// L4261.
    pub fn scroll(&mut self, direction: &str) -> Result<()> {
        let _ = direction;
        todo!("port L4261 scroll")
    }

    /// L4269. Defaults: where None, max_swipes 8, direction "down".
    pub fn scroll_to(&mut self, label: &str, where_: Option<&str>, max_swipes: usize, direction: &str) -> Result<Match> {
        let _ = (label, where_, max_swipes, direction);
        todo!("port L4269 scroll_to")
    }

    /// L4894.
    pub fn search(&mut self, query: &str) -> Result<String> {
        let _ = query;
        todo!("port L4894 search")
    }

    /// L4947.
    pub fn back(&mut self) -> Result<()> {
        todo!("port L4947 back")
    }

    /// L5150 (with `_visible_keyboard_rect` L5061 and
    /// `_stock_keyboard_dismiss_candidates` L5081).
    pub fn dismiss_kb(&mut self) -> Result<String> {
        todo!("port L5150 dismiss_kb")
    }
}
