//! Readiness, version and patch inventory for other native mods in the same process
//! (ThreeKingdoms-Coop asked for it: a successful LoadLibrary says nothing about whether our
//! asynchronous bootstrap succeeded, and comparing writes against writes / writes against anchors
//! catches collisions like the 0x1457760 one before anyone plays).
//!
//! C exports (GetProcAddress on script_extender.dll):
//!   `u32 se_status()`            0 booting, 1 ready, 2 refused (fingerprint / anchors), 3 not the game
//!   `const char* se_version()`   "<dll version>.<sync tag>", static, NUL-terminated
//! File: `se_inventory.json` next to the DLL, rewritten when the bootstrap ends and again once the
//! natives have been registered into the first Lua state.

use crate::log;
use core::ffi::c_char;
use std::ffi::CString;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

pub const BOOTING: u32 = 0;
pub const READY: u32 = 1;
pub const REFUSED: u32 = 2;
pub const NOT_GAME: u32 = 3;

static STATE: AtomicU32 = AtomicU32::new(BOOTING);
static VERSION: OnceLock<CString> = OnceLock::new();

struct Anchor {
    name: &'static str,
    rva: usize,
    got: Option<[u8; 8]>,
    ok: bool,
}

struct Patch {
    name: &'static str,
    cfg_key: &'static str,
    rva: usize,
    before: [u8; 16],
    after: [u8; 16],
}

#[derive(Default)]
struct Inventory {
    base: usize,
    fingerprint: Option<(u32, usize)>,
    store: &'static str,
    anchors: Vec<Anchor>,
    vtables: Vec<Anchor>,
    patches: Vec<Patch>,
    refused: String,
}

static INV: Mutex<Option<Inventory>> = Mutex::new(None);

fn with_inv<R>(f: impl FnOnce(&mut Inventory) -> R) -> R {
    let mut g = INV.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(Inventory::default))
}

pub fn state() -> u32 {
    STATE.load(Ordering::Acquire)
}

pub fn set_state(s: u32) {
    STATE.store(s, Ordering::Release);
}

/// "<dll version>.<sync tag>", the same pair the build string carries.
pub fn version_string() -> &'static str {
    VERSION
        .get_or_init(|| CString::new(format!("{}.{}", env!("CARGO_PKG_VERSION"), crate::build::sync_tag())).unwrap_or_default())
        .to_str()
        .unwrap_or("")
}

#[no_mangle]
pub extern "C" fn se_status() -> u32 {
    state()
}

#[export_name = "se_version"]
pub extern "C" fn export_se_version() -> *const c_char {
    let _ = version_string();
    VERSION.get().map(|c| c.as_ptr()).unwrap_or(c"".as_ptr())
}

pub fn record_image(base: usize, timestamp: u32, size_of_image: usize, store: &'static str) {
    with_inv(|i| {
        i.base = base;
        i.fingerprint = Some((timestamp, size_of_image));
        i.store = store;
    });
}

pub fn record_anchor(name: &'static str, rva: usize, got: Option<[u8; 8]>, ok: bool) {
    with_inv(|i| i.anchors.push(Anchor { name, rva, got, ok }));
}

pub fn record_vtable(name: &'static str, rva: usize, got: Option<[u8; 8]>, ok: bool) {
    with_inv(|i| i.vtables.push(Anchor { name, rva, got, ok }));
}

pub fn record_patch(name: &'static str, cfg_key: &'static str, addr: usize, before: [u8; 16], after: [u8; 16]) {
    with_inv(|i| {
        let rva = addr.wrapping_sub(i.base);
        i.patches.push(Patch { name, cfg_key, rva, before, after });
    });
}

pub fn patch_count() -> usize {
    with_inv(|i| i.patches.len())
}

/// Bootstrap gave up: remember why, publish REFUSED (or NOT_GAME) and write the file.
pub fn refuse(state: u32, reason: &str) {
    with_inv(|i| i.refused = reason.to_string());
    set_state(state);
    write_inventory();
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => { let _ = write!(o, "\\u{:04x}", c as u32); }
            c => o.push(c),
        }
    }
    o
}

/// Length of the patched span: last differing byte of the 16 read around `enable()`.
fn patched_len(p: &Patch) -> usize {
    (0..16).rev().find(|&k| p.before[k] != p.after[k]).map(|k| k + 1).unwrap_or(0)
}

fn render() -> String {
    let state = match state() { BOOTING => "booting", READY => "ready", REFUSED => "refused", NOT_GAME => "not_game", _ => "?" };
    let natives = crate::lua::native_names();
    with_inv(|i| {
        let mut s = String::new();
        let _ = writeln!(s, "{{");
        let _ = writeln!(s, "  \"schema\": 1,");
        let _ = writeln!(s, "  \"module\": \"script_extender.dll\",");
        let _ = writeln!(s, "  \"version\": \"{}\",", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(s, "  \"sync\": \"{}\",", crate::build::sync_tag());
        let _ = writeln!(s, "  \"state\": \"{state}\",");
        let _ = writeln!(s, "  \"refused_reason\": \"{}\",", esc(&i.refused));
        match i.fingerprint {
            Some((ts, soi)) => { let _ = writeln!(s, "  \"exe\": {{ \"timestamp\": \"0x{ts:x}\", \"size_of_image\": \"0x{soi:x}\", \"store\": \"{}\" }},", i.store); }
            None => { let _ = writeln!(s, "  \"exe\": null,"); }
        }
        let anchor_list = |list: &Vec<Anchor>| -> String {
            list.iter()
                .map(|a| format!("    {{ \"name\": \"{}\", \"rva\": \"0x{:x}\", \"len\": 8, \"ok\": {}, \"bytes\": \"{}\" }}", a.name, a.rva, a.ok, a.got.map(|b| hex(&b)).unwrap_or_else(|| "unreadable".into())))
                .collect::<Vec<_>>()
                .join(",\n")
        };
        let _ = writeln!(s, "  \"anchors\": [\n{}\n  ],", anchor_list(&i.anchors));
        let _ = writeln!(s, "  \"vtables\": [\n{}\n  ],", anchor_list(&i.vtables));
        let patches = i.patches.iter()
            .map(|p| format!("    {{ \"name\": \"{}\", \"cfg_key\": \"{}\", \"rva\": \"0x{:x}\", \"len\": {}, \"before\": \"{}\", \"after\": \"{}\" }}", p.name, p.cfg_key, p.rva, patched_len(p), hex(&p.before), hex(&p.after)))
            .collect::<Vec<_>>()
            .join(",\n");
        let _ = writeln!(s, "  \"patches\": [\n{patches}\n  ],");
        let names = natives.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
        let _ = writeln!(s, "  \"natives\": [{names}]");
        let _ = writeln!(s, "}}");
        s
    })
}

pub fn write_inventory() {
    let Some(dir) = crate::process::self_dir() else { return };
    let path = dir.join("se_inventory.json");
    match std::fs::write(&path, render()) {
        Ok(()) => log!("inventory written: {}", path.display()),
        Err(e) => log!("inventory not written ({}): {e}", path.display()),
    }
}
