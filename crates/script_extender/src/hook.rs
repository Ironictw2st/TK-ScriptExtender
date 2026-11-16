//! Trampoline hook on `lua_gettop`. Every Lua state the game touches passes through here
//! constantly, so the first call after injection lets us register our functions into that
//! state, on the thread that owns it. The registration is idempotent per state and the
//! fast path is a single atomic compare.

use crate::addrs::Table;
use crate::lua::{self, LuaState};
use crate::log;
use core::cell::Cell;
use core::ffi::c_int;
use core::sync::atomic::{AtomicUsize, Ordering};
use retour::GenericDetour;
use std::sync::{Mutex, OnceLock};

type GetTop = unsafe extern "C" fn(*mut LuaState) -> c_int;

static HOOK: OnceLock<GenericDetour<GetTop>> = OnceLock::new();
static LAST_SEEN: AtomicUsize = AtomicUsize::new(0);
static REGISTERED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" fn detour(l: *mut LuaState) -> c_int {
    let lu = l as usize;
    if lu != 0 && LAST_SEEN.load(Ordering::Relaxed) != lu {
        IN_HOOK.with(|flag| {
            if !flag.get() {
                flag.set(true);
                ensure_registered(l);
                flag.set(false);
            }
        });
    }
    HOOK.get().expect("hook installed").call(l)
}

/// Registry key marking a VM that already has the natives. The registry is shared by every
/// coroutine of a VM and has no metatable, so a plain lua_getfield on it runs no script code.
const MARKER: &[u8] = b"__se_registered\0";
/// Registrations ever done; a backstop against a runaway state churn.
static REGISTRATIONS: AtomicUsize = AtomicUsize::new(0);
const MAX_REGISTRATIONS: usize = 64;

unsafe fn has_marker(l: *mut LuaState) -> bool {
    let Some(api) = lua::api() else { return false };
    (api.getfield)(l, lua::LUA_REGISTRYINDEX, MARKER.as_ptr() as *const _);
    let set = (api.toboolean)(l, -1) != 0;
    (api.settop)(l, -2);
    set
}

unsafe fn ensure_registered(l: *mut LuaState) {
    let lu = l as usize;
    // 0.48.x and older kept a list of state pointers and stopped at 16. Two faults with that
    // (creator spike S1): a state freed on a frontend <-> campaign cycle can come back at the
    // same address (the list then skips the new VM), and every coroutine is its own lua_State
    // (each one counted against the 16). The registry marker answers "does this VM have the
    // natives" exactly; the pointer list is only a cache of states already checked.
    let cached = match REGISTERED.lock() { Ok(g) => g.contains(&lu), Err(p) => p.into_inner().contains(&lu) };
    let remember = || {
        let mut reg = match REGISTERED.lock() { Ok(g) => g, Err(p) => p.into_inner() };
        if !reg.contains(&lu) {
            if reg.len() >= 32 { reg.remove(0); }
            reg.push(lu);
        }
    };
    if has_marker(l) {
        if !cached { remember(); }
        LAST_SEEN.store(lu, Ordering::Relaxed);
        return;
    }
    if REGISTRATIONS.fetch_add(1, Ordering::Relaxed) >= MAX_REGISTRATIONS {
        LAST_SEEN.store(lu, Ordering::Relaxed);
        return;
    }
    if cached {
        log!("lua_State {:p} was seen before but has no natives: a new VM at a reused address", l);
    }
    remember();
    if let Some(api) = lua::api() {
        (api.pushboolean)(l, 1);
        (api.setfield)(l, lua::LUA_REGISTRYINDEX, MARKER.as_ptr() as *const _);
    }
    let _g = crate::crash::enter("lua_gettop_register");
    crate::crash::note_lua_thread();
    crate::pool::register(l);
    LAST_SEEN.store(lu, Ordering::Relaxed);
    log!("registered se_* functions into lua_State {:p}", l);
    // The Lua face of the API (se.query / se.modify) rides along in the DLL so no pack or
    // script file is needed; a console `dofile` of a newer copy simply redefines it.
    match lua::run_chunk(l, "se_api", SE_API_SRC) {
        Ok(()) => log!("se_api.lua loaded into lua_State {:p}", l),
        Err(e) => log!("se_api.lua failed in lua_State {:p}: {e}", l),
    }
    // the first state is where the natives list becomes known: refresh the inventory once
    static NATIVES_WRITTEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !NATIVES_WRITTEN.swap(true, Ordering::Relaxed) {
        crate::status::write_inventory();
    }
}

const SE_API_SRC: &str = include_str!("../lua/se_api.lua");

/// false when the hook could not be installed (then no Lua state ever gets the API).
pub fn install(t: &Table) -> bool {
    let target: GetTop = unsafe { core::mem::transmute(t.get("lua_gettop")) };
    // SAFETY: the target's first bytes were anchor-verified; lua_gettop has no RIP-relative
    // prologue, so the relocated trampoline is sound.
    unsafe {
        match GenericDetour::new(target, detour) {
            Ok(d) => {
                // Published before it is enabled: lua_gettop is hot, and a thread that enters the
                // detour before HOOK is set would find no trampoline. lua_gettop is 13 bytes long:
                // enable_detour freezes the other threads and keeps them out of it while the jump
                // is written.
                let _ = HOOK.set(d);
                let Some(d) = HOOK.get() else { return false };
                if let Err(e) = crate::freeze::enable_detour("lua_gettop", "-", target as usize, d) {
                    log!("failed to enable lua_gettop hook: {e}");
                    return false;
                }
                log!("lua_gettop hook installed");
                true
            }
            Err(e) => {
                log!("failed to create lua_gettop hook: {e}");
                false
            }
        }
    }
}

#[allow(dead_code)]
pub fn lua_api() -> Option<&'static lua::Api> {
    lua::api()
}
