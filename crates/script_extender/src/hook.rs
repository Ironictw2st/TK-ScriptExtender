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

unsafe fn ensure_registered(l: *mut LuaState) {
    let lu = l as usize;
    let mut reg = match REGISTERED.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if reg.contains(&lu) {
        LAST_SEEN.store(lu, Ordering::Relaxed);
        return;
    }
    if reg.len() >= 16 {
        // Something is churning states; stop registering rather than grow unbounded.
        LAST_SEEN.store(lu, Ordering::Relaxed);
        return;
    }
    reg.push(lu);
    drop(reg);
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
}

const SE_API_SRC: &str = include_str!("../lua/se_api.lua");

pub fn install(t: &Table) {
    let target: GetTop = unsafe { core::mem::transmute(t.get("lua_gettop")) };
    // SAFETY: the target's first bytes were anchor-verified; lua_gettop has no RIP-relative
    // prologue, so the relocated trampoline is sound.
    unsafe {
        match GenericDetour::new(target, detour) {
            Ok(d) => {
                // lua_gettop is 13 bytes long and hot: freeze the other threads and make sure
                // none of them is executing inside it while the jump is written.
                let enabled = crate::freeze::with_threads_frozen(target as usize, 16, || d.enable());
                if let Err(e) = enabled {
                    log!("failed to enable lua_gettop hook: {e}");
                    return;
                }
                let _ = HOOK.set(d);
                crate::crash::hook_installed("lua_gettop", "-", target as usize);
                log!("lua_gettop hook installed");
            }
            Err(e) => log!("failed to create lua_gettop hook: {e}"),
        }
    }
}

#[allow(dead_code)]
pub fn lua_api() -> Option<&'static lua::Api> {
    lua::api()
}
