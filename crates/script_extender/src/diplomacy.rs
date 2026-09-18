//! Diplomatic standing and attitude events (build 1.7.2.0).
//!
//!   diplomacy manager = *(world + 0x3ba0)
//!   standing A->B: FUN_141b965e0(mgr, factionA, factionB) -> int (what diplomatic_standing_with
//!     returns; computed from CAI components, not a stored number)
//!   attitude event: FUN_141b7cf60(mgr, factionA, factionB, level) = the engine's
//!     `diplomatic_attitude_change` incident payload: level 1/2/3 = small/medium/large positive,
//!     -1/-2/-3 = small/medium/large negative (DB attitude event records 0x27..0x29 / 0x24..0x26);
//!     it appends a dated entry to the A<->B relation (FUN_141b9dea0) so it shows in the
//!     diplomacy breakdown and decays like any event-driven attitude change.
//!
//! se_attitude_get(q_a, q_b) -> standing:int | nil, msg
//! se_attitude_change(q_a, q_b, level) -> ok, msg

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use crate::progression::faction_from_arg;
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

struct Engine {
    standing: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
    change: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, i32),
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            standing: core::mem::transmute(t.get("diplomacy_standing")),
            change: core::mem::transmute(t.get("diplomacy_attitude_change")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_attitude_get", se_attitude_get);
    lua::set_global_fn(l, "se_attitude_change", se_attitude_change);
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

unsafe fn manager_of(faction: usize) -> Result<usize, String> {
    let world = rq(rq(faction + 0x288) + 0x78);
    let mgr = rq(world + 0x3ba0);
    if world == 0 || mgr == 0 || !readable(mgr + 0xa70, 8) {
        return Err(format!("diplomacy manager not found (world={:#x}, mgr={:#x})", world, mgr));
    }
    Ok(mgr)
}

unsafe extern "C" fn se_attitude_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let result: Result<i32, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let a = faction_from_arg(l, 1)?;
        let b = faction_from_arg(l, 2)?;
        let mgr = manager_of(a)?;
        Ok((e.standing)(mgr as *mut c_void, a as *mut c_void, b as *mut c_void))
    })();
    match result {
        Ok(v) => { log!("se_attitude_get: {v}"); (api.pushinteger)(l, v as isize); 1 }
        Err(e) => { log!("se_attitude_get: {e}"); (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

unsafe extern "C" fn se_attitude_change(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let level = (api.tointeger)(l, 3) as i64;
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if level == 0 || !(-3..=3).contains(&level) {
            return Err("level must be -3..-1 or 1..3 (small/medium/large negative or positive)".into());
        }
        let a = faction_from_arg(l, 1)?;
        let b = faction_from_arg(l, 2)?;
        let mgr = manager_of(a)?;
        let before = (e.standing)(mgr as *mut c_void, a as *mut c_void, b as *mut c_void);
        (e.change)(mgr as *mut c_void, a as *mut c_void, b as *mut c_void, level as i32);
        let after = (e.standing)(mgr as *mut c_void, a as *mut c_void, b as *mut c_void);
        let msg = format!("attitude event level {level}: standing {before} -> {after}");
        log!("se_attitude_change: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_attitude_change: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
