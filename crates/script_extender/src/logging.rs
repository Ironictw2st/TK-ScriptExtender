//! Minimal file + OutputDebugString logger. No external deps.
//!
//! The log lands next to the DLL (falling back to the process cwd). Also mirrors
//! every line to OutputDebugStringW so DebugView/x64dbg catch it even if the file
//! can't be opened.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

pub fn init() {
    LOG.get_or_init(|| {
        let path = log_path();
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok()
            .map(Mutex::new)
    });
    line("==== script_extender log start ====");
}

pub fn line(msg: &str) {
    if let Some(Some(m)) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
    output_debug(msg);
}

/// Best-effort line for code that may be running while another thread holds the log (the crash
/// reporter): the file only, never waits, allocates nothing.
pub fn try_line(msg: &str) {
    if let Some(Some(m)) = LOG.get() {
        if let Ok(mut f) = m.try_lock() {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
}

fn output_debug(msg: &str) {
    let mut w: Vec<u16> = format!("[SE] {msg}\n").encode_utf16().collect();
    w.push(0);
    unsafe { OutputDebugStringW(w.as_ptr()) };
}

/// `<dir of our dll>\script_extender.log`, else a bare relative filename.
fn log_path() -> std::path::PathBuf {
    if let Some(dir) = crate::process::self_dir() {
        return dir.join("script_extender.log");
    }
    std::path::PathBuf::from("script_extender.log")
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logging::line(&format!($($arg)*)) };
}

extern "system" {
    fn OutputDebugStringW(s: *const u16);
}
