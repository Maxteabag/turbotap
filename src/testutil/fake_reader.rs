//! Deterministic offline `Reader` double (Python `tests/helpers.py::FakeReader`).
//!
//! Cloning shares state, so a test can keep a handle (to push frames or read
//! counters) after moving the reader into a `Flow`, or from a mock `on_action` hook.
use crate::error::Result;
use crate::image::Frame;
use crate::ocr::{ReadOpts, Reader};
use crate::text::Line;
use crate::wda::WdaClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

pub enum SauvolaLines {
    Fixed(Vec<Line>),
    Func(Box<dyn FnMut(Option<&Frame>) -> Vec<Line> + Send>),
}

pub struct FakeInner {
    pub pt: (i64, i64),
    pub scale: f64,
    pub frames: Vec<(Frame, Vec<Line>)>,
    pub idx: usize,
    pub sauvola_lines: Option<SauvolaLines>,
    pub last: Option<Frame>,
    pub last_ts: f64,
    pub last_lines: Option<Vec<Line>>,
    pub grab_calls: usize,
    pub read_calls: usize,
    pub sauvola_calls: usize,
    pub img_lines: HashMap<usize, Vec<Line>>,
}

fn key(img: &Frame) -> usize {
    Arc::as_ptr(img) as usize
}

impl FakeInner {
    fn next_frame(&mut self) -> (Frame, Vec<Line>) {
        let i = self.idx.min(self.frames.len() - 1);
        if self.idx < self.frames.len() - 1 {
            self.idx += 1;
        }
        self.frames[i].clone()
    }
}

#[derive(Clone)]
pub struct FakeReader(pub Arc<Mutex<FakeInner>>);

impl FakeReader {
    /// Python `FakeReader(frames=None, pt=(430, 932), scale=1.5)`; no frames = one
    /// white 120x240 frame with no lines.
    pub fn new(frames: Vec<(Frame, Vec<Line>)>) -> FakeReader {
        FakeReader::with(frames, (430, 932), 1.5)
    }

    pub fn with(frames: Vec<(Frame, Vec<Line>)>, pt: (i64, i64), scale: f64) -> FakeReader {
        let frames = if frames.is_empty() { vec![(super::solid_img(255, (120, 240)), vec![])] } else { frames };
        let mut img_lines = HashMap::new();
        for (img, lines) in &frames {
            img_lines.insert(key(img), lines.clone());
        }
        FakeReader(Arc::new(Mutex::new(FakeInner {
            pt,
            scale,
            frames,
            idx: 0,
            sauvola_lines: None,
            last: None,
            last_ts: 0.0,
            last_lines: None,
            grab_calls: 0,
            read_calls: 0,
            sauvola_calls: 0,
            img_lines,
        })))
    }

    pub fn lock(&self) -> MutexGuard<'_, FakeInner> {
        self.0.lock().unwrap()
    }

    pub fn set_sauvola_lines(&self, s: SauvolaLines) {
        self.lock().sauvola_lines = Some(s);
    }

    /// Append a frame and make it the next one returned.
    pub fn push_frame(&self, img: Frame, lines: Vec<Line>) {
        let mut g = self.lock();
        g.img_lines.insert(key(&img), lines.clone());
        g.frames.push((img, lines));
        g.idx = g.frames.len() - 1;
    }

    pub fn set_frames(&self, frames: Vec<(Frame, Vec<Line>)>) {
        let mut g = self.lock();
        for (img, lines) in &frames {
            g.img_lines.insert(key(img), lines.clone());
        }
        g.frames = frames;
        g.idx = 0;
    }

    pub fn grab_calls(&self) -> usize {
        self.lock().grab_calls
    }
    pub fn read_calls(&self) -> usize {
        self.lock().read_calls
    }
    pub fn sauvola_calls(&self) -> usize {
        self.lock().sauvola_calls
    }
    pub fn idx(&self) -> usize {
        self.lock().idx
    }
}

impl Reader for FakeReader {
    fn scale(&self) -> f64 {
        self.lock().scale
    }

    fn pt_size(&mut self, _c: &mut WdaClient, _force_refresh: bool, _deadline: Option<f64>) -> Result<(i64, i64)> {
        Ok(self.lock().pt)
    }

    fn grab(&mut self, _c: &mut WdaClient, _y0: i64, _y1: Option<i64>, _t: Option<f64>, _d: Option<f64>) -> Result<Frame> {
        let mut g = self.lock();
        g.grab_calls += 1;
        Ok(g.next_frame().0)
    }

    fn read(&mut self, _c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>> {
        let mut g = self.lock();
        if opts.sauvola {
            g.sauvola_calls += 1;
            let img = opts.img.clone();
            return Ok(match g.sauvola_lines.as_mut() {
                Some(SauvolaLines::Fixed(v)) => v.clone(),
                Some(SauvolaLines::Func(f)) => f(img.as_ref()),
                None => vec![],
            });
        }
        g.read_calls += 1;
        let fresh_grab = opts.img.is_none();
        let (img, lines) = match opts.img {
            None => {
                g.grab_calls += 1;
                g.next_frame()
            }
            Some(img) => {
                let lines = match g.img_lines.get(&key(&img)) {
                    Some(l) => l.clone(),
                    None => {
                        let i = g.idx.min(g.frames.len() - 1);
                        g.frames[i].1.clone()
                    }
                };
                (img, lines)
            }
        };
        let full = opts.y0 == 0 && opts.y1.is_none();
        if full {
            g.last = Some(img);
            g.last_lines = Some(lines.clone());
            if fresh_grab {
                g.last_ts = crate::clock::now();
            }
        } else {
            g.last = Some(img);
            g.last_lines = None;
            g.last_ts = 0.0;
        }
        Ok(lines)
    }

    fn last(&self) -> Option<Frame> {
        self.lock().last.clone()
    }
    fn last_ts(&self) -> f64 {
        self.lock().last_ts
    }
    fn last_lines(&self) -> Option<Vec<Line>> {
        self.lock().last_lines.clone()
    }
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>) {
        let mut g = self.lock();
        g.last = img;
        g.last_lines = lines;
        g.last_ts = ts.unwrap_or_else(crate::clock::now);
    }
    fn invalidate_last(&mut self) {
        let mut g = self.lock();
        g.last_ts = 0.0;
        g.last_lines = None;
    }
}
