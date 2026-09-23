//! The recruitment-pool primitives exposed to Lua.
//!
//! Engine facts (build 1.7.2.0, see notes/character_pools.md):
//!   CHARACTER +0x240 cqi, +0x260 handle->DETAILS, +0x270 handle->FACTION,
//!             +0x278 state (0 pool, 1 recruited, 2 other)
//!   FACTION   +0xd60 character manager, +0xd68/+0xd80/+0xd98 per-state lists,
//!             +0xdb0 RECRUITMENT_POOL object (may be null)
//!   DETAILS   +0xbd0 handle->assignment (must be empty to release)
//!   release_to_pool(faction+0xd60, details, flag): creates the pool entry, sets state 0,
//!             relinks into the pool list (the engine routine behind "Release from Service").
//!
//! Every function takes the QUERY_CHARACTER script object *and* its cqi; the cqi is
//! cross-checked against the character found through the object before anything is touched.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

type ReleaseToPool = unsafe extern "C" fn(*mut c_void, *mut c_void, u8);

struct Engine {
    release_to_pool: ReleaseToPool,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    // SAFETY: anchor-verified address; prototype from the decompile of FUN_1417c6130.
    let e = unsafe {
        Engine {
            release_to_pool: core::mem::transmute(t.get("release_to_pool")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_ping", se_ping);
    lua::set_global_fn(l, "se_version", se_version);
    lua::set_global_fn(l, "se_save_chunking", se_save_chunking);
    lua::set_global_fn(l, "se_log", se_log);
    lua::set_global_fn(l, "se_char_info", se_char_info);
    lua::set_global_fn(l, "se_release_to_pool", se_release_to_pool);
    lua::set_global_fn(l, "se_pool_lock_get", se_pool_lock_get);
    lua::set_global_fn(l, "se_pool_lock_set", se_pool_lock_set);
    crate::recruit::register(l);
    crate::units::register(l);
    crate::progression::register(l);
    crate::chars::register(l);
    crate::cai::register(l);
    crate::potential::register(l);
    crate::xp::register(l);
    crate::build::register(l);
    crate::buildings::register(l);
    crate::alliances::register(l);
    crate::bundles::register(l);
    crate::autoresolve::register(l);
    crate::income::register(l);
    crate::profiler::register(l);
    crate::perf::register(l);
    crate::diag::register(l);
    crate::diptrace::register(l);
    crate::followup::register(l);
    crate::airecruit::register(l);
    crate::diplomacy::register(l);
    crate::crash::register(l);
    crate::marriage::register(l);
}

/// Pool availability lock on the CHARACTER: `+0x75c` status byte (10 = available, 5 = locked,
/// as seen live) and `+0x764` round counter. The court's candidate list hides any pool member
/// whose status is not 10 (validation reason 0x10 in FUN_141a508b0). The game sets 5 on a
/// faction change and clears it back to 10/0 on a later turn tick.
const OFF_LOCK_STATUS: usize = 0x75c;
const OFF_LOCK_COUNTER: usize = 0x764;

unsafe fn wr8(p: usize, v: u8) -> bool {
    if readable(p, 1) { core::ptr::write_unaligned(p as *mut u8, v); true } else { false }
}
unsafe fn wr32(p: usize, v: u32) -> bool {
    if readable(p, 4) { core::ptr::write_unaligned(p as *mut u32, v); true } else { false }
}

/// se_pool_lock_get(query_character, cqi) -> status:int, counter:int   (or nil, message)
unsafe extern "C" fn se_pool_lock_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match character_from_arg(l, 1, cqi) {
        Ok(ch) => {
            let status = rd(ch + OFF_LOCK_STATUS) & 0xff;
            let counter = rd(ch + OFF_LOCK_COUNTER);
            log!("se_pool_lock_get: cqi {cqi} status={status} counter={counter} state={}", rd(ch + 0x278));
            (api.pushinteger)(l, status as isize);
            (api.pushinteger)(l, counter as isize);
            2
        }
        Err(e) => {
            log!("se_pool_lock_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

/// se_pool_lock_set(query_character, cqi, status, counter) -> ok:boolean, message:string
/// status 10 + counter 0 = available now; status 5 + counter N = locked (game clears it on a
/// turn tick). Any byte/int value is accepted so the field can be experimented with.
unsafe extern "C" fn se_pool_lock_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let status = ((api.tointeger)(l, 3) & 0xff) as u8;
    let counter = (api.tointeger)(l, 4) as u32;
    let result: Result<String, String> = (|| {
        let ch = character_from_arg(l, 1, cqi)?;
        let old_s = rd(ch + OFF_LOCK_STATUS) & 0xff;
        let old_c = rd(ch + OFF_LOCK_COUNTER);
        if !wr8(ch + OFF_LOCK_STATUS, status) || !wr32(ch + OFF_LOCK_COUNTER, counter) {
            return Err("character memory not writable".into());
        }
        let msg = format!("cqi {cqi}: lock status {old_s} -> {status}, counter {old_c} -> {counter}");
        log!("se_pool_lock_set: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_pool_lock_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
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

/// Walk from a Lua argument to the engine CHARACTER. The script object's exact shape is not
/// assumed: several plausible indirections are tried and the one whose cqi field matches wins.
pub unsafe fn character_from_arg(l: *mut LuaState, idx: c_int, cqi: u32) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    if p == 0 {
        return Err("null userdata".into());
    }
    let mut candidates: Vec<(String, usize)> = Vec::new();
    candidates.push(("p".into(), p));
    candidates.push(("*p".into(), rq(p)));
    candidates.push(("*(p+8)+0x18".into(), rq(rq(p + 8) + 0x18)));
    candidates.push(("*(*p+8)+0x18".into(), rq(rq(rq(p) + 8) + 0x18)));
    candidates.push(("*(p+0x18)".into(), rq(p + 0x18)));
    candidates.push(("*(*p+0x18)".into(), rq(rq(p) + 0x18)));
    for (how, c) in &candidates {
        if *c != 0 && readable(*c, 0x300) && rd(*c + 0x240) == cqi && rd(*c + 0x278) <= 2 {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if crate::chatty(&N, 10) { log!("character_from_arg: cqi {cqi} matched via {how} -> {:#x}", c); }
            return Ok(*c);
        }
    }
    let dump: Vec<String> = candidates
        .iter()
        .map(|(h, c)| format!("{h}={:#x}(cqi@240={})", c, if *c != 0 && readable(*c, 0x300) { rd(*c + 0x240) } else { 0 }))
        .collect();
    Err(format!("no candidate matched cqi {cqi}: {}", dump.join(" ")))
}

struct CharView {
    ch: usize,
    state: u32,
    faction: usize,
    faction_id: u32,
    details: usize,
    pool: usize,
    assignment: usize,
}

unsafe fn view(ch: usize) -> CharView {
    let faction = rq(rq(ch + 0x270));
    let details = rq(rq(ch + 0x260));
    CharView {
        ch,
        state: rd(ch + 0x278),
        faction,
        faction_id: rd(faction + 8),
        details,
        pool: rq(faction + 0xdb0),
        assignment: rq(rq(details + 0xbd0)),
    }
}

fn describe(v: &CharView) -> String {
    format!(
        "char={:#x} state={} faction={:#x} faction_id={} details={:#x} pool_obj={:#x} assignment={:#x}",
        v.ch, v.state, v.faction, v.faction_id, v.details, v.pool, v.assignment
    )
}

/// se_ping() -> "script_extender: pong"
unsafe extern "C" fn se_ping(l: *mut LuaState) -> c_int {
    lua::push_str(l, "script_extender: pong");
    1
}

/// se_log(text) : append a line to script_extender.log (fallback logger for the Lua module)
unsafe extern "C" fn se_log(l: *mut LuaState) -> c_int {
    let s = lua::to_str(l, 1);
    log!("[lua] {s}");
    0
}

/// se_version() -> "0.9.0"
unsafe extern "C" fn se_version(l: *mut LuaState) -> c_int {
    lua::push_str(l, env!("CARGO_PKG_VERSION"));
    1
}

/// se_save_chunking() -> bool : script_extender.cfg `save_chunking` (default on). se_api.lua splits
/// saved strings above the engine's 64 KiB cap into chunks only while this is true.
unsafe extern "C" fn se_save_chunking(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    (api.pushboolean)(l, crate::build::hook_enabled("save_chunking") as c_int);
    1
}

/// se_char_info(query_character, cqi) -> string
unsafe extern "C" fn se_char_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match character_from_arg(l, 1, cqi) {
        Ok(ch) => {
            let v = view(ch);
            let s = describe(&v);
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if crate::chatty(&N, 10) { log!("se_char_info: {s}"); }
            lua::push_str(l, &s);
        }
        Err(e) => {
            log!("se_char_info: {e}");
            lua::push_str(l, &format!("error: {e}"));
        }
    }
    1
}

/// se_release_to_pool(query_character, cqi) -> ok:boolean, message:string
///
/// Moves a RECRUITED character of any faction into that same faction's recruitment pool by
/// calling the engine's own release routine. Faction changes are left to Lua's
/// `move_to_faction`, which the engine implements as a pool-to-pool transfer for pool
/// characters.
unsafe extern "C" fn se_release_to_pool(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let result: Result<String, String> = (|| {
        let engine = ENGINE.get().ok_or("engine table missing")?;
        let ch = character_from_arg(l, 1, cqi)?;
        let v = view(ch);
        log!("se_release_to_pool: before {}", describe(&v));
        if v.state != 1 {
            return Err(format!("character is not in the recruited list (state {})", v.state));
        }
        if v.faction == 0 || v.details == 0 {
            return Err("character has no faction or no details".into());
        }
        if v.pool == 0 {
            return Err("faction has no recruitment pool object".into());
        }
        if v.assignment != 0 {
            return Err("character holds a post/assignment; unassign first".into());
        }
        let mgr = (v.faction + 0xd60) as *mut c_void;
        (engine.release_to_pool)(mgr, v.details as *mut c_void, 0);
        let after = view(ch);
        log!("se_release_to_pool: after  {}", describe(&after));
        if after.state == 0 {
            Ok(format!("released to pool of faction {}", after.faction_id))
        } else {
            Err(format!("engine declined; state is still {}", after.state))
        }
    })();
    match result {
        Ok(msg) => {
            (api.pushboolean)(l, 1);
            lua::push_str(l, &msg);
        }
        Err(e) => {
            log!("se_release_to_pool: {e}");
            (api.pushboolean)(l, 0);
            lua::push_str(l, &e);
        }
    }
    2
}
