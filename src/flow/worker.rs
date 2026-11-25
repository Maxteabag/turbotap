//! The single-thread UI worker (Python `Flow._get_ui_worker` / `_ui_pool`): a
//! dedicated thread owning a side [`WdaClient`] on its own keep-alive connection,
//! so a `/wda/fastUI` fetch overlaps screen capture + OCR on the main client.
use crate::clock;
use crate::wda::WdaClient;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce(&mut WdaClient) + Send>;

pub struct UiWorker {
    tx: Option<mpsc::Sender<Job>>,
    handle: Option<JoinHandle<WdaClient>>,
    cancelled: Arc<AtomicBool>,
}

/// Probe results copied from the main client before each job (Python syncs `sid`
/// and `turbo` always and every other capability only when the main client has
/// probed it).
#[derive(Clone, Debug)]
pub struct ProbeSnapshot {
    pub sid: Option<String>,
    pub turbo: Option<bool>,
    pub fast_ui: Option<bool>,
    pub raw_shot: Option<bool>,
    pub fast_keys: Option<bool>,
    pub fast_double_tap: Option<bool>,
    pub fast_pinch: Option<bool>,
    pub fast_tap_batch: Option<bool>,
    pub fast_launch: Option<bool>,
}

impl ProbeSnapshot {
    pub fn of(c: &WdaClient) -> ProbeSnapshot {
        ProbeSnapshot {
            sid: c.sid.clone(),
            turbo: c.turbo,
            fast_ui: c.fast_ui,
            raw_shot: c.raw_shot,
            fast_keys: c.fast_keys,
            fast_double_tap: c.fast_double_tap,
            fast_pinch: c.fast_pinch,
            fast_tap_batch: c.fast_tap_batch,
            fast_launch: c.fast_launch,
        }
    }

    pub fn apply(&self, side: &mut WdaClient) {
        side.sid = self.sid.clone();
        side.turbo = self.turbo;
        macro_rules! sync {
            ($($f:ident),*) => {$( if self.$f.is_some() { side.$f = self.$f; } )*};
        }
        sync!(fast_ui, raw_shot, fast_keys, fast_double_tap, fast_pinch, fast_tap_batch, fast_launch);
    }
}

impl UiWorker {
    /// Start the worker thread with `side` (carries this thread's clock hook).
    pub fn new(mut side: WdaClient) -> UiWorker {
        let (tx, rx) = mpsc::channel::<Job>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let handle = clock::spawn(move || {
            while let Ok(job) = rx.recv() {
                if flag.load(Ordering::SeqCst) {
                    continue; // cancel_futures=True: queued jobs never start
                }
                job(&mut side);
            }
            side
        });
        UiWorker { tx: Some(tx), handle: Some(handle), cancelled }
    }

    /// Queue `f` (after applying `probes` to the side client); the receiver yields its
    /// result. Dropping the receiver abandons the result, not the running request.
    pub fn submit<T, F>(&self, probes: ProbeSnapshot, f: F) -> mpsc::Receiver<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut WdaClient) -> T + Send + 'static,
    {
        let (rtx, rrx) = mpsc::channel();
        if let Some(tx) = self.tx.as_ref() {
            let job: Job = Box::new(move |side: &mut WdaClient| {
                probes.apply(side);
                let _ = rtx.send(f(side));
            });
            let _ = tx.send(job);
        }
        rrx
    }

    /// Cancel queued jobs, wait for a running one, and release the side
    /// connection to the daemon pool (Python `Flow.close`).
    pub fn close(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.tx = None;
        if let Some(h) = self.handle.take() {
            if let Ok(mut side) = h.join() {
                side.release_conn();
            }
        }
    }
}

impl Drop for UiWorker {
    fn drop(&mut self) {
        self.close();
    }
}
