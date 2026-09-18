//! Main-menu build number (build 1.7.2.0).
//!
//! CcoGameCore handlers (registration FUN_1402bbf50): `BuildNumber` returns the CA::String at
//! GameCore+0x90, `BuildNumberShort` the one at GameCore+0xa0, `IsBuildModified` the byte at
//! GameCore+0xea. GameCore is the 0x168-byte object built by FUN_14030dee0 (constructor
//! FUN_142617a70: +0x90 = long build String, +0xa0 = the short string composed by FUN_1402e6740
//! "v%d.%d.%d  Build %d.%d%S" with " (modded)" when mods are loaded, +0xea = modded flag) and
//! stored at `*(*(DAT_143c53a28) + 0x960)`. (FUN_1402ebb60() is the engine settings object,
//! not GameCore: 0.16 wrote into it by mistake.) The UI reads the strings back through the CCO,
//! so replacing the String objects changes what the menu shows.
//!
//! se_build_info_get() -> build:string, short:string, modified:boolean
//! se_build_info_set(build, short, modified) -> ok, msg   ("" or nil keeps a field; modified nil keeps)
//! Also applied at injection from `<dll dir>\script_extender.cfg` (see apply_config).

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

const OFF_BUILD: usize = 0x90;
const OFF_BUILD_SHORT: usize = 0xa0;
const OFF_MODIFIED: usize = 0xea;
/// Global pointer to the application object (DAT_143c53a28); GameCore = *(app + 0x960).
const APP_GLOBAL_RVA: usize = 0x3c53a28;
const OFF_APP_GAME_CORE: usize = 0x960;

struct Engine {
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
    string_assign: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
            string_assign: core::mem::transmute(t.get("string_assign")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_build_info_get", se_build_info_get);
    lua::set_global_fn(l, "se_build_info_set", se_build_info_set);
}

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool {
    p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0
}
unsafe fn rq(p: usize) -> usize {
    if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 }
}
unsafe fn rd(p: usize) -> u32 {
    if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 }
}

unsafe fn ca_string(sp: usize) -> String {
    if !readable(sp, 16) {
        return String::new();
    }
    let len = rd(sp) as usize;
    let ptr = rq(sp + 8);
    if len == 0 || len > 256 || !readable(ptr, len) {
        return String::new();
    }
    String::from_utf8_lossy(core::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
}

struct View {
    core: usize,
    build: String,
    short: String,
    modified: bool,
}

fn printable(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

unsafe fn view(_e: &Engine) -> Result<View, String> {
    let (base, _) = crate::process::main_module();
    let app = rq(base + APP_GLOBAL_RVA);
    let g = rq(app + OFF_APP_GAME_CORE);
    if app == 0 || g == 0 || !readable(g, 0x168) {
        return Err(format!("GameCore object not available (app={:#x}, core={:#x}; injected too early?)", app, g));
    }
    let build = ca_string(g + OFF_BUILD);
    let short = ca_string(g + OFF_BUILD_SHORT);
    // Both fields must look like the engine's own text before anything is written.
    if !printable(&build) || !printable(&short) {
        return Err(format!("GameCore {:#x} does not hold plausible build strings ('{}' / '{}'); refusing", g, build, short));
    }
    Ok(View { core: g, build, short, modified: rd(g + OFF_MODIFIED) & 0xff != 0 })
}

unsafe fn assign(e: &Engine, dst: usize, text: &str) {
    let mut c = text.as_bytes().to_vec();
    c.push(0);
    let mut buf = [0u8; 64];
    (e.string_from_cstr)(buf.as_mut_ptr() as *mut c_void, c.as_ptr() as *const c_char);
    (e.string_assign)(dst as *mut c_void, buf.as_ptr() as *const c_void);
    (e.string_dtor)(buf.as_mut_ptr() as *mut c_void);
}

/// Apply new values; empty strings keep the current text, `modified` None keeps the flag.
pub unsafe fn apply(build: &str, short: &str, modified: Option<bool>) -> Result<String, String> {
    let e = ENGINE.get().ok_or("engine table missing")?;
    let before = view(e)?;
    if build.len() > 200 || short.len() > 200 {
        return Err("build strings must be at most 200 characters".into());
    }
    if !build.is_empty() {
        assign(e, before.core + OFF_BUILD, build);
    }
    if !short.is_empty() {
        assign(e, before.core + OFF_BUILD_SHORT, short);
    }
    if let Some(m) = modified {
        core::ptr::write_unaligned((before.core + OFF_MODIFIED) as *mut u8, m as u8);
    }
    let after = view(e)?;
    let msg = format!("build '{}' -> '{}', short '{}' -> '{}', modified {} -> {}", before.build, after.build, before.short, after.short, before.modified, after.modified);
    log!("build info: {msg}");
    Ok(msg)
}

/// `<dll dir>\script_extender.cfg`, one `key=value` per line (`#` comments):
///   build_number=..., build_number_short=..., build_modified=0|1
pub fn apply_config() {
    let Some(dir) = crate::process::self_dir() else { return };
    let path = dir.join("script_extender.cfg");
    let Ok(text) = std::fs::read_to_string(&path) else {
        log!("no config at {} (build number left alone)", path.display());
        return;
    };
    let (mut build, mut short, mut modified) = (String::new(), String::new(), None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            match k {
                "build_number" => build = v.to_string(),
                "build_number_short" => short = v.to_string(),
                "build_modified" => modified = Some(v == "1" || v.eq_ignore_ascii_case("true")),
                _ => log!("config: unknown key '{k}' ignored"),
            }
        }
    }
    if build.is_empty() && short.is_empty() && modified.is_none() {
        log!("config at {} sets nothing", path.display());
        return;
    }
    match unsafe { apply(&build, &short, modified) } {
        Ok(m) => log!("config applied: {m}"),
        Err(e) => log!("config not applied: {e}"),
    }
}

unsafe extern "C" fn se_build_info_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let result: Result<View, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        view(e)
    })();
    match result {
        Ok(v) => {
            lua::push_str(l, &v.build);
            lua::push_str(l, &v.short);
            (api.pushboolean)(l, v.modified as c_int);
            3
        }
        Err(e) => {
            log!("se_build_info_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_build_info_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let build = lua::to_str(l, 1);
    let short = lua::to_str(l, 2);
    let modified = if (api.type_)(l, 3) == 1 { Some((api.toboolean)(l, 3) != 0) } else { None };
    match apply(&build, &short, modified) {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_build_info_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
