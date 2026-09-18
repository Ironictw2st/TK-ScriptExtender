//! Faction potential (the campaign's AI handicap rating, build 1.7.2.0).
//!
//!   FACTION + 0xee0 -> FACTION_POTENTIAL object (0x20 bytes, created by FUN_141752280 from the
//!   "FACTION_POTENTIAL" save tag; null for factions without one):
//!     +0 faction, +8 ?, +0x10 base (i32), +0x14 bonus (i32), +0x18 roll (i32)
//!   value = FUN_1417bf6a0(obj) = base + bonus + roll, or 0 when the faction is human (+0xcd0).
//!   FUN_1419f5aa0(faction) rebuilds the handicap effect bundle (faction+0x2108) from the
//!   campaign_faction_potential_handicap_effects rows matching the value, then FUN_1419f65b0.
//!
//! se_faction_potential_get(q_faction) -> value, base, bonus, roll | nil, msg
//! se_faction_potential_set(q_faction, value) -> ok, msg   (rewrites base so the sum is value,
//!     then re-applies the handicap effects through the engine routine)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use crate::progression::faction_from_arg;
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

const OFF_POTENTIAL: usize = 0xee0;
const MIN_POTENTIAL: i64 = -100;
const MAX_POTENTIAL: i64 = 150;

struct Engine {
    apply: unsafe extern "C" fn(*mut c_void),
    value: unsafe extern "C" fn(*mut c_void) -> i32,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            apply: core::mem::transmute(t.get("potential_apply")),
            value: core::mem::transmute(t.get("potential_value")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_faction_potential_get", se_faction_potential_get);
    lua::set_global_fn(l, "se_faction_potential_set", se_faction_potential_set);
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
unsafe fn ri(p: usize) -> i32 {
    if readable(p, 4) { core::ptr::read_unaligned(p as *const i32) } else { 0 }
}

struct View {
    obj: usize,
    value: i32,
    base: i32,
    bonus: i32,
    roll: i32,
    human: bool,
}

unsafe fn view(e: &Engine, f: usize) -> Result<View, String> {
    let obj = rq(f + OFF_POTENTIAL);
    if obj == 0 || !readable(obj, 0x20) {
        return Err("faction has no potential object (faction+0xee0 is null)".into());
    }
    Ok(View {
        obj,
        value: (e.value)(obj as *mut c_void),
        base: ri(obj + 0x10),
        bonus: ri(obj + 0x14),
        roll: ri(obj + 0x18),
        human: ri(f + 0xcd0) & 0xff != 0,
    })
}

unsafe extern "C" fn se_faction_potential_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let result: Result<View, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction_from_arg(l, 1)?;
        view(e, f)
    })();
    match result {
        Ok(v) => {
            log!("se_faction_potential_get: obj={:#x} value={} base={} bonus={} roll={} human={}", v.obj, v.value, v.base, v.bonus, v.roll, v.human);
            (api.pushinteger)(l, v.value as isize);
            (api.pushinteger)(l, v.base as isize);
            (api.pushinteger)(l, v.bonus as isize);
            (api.pushinteger)(l, v.roll as isize);
            4
        }
        Err(e) => {
            log!("se_faction_potential_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_faction_potential_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let target = (api.tointeger)(l, 2) as i64;
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if !(MIN_POTENTIAL..=MAX_POTENTIAL).contains(&target) {
            return Err(format!("potential {target} is outside {MIN_POTENTIAL}..{MAX_POTENTIAL}"));
        }
        let f = faction_from_arg(l, 1)?;
        let before = view(e, f)?;
        if before.human {
            return Err("faction is human: the engine reports 0 potential for humans regardless of the fields".into());
        }
        let new_base = target as i32 - before.bonus - before.roll;
        core::ptr::write_unaligned((before.obj + 0x10) as *mut i32, new_base);
        (e.apply)(f as *mut c_void);
        let after = view(e, f)?;
        let msg = format!("potential {} -> {} (base {} -> {}, bonus {}, roll {}); handicap effects re-applied", before.value, after.value, before.base, after.base, after.bonus, after.roll);
        log!("se_faction_potential_set: {msg}");
        if after.value as i64 == target { Ok(msg) } else { Err(format!("value did not land: {msg}")) }
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_faction_potential_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
