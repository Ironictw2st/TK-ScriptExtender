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
    // The version lock is enforced here, so neither the cfg nor a script can set a build string
    // without it: "" / "{game}" keep the current text, everything ends up version-locked.
    let lock = |current: &str, text: &str| -> String {
        let base = if text.is_empty() || text == "{game}" { current } else { text };
        let base = match base.rfind(" [se ") { Some(i) if base.ends_with(']') => &base[..i], _ => base };
        version_locked(base)
    };
    let (build, short) = (lock(&before.build, build), lock(&before.short, short));
    if build.len() > 250 || short.len() > 250 {
        return Err("build strings are too long".into());
    }
    assign(e, before.core + OFF_BUILD, &build);
    assign(e, before.core + OFF_BUILD_SHORT, &short);
    if let Some(m) = modified {
        core::ptr::write_unaligned((before.core + OFF_MODIFIED) as *mut u8, m as u8);
    }
    let after = view(e)?;
    let msg = format!("build '{}' -> '{}', short '{}' -> '{}', modified {} -> {}", before.build, after.build, before.short, after.short, before.modified, after.modified);
    log!("build info: {msg}");
    Ok(msg)
}

/// `script_extender.cfg` next to the DLL, else in the DLL's parent folder (mod-manager layout
/// `dll\script_extender.cfg` + `dll\<version>\script_extender.dll`). One `key=value` per line
/// (`#` comments): build_number=..., build_number_short=..., build_modified=0|1
fn config_text() -> Option<(std::path::PathBuf, String)> {
    let dir = crate::process::self_dir()?;
    let mut candidates = vec![dir.join("script_extender.cfg")];
    if let Some(parent) = dir.parent() {
        candidates.push(parent.join("script_extender.cfg"));
    }
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some((path, text));
        }
    }
    None
}

/// Value of one `key=value` line of script_extender.cfg, if the file and the key exist.
pub fn config_value(key: &str) -> Option<String> {
    let (_, text) = config_text()?;
    text.lines().filter_map(|l| l.trim().split_once('=')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().trim_matches('"').to_string())
}

/// Called from the bootstrap thread. The manager injects seconds after launch, before the game
/// has composed its build strings, so the apply is retried in a background thread until
/// GameCore holds plausible text (then done once), giving up after two minutes.
/// Version lock for multiplayer: the game build string always carries the DLL version and a
/// fingerprint of every setting that changes the simulation, so two machines only see the same
/// build when they run the same script extender with the same simulation settings.
/// `{version}` / `{sync}` in the cfg text are replaced; text without `{version}` gets
/// " [se <version>.<sync>]" appended; without any cfg text the game's own string is extended.
pub fn sync_tag() -> String {
    let mut h: u32 = 0x811c9dc5;
    let mut feed = |s: &str| for b in s.bytes() { h ^= b as u32; h = h.wrapping_mul(0x01000193); };
    feed(env!("CARGO_PKG_VERSION"));
    for key in ["autoresolve_hooks", "horde_income", "horde_income_category", "ai_recruit_cache", "recruit_perm_cache"] {
        feed(key);
        feed(&config_value(key).unwrap_or_default());
    }
    format!("{:04x}", (h ^ (h >> 16)) & 0xffff)
}

fn version_locked(text: &str) -> String {
    let (version, sync) = (env!("CARGO_PKG_VERSION"), sync_tag());
    if text.contains("{version}") {
        text.replace("{version}", version).replace("{sync}", &sync)
    } else {
        format!("{text} [se {version}.{sync}]")
    }
}

pub fn apply_config() {
    let (path, text) = config_text().unwrap_or_else(|| (std::path::PathBuf::from("(no script_extender.cfg)"), String::new()));
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
                "autoresolve_hooks" | "horde_income" | "horde_income_category" | "ui_recruit_cache_ms" | "ai_recruit_cache" | "recruit_perm_cache" | "file_probe_cache_ms" | "diag_diplomacy" => {}
                _ => log!("config: unknown key '{k}' ignored"),
            }
        }
    }
    // Always applied, cfg or not: the version lock.
    if build.is_empty() { build = "{game}".to_string(); }
    if short.is_empty() { short = "{game}".to_string(); }
    if modified.is_none() { modified = Some(true); }
    log!("version lock: se {} sync {}", env!("CARGO_PKG_VERSION"), sync_tag());
    log!("config at {}: build='{}' short='{}' modified={:?}; waiting for GameCore", path.display(), build, short, modified);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut last_err = String::new();
        loop {
            let ready = unsafe { ENGINE.get().map(|e| view(e)) };
            match ready {
                Some(Ok(_)) => {
                    // The game composes its strings during start-up; give it a moment more so
                    // our text is written after, not before, that composition.
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    match unsafe { apply(&build, &short, modified) } {
                        Ok(m) => log!("config applied: {m}"),
                        Err(e) => log!("config not applied: {e}"),
                    }
                    return;
                }
                Some(Err(e)) => last_err = e,
                None => last_err = "engine table missing".into(),
            }
            if std::time::Instant::now() >= deadline {
                log!("config not applied: GameCore never became ready ({last_err})");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
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
