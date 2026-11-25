//! Process environment changes in tests (Python `mock.patch.dict("os.environ", ...)`).
//!
//! The environment is process-wide and tests run on parallel threads: hold
//! [`env_lock`] for the whole test whenever it sets variables, or reads ones
//! another test may set. [`EnvVar`] restores the previous value on drop.
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialise tests that touch the process environment.
pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set (`Some`) or remove (`None`) one variable until dropped.
pub struct EnvVar {
    key: String,
    prev: Option<std::ffi::OsString>,
}

impl EnvVar {
    pub fn set(key: &str, value: Option<&str>) -> EnvVar {
        let prev = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        EnvVar { key: key.to_string(), prev }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}
