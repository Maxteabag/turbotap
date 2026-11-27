//! Optional cross-invocation CLI redaction. The private registry holds keyed
//! fingerprints only. A writer commits them before sending a credential and
//! observations check the registry after collecting device data.
use crate::{Error, Result};
use blake2::digest::{KeyInit, Mac};
use blake2::{Blake2s256, Blake2sMac256, Digest};
use serde_json::Value;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use unicode_normalization::UnicodeNormalization;

const DIR_ENV: &str = "TURBOTAP_REDACTION_DIR";
pub(crate) const KEY_ENV: &str = "TURBOTAP_REDACTION_KEY";
const FILE_NAME: &str = "fingerprints-v1";
const ANCHOR_NAME: &str = "fingerprints-v1.anchor";
const MASK: &str = "<redacted>";
const STATE_ERROR: &str = "credential redaction state is unavailable or invalid";
const TAG_BYTES: usize = 32;
const RECORD_BYTES: usize = 3 + TAG_BYTES;
const MAX_CHARS: usize = 256;
const MAX_REGISTRY_BYTES: usize = 512 * 1024;
const TEXT: u8 = 0;
const DIGITS: u8 = 1;

type Tag = [u8; TAG_BYTES];
type Fingerprints = HashMap<(u8, u16), HashSet<Tag>>;

struct Lock<'a>(&'a File);

impl<'a> Lock<'a> {
    fn take(file: &'a File, mode: libc::c_int) -> io::Result<Self> {
        loop {
            // SAFETY: the borrowed file keeps the descriptor live through the call.
            if unsafe { libc::flock(file.as_raw_fd(), mode) } == 0 {
                return Ok(Self(file));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        // SAFETY: the lock guard borrows the same live descriptor.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct Scope {
    path: PathBuf,
    dir: File,
    registry: File,
    auth: Blake2sMac256,
    device: Tag,
}

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}

fn private(metadata: &std::fs::Metadata, mode: u32) -> bool {
    // SAFETY: geteuid takes no arguments.
    let uid = unsafe { libc::geteuid() };
    metadata.uid() == uid && metadata.mode() & 0o777 == mode
}

fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

fn registry_options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).append(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    opts
}

/// Publish a complete authenticated header. A competing opener either opens
/// this inode or wins publication with its own complete header.
fn open_registry(path: &Path, header: &Tag) -> io::Result<File> {
    let dest = path.join(FILE_NAME);
    match registry_options().open(&dest) {
        Ok(file) => return Ok(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // The anchor distinguishes a lost registry from a fresh run. Reopen in
    // case another invocation just published both names.
    match std::fs::symlink_metadata(path.join(ANCHOR_NAME)) {
        Ok(_) => return registry_options().open(dest),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let (tmp, mut file) = loop {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let tmp = path.join(format!(".fingerprints-{}-{serial}", std::process::id()));
        match registry_options().create_new(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(header)?;
        file.sync_data()?;
        match std::fs::hard_link(&tmp, &dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result?;
    registry_options().open(dest)
}

fn mac(auth: &Blake2sMac256, domain: &[u8], message: &[u8]) -> Blake2sMac256 {
    let mut mac = auth.clone();
    Mac::update(&mut mac, domain);
    Mac::update(&mut mac, message);
    mac
}

impl Scope {
    fn open(path: PathBuf, key: &[u8; 32], url: &str, udid: &str) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid());
        }
        // Normalize trailing separators and "." so O_NOFOLLOW applies to the directory.
        let path: PathBuf = path.components().collect();
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)?;
        let meta = dir.metadata()?;
        if !private(&meta, 0o700) || !same_file(&meta, &std::fs::symlink_metadata(&path)?) {
            return Err(invalid());
        }

        let mut auth = <Blake2sMac256 as KeyInit>::new_from_slice(key).map_err(|_| invalid())?;
        Mac::update(&mut auth, b"turbotap scoped redaction v1\0");
        Mac::update(&mut auth, &meta.dev().to_be_bytes());
        Mac::update(&mut auth, &meta.ino().to_be_bytes());
        let header = mac(&auth, b"header\0", b"").finalize().into_bytes().into();
        let registry = open_registry(&path, &header)?;
        let registered = registry.metadata()?;
        if !registered.is_file() || !private(&registered, 0o600) {
            return Err(invalid());
        }
        match std::fs::hard_link(path.join(FILE_NAME), path.join(ANCHOR_NAME)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let mut id = <Blake2s256 as Digest>::new();
        if udid.is_empty() {
            let (host, port) = crate::wda::parse_host_port(url);
            Digest::update(&mut id, b"endpoint:");
            Digest::update(&mut id, host.to_ascii_lowercase().as_bytes());
            Digest::update(&mut id, port.to_be_bytes());
        } else {
            Digest::update(&mut id, b"udid:");
            Digest::update(&mut id, udid.to_ascii_lowercase().as_bytes());
        }
        let scope = Self { path, dir, registry, auth, device: Digest::finalize(id).into() };
        scope.read()?;
        Ok(scope)
    }

    fn check_files(&self) -> io::Result<()> {
        let dir = std::fs::symlink_metadata(&self.path)?;
        let registry = std::fs::symlink_metadata(self.path.join(FILE_NAME))?;
        let anchor = std::fs::symlink_metadata(self.path.join(ANCHOR_NAME))?;
        if !dir.is_dir() || !private(&dir, 0o700) || !same_file(&dir, &self.dir.metadata()?)
            || !registry.is_file() || !private(&registry, 0o600) || !same_file(&registry, &self.registry.metadata()?)
            || !same_file(&registry, &anchor)
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn read(&self) -> io::Result<Fingerprints> {
        let _lock = Lock::take(&self.registry, libc::LOCK_SH)?;
        self.read_locked().map(|(fingerprints, _)| fingerprints)
    }

    /// Registry format: an authenticated header and length-prefixed credential
    /// batches. Each batch authenticates its device, lengths and fingerprints.
    fn read_locked(&self) -> io::Result<(Fingerprints, usize)> {
        self.check_files()?;
        let mut file = &self.registry;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(MAX_REGISTRY_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if !(TAG_BYTES..=MAX_REGISTRY_BYTES).contains(&bytes.len()) {
            return Err(invalid());
        }
        mac(&self.auth, b"header\0", b"").verify_slice(&bytes[..TAG_BYTES]).map_err(|_| invalid())?;

        let mut fingerprints = Fingerprints::new();
        let mut tail = &bytes[TAG_BYTES..];
        while !tail.is_empty() {
            let count = tail.get(..2).ok_or_else(invalid)?;
            let count = u16::from_be_bytes([count[0], count[1]]) as usize;
            if count == 0 {
                return Err(invalid());
            }
            let data_end = 2 + TAG_BYTES + count * RECORD_BYTES;
            let frame_end = data_end + TAG_BYTES;
            let frame = tail.get(..frame_end).ok_or_else(invalid)?;
            mac(&self.auth, b"batch\0", &frame[..data_end]).verify_slice(&frame[data_end..]).map_err(|_| invalid())?;
            if frame[2..2 + TAG_BYTES] == self.device[..] {
                for record in frame[2 + TAG_BYTES..data_end].chunks_exact(RECORD_BYTES) {
                    let kind = record[0];
                    let length = u16::from_be_bytes([record[1], record[2]]);
                    if !matches!(kind, TEXT | DIGITS) || length == 0 || length as usize > MAX_CHARS {
                        return Err(invalid());
                    }
                    let tag: Tag = record[3..].try_into().map_err(|_| invalid())?;
                    fingerprints.entry((kind, length)).or_default().insert(tag);
                }
            }
            tail = &tail[frame_end..];
        }
        self.check_files()?;
        Ok((fingerprints, bytes.len()))
    }

    fn fingerprint(&self, kind: u8, chars: &[char]) -> Tag {
        let mut mac = mac(&self.auth, b"match\0", &self.device);
        Mac::update(&mut mac, &[kind]);
        let mut utf8 = [0; 4];
        for ch in chars {
            Mac::update(&mut mac, ch.encode_utf8(&mut utf8).as_bytes());
        }
        mac.finalize().into_bytes().into()
    }

    fn register(&self, secrets: &[&str]) -> io::Result<()> {
        let _lock = Lock::take(&self.registry, libc::LOCK_EX)?;
        let (mut seen, size) = self.read_locked()?;
        let mut records = Vec::new();
        let mut add = |kind, chars: &[char]| {
            let length = chars.len() as u16;
            let tag = self.fingerprint(kind, chars);
            if seen.entry((kind, length)).or_default().insert(tag) {
                records.push(kind);
                records.extend_from_slice(&length.to_be_bytes());
                records.extend_from_slice(&tag);
            }
        };
        for secret in secrets.iter().filter(|s| !s.is_empty()) {
            let chars = normalized(secret);
            if chars.is_empty() || chars.len() > MAX_CHARS {
                return Err(invalid());
            }
            add(TEXT, &chars);
            for fragment in chars.windows(5.min(chars.len())) {
                add(TEXT, fragment);
            }
            // Diagnostics sometimes include a quoted, escaped representation.
            let quoted = crate::json::py_repr_str(secret);
            let escaped = normalized(&quoted[1..quoted.len() - 1]);
            if escaped != chars && escaped.len() <= MAX_CHARS {
                add(TEXT, &escaped);
            }
            if chars.iter().all(|ch| ch.is_ascii_digit() || separator(*ch)) {
                let digits: Vec<_> = chars.iter().copied().filter(char::is_ascii_digit).collect();
                if !digits.is_empty() {
                    add(DIGITS, &digits);
                    for fragment in digits.windows(4.min(digits.len())) {
                        add(DIGITS, fragment);
                    }
                }
            }
        }
        if records.is_empty() {
            return Ok(());
        }
        let count = u16::try_from(records.len() / RECORD_BYTES).map_err(|_| invalid())?;
        if size + 2 + TAG_BYTES * 2 + records.len() > MAX_REGISTRY_BYTES {
            return Err(invalid());
        }
        let mut frame = Vec::new();
        frame.extend_from_slice(&count.to_be_bytes());
        frame.extend_from_slice(&self.device);
        frame.extend_from_slice(&records);
        let auth = mac(&self.auth, b"batch\0", &frame).finalize().into_bytes();
        frame.extend_from_slice(&auth);
        let mut file = &self.registry;
        file.write_all(&frame)?;
        file.sync_data()?;
        self.check_files()
    }

    fn matches(&self, fingerprints: &Fingerprints, text: &str) -> bool {
        let chars = normalized(text);
        let mut numeric = None;
        fingerprints.iter().any(|(&(kind, length), tags)| {
            let length = usize::from(length);
            if kind == TEXT {
                chars.windows(length).any(|candidate| tags.contains(&self.fingerprint(kind, candidate)))
            } else {
                let runs = numeric.get_or_insert_with(|| digit_runs(&chars));
                runs.iter().any(|run| run.windows(length).any(|candidate| tags.contains(&self.fingerprint(kind, candidate))))
            }
        })
    }
}

fn normalized(s: &str) -> Vec<char> {
    s.nfkc().flat_map(char::to_lowercase).collect()
}

fn separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '-' | '‐' | '‑' | '–' | '—' | '.' | ',' | ':' | '/' | '\u{200b}')
}

fn digit_runs(chars: &[char]) -> Vec<Vec<char>> {
    chars.split(|&ch| !ch.is_ascii_digit() && !separator(ch))
        .map(|run| run.iter().copied().filter(char::is_ascii_digit).collect())
        .collect()
}

thread_local! {
    static CURRENT: RefCell<Option<Scope>> = const { RefCell::new(None) };
    static STATE_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Device dispatch and final error output share one scope, including under the
/// daemon, which installs the caller's environment before dispatch.
pub(crate) struct Invocation(Option<Scope>, bool);

impl Invocation {
    pub(crate) fn begin() -> Self {
        Self(CURRENT.with(|scope| scope.take()), STATE_FAILED.with(|failed| failed.replace(false)))
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        CURRENT.with(|scope| scope.replace(self.0.take()));
        STATE_FAILED.with(|failed| failed.set(self.1));
    }
}

fn read_key(key: &str) -> Option<[u8; 32]> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 32];
    for (i, dst) in bytes.iter_mut().enumerate() {
        *dst = u8::from_str_radix(&key[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(bytes)
}

pub(crate) fn configure(url: &str, udid: &str) -> Result<()> {
    let path = std::env::var_os(DIR_ENV);
    let key = std::env::var_os(KEY_ENV);
    if path.is_none() && key.is_none() {
        return Ok(());
    }
    let path = path.ok_or_else(|| Error::runtime("TURBOTAP_REDACTION_DIR is required for scoped credential redaction"))?;
    let key = key.as_deref().and_then(|s| s.to_str()).and_then(read_key)
        .ok_or_else(|| Error::runtime("TURBOTAP_REDACTION_KEY must be a fresh random 32-byte key encoded as 64 hex digits"))?;
    let scope = Scope::open(path.into(), &key, url, udid).map_err(|_| Error::runtime(STATE_ERROR))?;
    CURRENT.with(|current| current.replace(Some(scope)));
    Ok(())
}

pub(crate) fn before_input(secrets: &[&str]) -> Result<()> {
    CURRENT.with(|scope| {
        if let Some(scope) = scope.borrow().as_ref() {
            if failed() || scope.register(secrets).is_err() {
                return Err(Error::flow("credential input refused because fingerprints could not be registered"));
            }
        }
        Ok(())
    })
}

pub(crate) fn failed() -> bool {
    STATE_FAILED.with(|failed| failed.get())
}

fn with_matcher<T>(f: impl FnOnce(Option<&dyn Fn(&str) -> bool>) -> T) -> io::Result<T> {
    CURRENT.with(|scope| {
        let guard = scope.borrow();
        let Some(scope) = guard.as_ref() else { return Ok(f(None)) };
        if failed() {
            return Err(invalid());
        }
        let fingerprints = scope.read().inspect_err(|_| STATE_FAILED.with(|failed| failed.set(true)))?;
        if fingerprints.is_empty() {
            return Ok(f(None));
        }
        Ok(f(Some(&|text| scope.matches(&fingerprints, text))))
    })
}

pub(crate) fn before_image_export() -> Result<()> {
    if failed() {
        return Err(Error::runtime(STATE_ERROR));
    }
    let contaminated = CURRENT.with(|scope| {
        scope.borrow().as_ref().map(Scope::read).transpose()
    }).map_err(|_| Error::runtime(STATE_ERROR))?.is_some_and(|entries| !entries.is_empty());
    if contaminated {
        return Err(Error::runtime("image export refused because the screen may contain a credential"));
    }
    Ok(())
}

fn scrub_value(value: &mut Value, matches: &dyn Fn(&str) -> bool) -> bool {
    match value {
        Value::String(s) if matches(s) => {
            *s = MASK.into();
            true
        }
        Value::Array(a) => a.iter_mut().fold(false, |changed, v| scrub_value(v, matches) || changed),
        Value::Object(o) => o.values_mut().fold(false, |changed, v| scrub_value(v, matches) || changed),
        _ => false,
    }
}

pub(crate) fn receipt(build: impl FnOnce(&dyn Fn(&str) -> String) -> Value) -> io::Result<Value> {
    with_matcher(|matches| {
        let Some(matches) = matches else { return build(&str::to_string) };
        let mut value = build(&|s| if matches(s) { MASK.into() } else { s.to_string() });
        scrub_value(&mut value, matches);
        value
    })
}

pub(crate) fn output(s: &str) -> Cow<'_, str> {
    match with_matcher(|matches| {
        let Some(matches) = matches else { return Cow::Borrowed(s) };
        if let Ok(mut value @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(s) {
            if !scrub_value(&mut value, matches) {
                return Cow::Borrowed(s);
            }
            if let Some(object) = value.as_object_mut() {
                object.insert("redacted".into(), Value::Bool(true));
            }
            return Cow::Owned(format!("{value}\n"));
        }
        if !matches(s) {
            return Cow::Borrowed(s);
        }
        // Match across whitespace (including a newline in a numeric code) first,
        // then preserve unrelated lines when the hit is confined to one line.
        let mut changed = false;
        let safe: String = s.split_inclusive('\n').map(|line| {
            if matches(line) {
                changed = true;
                if line.ends_with('\n') { "<redacted>\n" } else { MASK }
            } else {
                line
            }
        }).collect();
        if !changed || matches(&safe) {
            return Cow::Borrowed("<redacted>\n");
        }
        Cow::Owned(safe)
    }) {
        Ok(output) => output,
        Err(_) => Cow::Borrowed("turbotap: credential redaction state is unavailable or invalid\n"),
    }
}
