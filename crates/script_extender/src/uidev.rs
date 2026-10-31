//! UI dev loader: open a .twui.xml layout and its Lua straight from disk, for testing without
//! building a pack (se.ui.open / reload / close in se_api.lua; notes/ui_dev_loader.md).
//!
//! The engine creates a layout through FUN_140610da0(ui, path, name, parent, ..): it appends
//! ".twui.xml", looks the path up in a parsed-layout cache (0x143c01780) and only on a miss reads
//! the file through UTILITYDLL::vfs(). A path that no pack holds is found as a loose file in one
//! of the VFS search roots (fexist_vfs walks them). So a layout is handed over by copying it to
//! `data\ui\se_dev\<n>_<stem>.twui.xml`: a new name for every load, so the cache never returns
//! the previous text. No engine memory is read or written here.
//!
//! Natives (internal; the public face is se.ui in se_api.lua). Paths are disk paths; only
//! `*.twui.xml` and `*.lua` files are accepted.
//!   se_uidev_place(twui_path) -> virtual_path | nil, message   copy into data\ui\se_dev\
//!   se_uidev_stamp(path)      -> "mtime:size" | nil, message   change detection for the watcher
//!   se_uidev_load_lua(path)   -> function | nil, message       luaL_loadbuffer of the file now
//! The copies are deleted when the DLL loads (left over from an earlier session); cfg
//! `ui_dev_loader=1` turns the natives on (off by default). Multiplayer is refused in se_api.lua.

use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Where the copies go, relative to the game root (the process working directory).
const DEV_DIR: &str = "data/ui/se_dev";
/// Virtual folder the engine is asked for (the VFS adds ".twui.xml").
const VIRTUAL_DIR: &str = "ui/se_dev";
const MAX_FILE: u64 = 16 * 1024 * 1024;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Opt-in: only `ui_dev_loader=1` in the cfg turns the loader on (it writes files under data\).
fn enabled() -> bool {
    crate::build::config_value("ui_dev_loader").as_deref().map(str::trim) == Some("1")
}

/// The game root: the folder of the running exe (the working directory is not guaranteed when
/// the game was started by a launcher).
fn game_root() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn dev_dir() -> Option<PathBuf> {
    game_root().map(|r| r.join(DEV_DIR))
}

/// Remove copies left by an earlier session. Only files this module writes (`<n>_*.twui.xml`)
/// are deleted; the folder itself is kept.
pub fn install() {
    if !enabled() {
        log!("ui dev loader off (set ui_dev_loader=1 to use se.ui.open)");
        return;
    }
    let Some(dir) = dev_dir() else { return };
    let mut removed = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if is_our_copy(&name) && std::fs::remove_file(e.path()).is_ok() { removed += 1; }
        }
    }
    if removed > 0 { log!("ui dev loader: removed {removed} copies from {}", dir.display()); }
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_uidev_place", se_uidev_place);
    lua::set_global_fn(l, "se_uidev_stamp", se_uidev_stamp);
    lua::set_global_fn(l, "se_uidev_load_lua", se_uidev_load_lua);
}

unsafe fn fail(l: *mut LuaState, msg: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    (api.pushnil)(l);
    lua::push_str(l, msg);
    2
}

#[derive(PartialEq)]
enum Kind { Twui, Lua }

/// The path argument, checked: a file name ending in .twui.xml or .lua.
unsafe fn path_arg(l: *mut LuaState, idx: c_int) -> Result<(PathBuf, Kind), String> {
    if !enabled() { return Err("ui dev loader is off: add ui_dev_loader=1 to script_extender.cfg".into()); }
    let s = lua::to_str(l, idx);
    if s.is_empty() { return Err("file path expected".into()); }
    let lower = s.to_ascii_lowercase();
    let kind = if lower.ends_with(".twui.xml") { Kind::Twui } else if lower.ends_with(".lua") { Kind::Lua } else {
        return Err(format!("{s}: only .twui.xml and .lua files are accepted"));
    };
    Ok((PathBuf::from(s), kind))
}

fn read_file(p: &Path) -> Result<Vec<u8>, String> {
    let md = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if !md.is_file() { return Err(format!("{}: not a file", p.display())); }
    if md.len() > MAX_FILE { return Err(format!("{}: larger than {MAX_FILE} bytes", p.display())); }
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Lower-case file stem without ".twui.xml", reduced to [a-z0-9_] so the virtual path is plain.
fn clean_stem(p: &Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let stem = name.strip_suffix(".twui.xml").unwrap_or(&name);
    let s: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).take(48).collect();
    if s.is_empty() { "layout".into() } else { s }
}

/// se_uidev_place(twui_path) -> virtual_path | nil, message
unsafe extern "C" fn se_uidev_place(l: *mut LuaState) -> c_int {
    let (src, kind) = match path_arg(l, 1) { Ok(v) => v, Err(e) => return fail(l, &e) };
    if kind != Kind::Twui { return fail(l, "a .twui.xml file is expected"); }
    let bytes = match read_file(&src) { Ok(b) => b, Err(e) => return fail(l, &e) };
    let Some(dir) = dev_dir() else { return fail(l, "game folder not found") };
    if let Err(e) = std::fs::create_dir_all(&dir) { return fail(l, &format!("{}: {e}", dir.display())); }
    let n = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let name = format!("{n}_{}", clean_stem(&src));
    let dst = dir.join(format!("{name}.twui.xml"));
    if let Err(e) = std::fs::write(&dst, &bytes) { return fail(l, &format!("{}: {e}", dst.display())); }
    // the previous copy of the same layout is no longer needed once a new one exists
    if n > 1 {
        let prev_suffix = format!("_{}.twui.xml", clean_stem(&src));
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let f = e.file_name().to_string_lossy().to_ascii_lowercase();
                if f.ends_with(&prev_suffix) && e.path() != dst { let _ = std::fs::remove_file(e.path()); }
            }
        }
    }
    log!("ui dev: {} -> {} ({} bytes)", src.display(), dst.display(), bytes.len());
    lua::push_str(l, &format!("{VIRTUAL_DIR}/{name}"));
    1
}

/// se_uidev_stamp(path) -> "mtime:size" | nil, message
unsafe extern "C" fn se_uidev_stamp(l: *mut LuaState) -> c_int {
    let (p, _) = match path_arg(l, 1) { Ok(v) => v, Err(e) => return fail(l, &e) };
    let md = match std::fs::metadata(&p) { Ok(m) => m, Err(e) => return fail(l, &format!("{}: {e}", p.display())) };
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis()).unwrap_or(0);
    lua::push_str(l, &format!("{mtime}:{}", md.len()));
    1
}

/// se_uidev_load_lua(path) -> function | nil, message   (compiled, not run; chunk name "@path")
unsafe extern "C" fn se_uidev_load_lua(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let (p, kind) = match path_arg(l, 1) { Ok(v) => v, Err(e) => return fail(l, &e) };
    if kind != Kind::Lua { return fail(l, "a .lua file is expected"); }
    let src = match read_file(&p) { Ok(b) => b, Err(e) => return fail(l, &e) };
    let mut cname = format!("@{}", p.display()).into_bytes();
    cname.push(0);
    let top = (api.gettop)(l);
    let st = (api.loadbuffer)(l, src.as_ptr() as *const c_char, src.len(), cname.as_ptr() as *const c_char);
    if st != 0 {
        let msg = lua::to_str(l, -1);
        (api.settop)(l, top);
        return fail(l, &msg);
    }
    1
}

/// A file this module wrote: `<digits>_<stem>.twui.xml` (lower-cased name).
fn is_our_copy(lower_name: &str) -> bool {
    lower_name.ends_with(".twui.xml")
        && lower_name.split_once('_').is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_is_plain() {
        assert_eq!(clean_stem(Path::new(r"Z:\My Panels\Test-Panel.twui.xml")), "test_panel");
        assert_eq!(clean_stem(Path::new("a/b/panel.TWUI.XML")), "panel");
        assert_eq!(clean_stem(Path::new(".twui.xml")), "layout");
        assert_eq!(clean_stem(Path::new(&format!("{}.twui.xml", "x".repeat(80)))).len(), 48);
    }

    #[test]
    fn only_our_copies_are_cleaned() {
        assert!(is_our_copy("12_test_panel.twui.xml"));
        assert!(!is_our_copy("test_panel.twui.xml"));
        assert!(!is_our_copy("12_test_panel.lua"));
        assert!(!is_our_copy("_x.twui.xml"));
        assert!(!is_our_copy("1a_x.twui.xml"));
    }
}
