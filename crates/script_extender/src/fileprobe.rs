//! Performance: loose-file probes into directories that do not exist.
//!
//! Before the engine reads a UI image (or any other file) from a pack it looks for a loose file
//! in every search root: FUN_1406721e0(fs, relative_path) concatenates each root (the user maps
//! folder, every subscribed workshop folder, `data`, ...) with the path and asks
//! FUN_1406a6d10(full_path) = `GetFileAttributesW(...) != INVALID`. Nothing is remembered. A
//! Process Monitor capture of zooming the campaign camera in (notes/performance.md) shows 1316
//! such probes in one 114 ms burst on the main thread: 94 images under `ui\skins\bandits\` x 14
//! roots, every one failing with PATH NOT FOUND, because none of the roots has that directory.
//!
//! The detour on FUN_1406a6d10 remembers, per directory, whether the directory exists (checked
//! once with GetFileAttributesA after a failed probe). While a directory is known to be missing,
//! a probe for a file inside it is answered "no" without a system call; everything else goes to
//! the engine's routine. An entry is trusted for `file_probe_cache_ms` (script_extender.cfg,
//! default 10000, 0 = off), so a folder created while the game runs is seen within that time.
//! Paths with non-ASCII bytes or longer than the buffer are never cached.

use crate::addrs::Table;
use crate::log;
use core::ffi::c_char;
use retour::GenericDetour;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

type Exists = unsafe extern "C" fn(*const c_char) -> u64;

static HOOK: OnceLock<GenericDetour<Exists>> = OnceLock::new();
static DIRS: Mutex<Option<HashMap<Box<[u8]>, (Instant, bool)>>> = Mutex::new(None);
static TTL_MS: AtomicU64 = AtomicU64::new(10_000);
pub static CALLS: AtomicU64 = AtomicU64::new(0);
pub static SKIPPED: AtomicU64 = AtomicU64::new(0);

const MAX_PATH_BYTES: usize = 600;
const MAX_DIRS: usize = 8192;

extern "system" {
    fn GetFileAttributesA(path: *const u8) -> u32;
}

unsafe extern "C" fn exists_detour(path: *const c_char) -> u64 {
    let Some(h) = HOOK.get() else { return 0 };
    CALLS.fetch_add(1, Ordering::Relaxed);
    if path.is_null() { return h.call(path); }
    // directory part, lower case, '/' -> '\'; NUL-terminated copy for the directory check
    let mut buf = [0u8; MAX_PATH_BYTES + 1];
    let (mut len, mut last_sep) = (0usize, None);
    loop {
        let b = *(path.add(len)) as u8;
        if b == 0 { break; }
        if len >= MAX_PATH_BYTES || b >= 0x80 { return h.call(path); }
        let c = if b == b'/' { b'\\' } else { b.to_ascii_lowercase() };
        if c == b'\\' { last_sep = Some(len); }
        buf[len] = c;
        len += 1;
    }
    let Some(dir_len) = last_sep.filter(|n| *n >= 3) else { return h.call(path) };
    let ttl = Duration::from_millis(TTL_MS.load(Ordering::Relaxed));
    let now = Instant::now();
    let known = DIRS.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&buf[..dir_len]).copied()));
    match known {
        Some((at, true)) if now.duration_since(at) < ttl => { SKIPPED.fetch_add(1, Ordering::Relaxed); return 0; }
        Some((at, false)) if now.duration_since(at) < ttl => return h.call(path),
        _ => {}
    }
    let found = h.call(path);
    // a found file proves the directory; after a miss ask once whether the directory is there
    let missing = if found & 0xff != 0 { false } else {
        buf[dir_len] = 0;
        let attrs = GetFileAttributesA(buf.as_ptr());
        buf[dir_len] = b'\\';
        attrs == u32::MAX
    };
    if let Ok(mut g) = DIRS.lock() {
        let map = g.get_or_insert_with(HashMap::new);
        if map.len() >= MAX_DIRS { map.clear(); }
        map.insert(buf[..dir_len].to_vec().into_boxed_slice(), (now, missing));
    }
    found
}

pub fn install(t: &Table) {
    let ttl = crate::build::config_value("file_probe_cache_ms").and_then(|v| v.parse::<u64>().ok()).unwrap_or(10_000).min(600_000);
    TTL_MS.store(ttl, Ordering::Relaxed);
    if ttl == 0 {
        log!("file probe cache off (file_probe_cache_ms=0)");
        return;
    }
    // SAFETY: anchor-verified prologue `push rbx / sub rsp,0x40 / mov rdx,rcx` (no RIP-relative
    // instruction in the first bytes; the call that follows is relocated by the detour crate).
    unsafe {
        let exists: Exists = core::mem::transmute(t.get("file_exists"));
        let Ok(d) = GenericDetour::new(exists, exists_detour) else {
            log!("file probe cache: could not create the detour");
            return;
        };
        // stored before it is enabled: the detour must never run without its trampoline
        let _ = HOOK.set(d);
        let Some(d) = HOOK.get() else { return };
        if crate::freeze::with_threads_frozen(exists as usize, 16, || d.enable()).is_err() {
            log!("file probe cache: could not enable the detour; off");
            return;
        }
    }
    log!("file probe cache installed (missing directories remembered for {ttl} ms)");
}
