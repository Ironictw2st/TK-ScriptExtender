//! Unit strength and experience primitives (build 1.7.2.0).
//!
//! Layout, from the QUERY_UNIT natives (registration FUN_14011fbd0 / FUN_14013f790):
//!   percentage_proportion_of_full_strength = FUN_1415ffac0 -> FUN_1417b3250(unit + 0xa0)
//!       = (float)*(u32*)(unit+0xac) / (float)*(u32*)(unit+0xa8) * 100
//!       so  unit+0xa8 = full-strength men, unit+0xac = current men (hp-scaled: a peasant band
//!       reads 72000 / 14400 at 20%, so treat both as arbitrary units and work in percent)
//!   experience_level = FUN_141548420 -> byte at (unit + 0xf8) + 0x18 = unit+0x110
//!
//! 0.8 writes these fields directly (the UI and the stock getters read the same fields). If a
//! live test shows stale derived state, the writers found by a hardware write-watch on
//! unit+0xac / unit+0x110 replace the direct stores.
//!
//! se_unit_strength_get(q_unit)           -> percent:number, current:int, max:int | nil, message
//! se_unit_strength_set(q_unit, percent)  -> ok, message   (0..100 of full strength, min 1 man)
//! se_unit_xp_get(q_unit)                 -> level:int | nil, message
//! se_unit_xp_set(q_unit, level)          -> ok, message   (0..9)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

const OFF_MEN_MAX: usize = 0xa8;
const OFF_MEN_CUR: usize = 0xac;
const OFF_XP_LEVEL: usize = 0x110;
const MAX_XP_LEVEL: u8 = 9;

struct Engine {
    unit_is_valid: unsafe extern "C" fn(*mut c_void) -> u8,
    unit_vtable: usize,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe { Engine { unit_is_valid: core::mem::transmute(t.get("unit_is_valid")), unit_vtable: t.get("unit_vtable") } };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_unit_strength_get", se_unit_strength_get);
    lua::set_global_fn(l, "se_unit_strength_set", se_unit_strength_set);
    lua::set_global_fn(l, "se_unit_xp_get", se_unit_xp_get);
    lua::set_global_fn(l, "se_unit_xp_set", se_unit_xp_set);
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

/// Engine UNIT from a QUERY_UNIT script object: script object = *(payload), UNIT = *(obj+0x18)
/// (verified live 2026-09-17); the candidate is accepted by its vtable (RVA 0x346e000).
pub unsafe fn unit_from_arg(l: *mut LuaState, idx: c_int) -> Result<usize, String> {
    let e = ENGINE.get().ok_or("engine table missing")?;
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    let cands = [rq(rq(p) + 0x18), rq(rq(rq(p) + 8) + 0x18), rq(rq(p + 8) + 0x18), rq(p + 0x18), rq(p), p];
    for c in cands {
        if c != 0 && readable(c, 0x120) && rq(c) == e.unit_vtable {
            return Ok(c);
        }
    }
    let dump: Vec<String> = cands.iter().map(|c| format!("{:#x}(vt={:#x})", c, if *c != 0 && readable(*c, 8) { rq(*c) } else { 0 })).collect();
    Err(format!("no candidate has the UNIT vtable {:#x}: {}", e.unit_vtable, dump.join(" ")))
}

/// Diagnostic value of the executor's validity helper (not used as a gate).
pub unsafe fn unit_valid_flag(u: usize) -> u8 {
    match ENGINE.get() { Some(e) => (e.unit_is_valid)(u as *mut c_void), None => 0 }
}

unsafe extern "C" fn se_unit_strength_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match unit_from_arg(l, 1) {
        Ok(u) => {
            let max = rd(u + OFF_MEN_MAX);
            let cur = rd(u + OFF_MEN_CUR);
            let pct = if max == 0 { 0.0 } else { cur as f64 * 100.0 / max as f64 };
            log!("se_unit_strength_get: unit={:#x} cur={cur} max={max} ({pct:.1}%)", u);
            (api.pushnumber)(l, pct as f32);
            (api.pushinteger)(l, cur as isize);
            (api.pushinteger)(l, max as isize);
            3
        }
        Err(e) => {
            log!("se_unit_strength_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_unit_strength_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let pct = (api.tonumber)(l, 2) as f64;
    let result: Result<String, String> = (|| {
        if !(0.0..=100.0).contains(&pct) {
            return Err(format!("percent {pct} is outside 0..100"));
        }
        let u = unit_from_arg(l, 1)?;
        let max = rd(u + OFF_MEN_MAX);
        let old = rd(u + OFF_MEN_CUR);
        if max == 0 || max > 100_000_000 {
            return Err(format!("implausible full-strength count {max} at unit+0xa8; refusing"));
        }
        let mut new = (max as f64 * pct / 100.0).round() as u32;
        if new == 0 { new = 1; }
        if new > max { new = max; }
        core::ptr::write_unaligned((u + OFF_MEN_CUR) as *mut u32, new);
        let msg = format!("unit {:#x}: men {old} -> {new} of {max} ({:.1}%)", u, new as f64 * 100.0 / max as f64);
        log!("se_unit_strength_set: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_unit_strength_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_unit_xp_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match unit_from_arg(l, 1) {
        Ok(u) => {
            let level = rd(u + OFF_XP_LEVEL) & 0xff;
            let dump: Vec<String> = (0..10).map(|i| format!("{:08x}", rd(u + 0xf8 + i * 4))).collect();
            log!("se_unit_xp_get: unit={:#x} level={level} words@0xf8: {}", u, dump.join(" "));
            (api.pushinteger)(l, level as isize);
            1
        }
        Err(e) => {
            log!("se_unit_xp_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_unit_xp_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let level = (api.tointeger)(l, 2);
    let result: Result<String, String> = (|| {
        if !(0..=MAX_XP_LEVEL as isize).contains(&level) {
            return Err(format!("level {level} is outside 0..{MAX_XP_LEVEL}"));
        }
        let u = unit_from_arg(l, 1)?;
        let old = rd(u + OFF_XP_LEVEL) & 0xff;
        core::ptr::write_unaligned((u + OFF_XP_LEVEL) as *mut u8, level as u8);
        let msg = format!("unit {:#x}: experience level {old} -> {level} (direct write; points untouched)", u);
        log!("se_unit_xp_set: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_unit_xp_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
