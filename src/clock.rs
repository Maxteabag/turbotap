//! Monotonic clock and sleep with per-thread test hooks.
//!
//! Every time read and sleep in turbotap goes through [`now`], [`wall`] and
//! [`sleep`] so tests can replace them (the Rust analogue of the Python suite's
//! `mock.patch("turbotap.cli.time.sleep")` / `time.monotonic`). Hooks are
//! thread-local; code that spawns worker threads must carry the parent's hook into
//! the child with [`current`] + [`with_hook`] (see [`spawn_scoped`]).
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A replaceable time source.
pub trait Clock: Send + Sync {
    /// Monotonic seconds (any fixed origin).
    fn now(&self) -> f64;
    /// Seconds since the Unix epoch.
    fn wall(&self) -> f64 {
        real_wall()
    }
    fn sleep(&self, secs: f64);
}

fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// Real monotonic seconds since the first clock use in this process (never 0.0
/// after start-up: offset by 1000 s so "0.0 = never" sentinels stay unambiguous).
pub fn real_now() -> f64 {
    1000.0 + origin().elapsed().as_secs_f64()
}

pub fn real_wall() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn real_sleep(secs: f64) {
    if secs > 0.0 && secs.is_finite() {
        std::thread::sleep(Duration::from_secs_f64(secs));
    }
}

thread_local! {
    static HOOK: RefCell<Option<Arc<dyn Clock>>> = const { RefCell::new(None) };
}

pub fn now() -> f64 {
    HOOK.with(|h| h.borrow().as_ref().map(|c| c.now())).unwrap_or_else(real_now)
}

pub fn wall() -> f64 {
    HOOK.with(|h| h.borrow().as_ref().map(|c| c.wall())).unwrap_or_else(real_wall)
}

thread_local! {
    /// Seconds slept through [`sleep`] on this thread (the timing log reads deltas).
    static SLEPT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// Total seconds this thread has slept through [`sleep`] (requested durations).
pub fn slept_total() -> f64 {
    SLEPT.with(|c| c.get())
}

pub fn sleep(secs: f64) {
    if secs > 0.0 && secs.is_finite() {
        SLEPT.with(|c| c.set(c.get() + secs));
    }
    let hook = HOOK.with(|h| h.borrow().clone());
    match hook {
        Some(c) => c.sleep(secs),
        None => real_sleep(secs),
    }
}

/// The hook installed on this thread (to hand to worker threads).
pub fn current() -> Option<Arc<dyn Clock>> {
    HOOK.with(|h| h.borrow().clone())
}

/// Restores the previous hook on drop.
pub struct HookGuard(Option<Arc<dyn Clock>>);

impl Drop for HookGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        HOOK.with(|h| *h.borrow_mut() = prev);
    }
}

/// Install `c` on this thread until the guard drops.
pub fn install(c: Arc<dyn Clock>) -> HookGuard {
    let prev = HOOK.with(|h| h.borrow_mut().replace(c));
    HookGuard(prev)
}

/// Run `f` with `hook` installed on the current thread.
pub fn with_hook<T>(hook: Option<Arc<dyn Clock>>, f: impl FnOnce() -> T) -> T {
    let prev = HOOK.with(|h| std::mem::replace(&mut *h.borrow_mut(), hook));
    let _g = HookGuard(prev);
    f()
}

/// `std::thread::Scope::spawn` that carries this thread's clock hook into the child.
pub fn spawn_scoped<'scope, 'env, T, F>(
    s: &'scope std::thread::Scope<'scope, 'env>,
    f: F,
) -> std::thread::ScopedJoinHandle<'scope, T>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    let hook = current();
    s.spawn(move || with_hook(hook, f))
}

/// `std::thread::spawn` that carries this thread's clock hook into the child.
pub fn spawn<T, F>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let hook = current();
    std::thread::spawn(move || with_hook(hook, f))
}

/// Real time, but `sleep` returns immediately and is recorded
/// (Python: `mock.patch("turbotap.cli.time.sleep")`).
#[derive(Default)]
pub struct SkipSleep {
    pub sleeps: Mutex<Vec<f64>>,
}

impl Clock for SkipSleep {
    fn now(&self) -> f64 {
        real_now()
    }
    fn sleep(&self, secs: f64) {
        self.sleeps.lock().unwrap().push(secs);
    }
}

/// Real time plus a virtual offset: `sleep` returns immediately and advances the
/// clock by the requested amount, so deadline loops run instantly but still end.
#[derive(Default)]
pub struct VirtualClock {
    offset_nanos: AtomicU64,
    pub sleeps: Mutex<Vec<f64>>,
}

impl VirtualClock {
    pub fn advance(&self, secs: f64) {
        if secs > 0.0 {
            self.offset_nanos.fetch_add((secs * 1e9) as u64, Ordering::SeqCst);
        }
    }
    pub fn offset(&self) -> f64 {
        self.offset_nanos.load(Ordering::SeqCst) as f64 / 1e9
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> f64 {
        real_now() + self.offset()
    }
    fn wall(&self) -> f64 {
        real_wall() + self.offset()
    }
    fn sleep(&self, secs: f64) {
        self.sleeps.lock().unwrap().push(secs);
        self.advance(secs);
    }
}
