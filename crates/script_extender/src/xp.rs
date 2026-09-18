//! Character experience, rank and skill points (build 1.7.2.0), from the experience flush:
//!   FUN_1419aad80 (tick flush of the pending vector) -> FUN_141a36ff0(details, &entry)
//!   -> FUN_141a36ae0(details, xp): applied = ceil((1 + (char_effect_0x181 + faction_effect_0x181)
//!   * 0.01) * xp); DETAILS+0xcc += applied; then while xp >= thresholds[rank]: rank++ and
//!   DETAILS+0xc0 += skill_points_per_rank[rank] (fires the rank-up notification).
//!   DETAILS: +0xa8 rank table {+8 thresholds*, +0x18 points*}, +0xc0 unspent skill points,
//!            +0xc4 rank index (rank = index + 1), +0xc8 rank count, +0xcc experience.
//!   Effect values: ctx = FUN_141415530(faction + 0x18); FUN_140abcba0(ctx, id) -> int (percent).
//!
//! se_char_xp_add(q, cqi, n, raw) -> ok, msg
//!     raw = true : DETAILS+0xcc += n exactly, then FUN_141a36ae0(details, 0) to process rank-ups
//!     raw = false: FUN_141a36ff0(details, &{n, 0, 0}) = the engine's own (scaled) path, now
//! se_char_rank_get(q, cqi) -> xp, rank, max_rank, skill_points | nil, msg
//! se_skill_points_set(q, cqi, n) -> ok, msg
//! se_faction_effect_value(q_faction, effect_id) -> value:int | nil, msg   (0x181 = xp gain %)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

struct Engine {
    add_scaled: unsafe extern "C" fn(*mut c_void, u32) -> u8,
    flush_one: unsafe extern "C" fn(*mut c_void, *mut i32) -> u8,
    effect_ctx: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    // returns the value in xmm0 (an int return read 0x749f3128 garbage live)
    effect_value: unsafe extern "C" fn(*mut c_void, u16) -> f32,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            add_scaled: core::mem::transmute(t.get("xp_add_scaled")),
            flush_one: core::mem::transmute(t.get("xp_flush_one")),
            effect_ctx: core::mem::transmute(t.get("effect_ctx")),
            effect_value: core::mem::transmute(t.get("effect_value")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_char_xp_add", se_char_xp_add);
    lua::set_global_fn(l, "se_char_rank_get", se_char_rank_get);
    lua::set_global_fn(l, "se_skill_points_set", se_skill_points_set);
    lua::set_global_fn(l, "se_faction_effect_value", se_faction_effect_value);
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

unsafe fn details_of(l: *mut LuaState, cqi: u32) -> Result<usize, String> {
    let ch = crate::pool::character_from_arg(l, 1, cqi)?;
    let d = rq(rq(ch + 0x260));
    if d == 0 || !readable(d, 0xd00) {
        return Err("character has no details object".into());
    }
    Ok(d)
}

struct View {
    xp: u32,
    rank: u32,
    max_rank: u32,
    skill_points: u32,
}

unsafe fn view(d: usize) -> View {
    View { xp: rd(d + 0xcc), rank: rd(d + 0xc4) + 1, max_rank: rd(d + 0xc8), skill_points: rd(d + 0xc0) }
}

unsafe extern "C" fn se_char_rank_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match details_of(l, cqi) {
        Ok(d) => {
            let v = view(d);
            log!("se_char_rank_get: cqi {cqi} xp={} rank={}/{} skill_points={}", v.xp, v.rank, v.max_rank, v.skill_points);
            (api.pushinteger)(l, v.xp as isize);
            (api.pushinteger)(l, v.rank as isize);
            (api.pushinteger)(l, v.max_rank as isize);
            (api.pushinteger)(l, v.skill_points as isize);
            4
        }
        Err(e) => {
            log!("se_char_rank_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_char_xp_add(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let n = (api.tointeger)(l, 3) as i64;
    let raw = (api.toboolean)(l, 4) != 0;
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if n <= 0 || n > 1_000_000 {
            return Err(format!("amount {n} must be 1..1000000"));
        }
        let d = details_of(l, cqi)?;
        let before = view(d);
        let ranked;
        if raw {
            core::ptr::write_unaligned((d + 0xcc) as *mut u32, before.xp + n as u32);
            ranked = (e.add_scaled)(d as *mut c_void, 0);
        } else {
            let mut entry: [i32; 4] = [n as i32, 0, 0, 0];
            ranked = (e.flush_one)(d as *mut c_void, entry.as_mut_ptr());
        }
        let after = view(d);
        let how = if raw { "raw" } else { "engine-scaled" };
        let msg = format!("cqi {cqi}: xp {} -> {} ({how}, requested {n}), rank {} -> {}, skill points {} -> {}, ranked_up={ranked}", before.xp, after.xp, before.rank, after.rank, before.skill_points, after.skill_points);
        log!("se_char_xp_add: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_char_xp_add: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_skill_points_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let n = (api.tointeger)(l, 3) as i64;
    let result: Result<String, String> = (|| {
        if !(0..=100).contains(&n) {
            return Err(format!("skill points {n} must be 0..100"));
        }
        let d = details_of(l, cqi)?;
        let before = view(d);
        core::ptr::write_unaligned((d + 0xc0) as *mut u32, n as u32);
        let msg = format!("cqi {cqi}: unspent skill points {} -> {n}", before.skill_points);
        log!("se_skill_points_set: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_skill_points_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_faction_effect_value(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let id = (api.tointeger)(l, 2) as i64;
    let result: Result<f32, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if !(0..=0xffff).contains(&id) {
            return Err(format!("effect id {id} is outside 0..65535"));
        }
        let f = crate::progression::faction_from_arg(l, 1)?;
        let ctx = (e.effect_ctx)((f + 0x18) as *mut c_void);
        if ctx.is_null() {
            return Err("faction effect context is null".into());
        }
        Ok((e.effect_value)(ctx, id as u16))
    })();
    match result {
        Ok(v) => {
            log!("se_faction_effect_value: id {id} = {v}");
            (api.pushnumber)(l, v);
            1
        }
        Err(e) => {
            log!("se_faction_effect_value: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}
