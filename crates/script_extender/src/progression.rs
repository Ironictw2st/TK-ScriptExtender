//! Faction progression level / world leader ("emperor") primitives (build 1.7.2.0).
//! Layouts and routines: notes/progression.md.
//!
//!   FACTION +8 id, +0x288 world handle (*(h)+0x78 = world), +0x2290 PROGRESSION manager
//!   PROGRESSION +0 faction, +0x10 -> levels vector {_, count @+4, data @+8}, +0x18 current level
//!               index, +0x30 lock byte; level object +8 key String*, +0x28 prestige threshold,
//!               +0x2c index
//!   world +0x3b68 faction manager; *(fm+0x270) = WORLD LEADER manager: +0xa4 count, +0xa8 data
//!   (FACTION*), max seats = campaign_variable(world, 0x24a)
//!
//! se_faction_progression_get(q_faction) -> level, max, key, is_world_leader, locked | nil, msg
//! se_faction_progression_set(q_faction, level) -> ok, msg
//!     Raises the level through the engine's own unlock+process path. Prestige is derived (not
//!     stored), so the level thresholds are temporarily rewritten (<= target: 0, > target:
//!     u32::MAX) around the call and restored afterwards.
//! se_world_leaders(q_faction) -> string "count/max: id,id,..."
//! se_world_leader_force(q_faction) -> ok, msg : seat the faction directly through the world
//!     leader manager (FUN_1416901c0 + FUN_141690300 on its capital, faction+0xd30), bypassing
//!     the eligibility check FUN_1419c1680(faction, 0x21) that FUN_1416a4c10 applies.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

const OFF_MGR: usize = 0x2290;
const OFF_LEVEL: usize = 0x18;
const OFF_LOCK: usize = 0x30;
const CV_MAX_WORLD_LEADERS: u32 = 0x24a;

struct Engine {
    process: unsafe extern "C" fn(*mut c_void),
    campaign_variable: unsafe extern "C" fn(*mut c_void, u32) -> u32,
    become_world_leader: unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8),
    add_world_leader_region: unsafe extern "C" fn(*mut c_void, *mut c_void, u8),
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            process: core::mem::transmute(t.get("progression_process")),
            campaign_variable: core::mem::transmute(t.get("campaign_variable")),
            become_world_leader: core::mem::transmute(t.get("become_world_leader")),
            add_world_leader_region: core::mem::transmute(t.get("add_world_leader_region")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_faction_progression_get", se_faction_progression_get);
    lua::set_global_fn(l, "se_faction_progression_set", se_faction_progression_set);
    lua::set_global_fn(l, "se_world_leaders", se_world_leaders);
    lua::set_global_fn(l, "se_world_leader_force", se_world_leader_force);
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

/// CA::String* -> Rust string (len @0, char* @+8).
unsafe fn ca_string(sp: usize) -> String {
    if !readable(sp, 16) {
        return String::new();
    }
    let len = rd(sp) as usize;
    let ptr = rq(sp + 8);
    if len == 0 || len > 128 || !readable(ptr, len) {
        return String::new();
    }
    String::from_utf8_lossy(core::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
}

/// Engine FACTION from a QUERY_FACTION script object: the candidate whose embedded progression
/// manager points back at it.
pub unsafe fn faction_from_arg(l: *mut LuaState, idx: c_int) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    let cands = [rq(rq(rq(p) + 8) + 0x18), rq(rq(p + 8) + 0x18), rq(p + 0x18), rq(p), p];
    for c in cands {
        if c != 0 && readable(c, OFF_MGR + 0x40) && rq(c + OFF_MGR) == c {
            return Ok(c);
        }
    }
    Err(format!("no candidate looks like a FACTION (p={:#x})", p))
}

unsafe fn world_of(faction: usize) -> usize {
    rq(rq(faction + 0x288) + 0x78)
}

struct Level {
    ptr: usize,
    index: u32,
    threshold: u32,
    key: String,
}

unsafe fn levels(faction: usize) -> Vec<Level> {
    let vec = rq(faction + OFF_MGR + 0x10);
    let mut out = Vec::new();
    if !readable(vec, 16) {
        return out;
    }
    let count = rd(vec + 4) as usize;
    let data = rq(vec + 8);
    for i in 0..count.min(32) {
        let lv = rq(data + i * 8);
        if lv != 0 && readable(lv, 0x30) {
            // progression_level_key copies the String object embedded at level+8 (FUN_140662cf0(out, level+8))
            out.push(Level { ptr: lv, index: rd(lv + 0x2c), threshold: rd(lv + 0x28), key: ca_string(lv + 8) });
        }
    }
    out
}

unsafe fn world_leader_mgr(world: usize) -> usize {
    rq(rq(world + 0x3b68) + 0x270)
}

unsafe fn leader_ids(wl: usize) -> Vec<u32> {
    let mut ids = Vec::new();
    if !readable(wl, 0xc0) {
        return ids;
    }
    let count = rd(wl + 0xa4) as usize;
    let data = rq(wl + 0xa8);
    for i in 0..count.min(16) {
        let f = rq(data + i * 8);
        if f != 0 && readable(f, 16) {
            ids.push(rd(f + 8));
        }
    }
    ids
}

struct View {
    id: u32,
    level: u32,
    max: u32,
    key: String,
    locked: bool,
    leader: bool,
    leaders: Vec<u32>,
    max_leaders: u32,
}

unsafe fn view(e: &Engine, faction: usize) -> View {
    let mgr = faction + OFF_MGR;
    let lv = levels(faction);
    let level = rd(mgr + OFF_LEVEL);
    let key = lv.iter().find(|x| x.index == level).map(|x| x.key.clone()).unwrap_or_default();
    let world = world_of(faction);
    let wl = world_leader_mgr(world);
    let leaders = leader_ids(wl);
    let id = rd(faction + 8);
    View {
        id,
        level,
        max: lv.iter().map(|x| x.index).max().unwrap_or(0),
        key,
        locked: rd(mgr + OFF_LOCK) & 0xff != 0,
        leader: leaders.contains(&id),
        leaders,
        max_leaders: if world != 0 { (e.campaign_variable)(world as *mut c_void, CV_MAX_WORLD_LEADERS) } else { 0 },
    }
}

unsafe extern "C" fn se_faction_progression_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let result: Result<View, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction_from_arg(l, 1)?;
        Ok(view(e, f))
    })();
    match result {
        Ok(v) => {
            log!("se_faction_progression_get: id={} level={}/{} key={} locked={} leader={} leaders={:?} max={}", v.id, v.level, v.max, v.key, v.locked, v.leader, v.leaders, v.max_leaders);
            (api.pushinteger)(l, v.level as isize);
            (api.pushinteger)(l, v.max as isize);
            lua::push_str(l, &v.key);
            (api.pushboolean)(l, v.leader as c_int);
            (api.pushboolean)(l, v.locked as c_int);
            5
        }
        Err(e) => {
            log!("se_faction_progression_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_faction_progression_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let target = (api.tointeger)(l, 2);
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction_from_arg(l, 1)?;
        let before = view(e, f);
        let lv = levels(f);
        if lv.is_empty() {
            return Err("faction has no progression levels".into());
        }
        if target < 0 || target as u32 > before.max {
            return Err(format!("level {target} is outside 0..{}", before.max));
        }
        let target = target as u32;
        if target <= before.level {
            return Err(format!("faction is already at level {} (only raising is supported)", before.level));
        }
        log!("se_faction_progression_set: id={} level {} -> {} (locked={}, leader={}, leaders={:?}/{})", before.id, before.level, target, before.locked, before.leader, before.leaders, before.max_leaders);
        // Rewrite thresholds so the engine's prestige check resolves to exactly `target`.
        let saved: Vec<(usize, u32)> = lv.iter().map(|x| (x.ptr, x.threshold)).collect();
        for x in &lv {
            let v = if x.index <= target { 0 } else { u32::MAX };
            core::ptr::write_unaligned((x.ptr + 0x28) as *mut u32, v);
        }
        (e.process)(f as *mut c_void);
        for (p, t) in saved {
            core::ptr::write_unaligned((p + 0x28) as *mut u32, t);
        }
        let after = view(e, f);
        let msg = format!("id {}: level {} -> {} ({}), world_leader {} -> {}, leaders now {:?}/{}, lock {} -> {}", after.id, before.level, after.level, after.key, before.leader, after.leader, after.leaders, after.max_leaders, before.locked, after.locked);
        log!("se_faction_progression_set: {msg}");
        if after.level == target { Ok(msg) } else { Err(format!("engine did not move the level: {msg}")) }
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_faction_progression_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_world_leaders(l: *mut LuaState) -> c_int {
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction_from_arg(l, 1)?;
        let v = view(e, f);
        let ids: Vec<String> = v.leaders.iter().map(|i| i.to_string()).collect();
        Ok(format!("{}/{}: {}", v.leaders.len(), v.max_leaders, ids.join(",")))
    })();
    match result {
        Ok(s) => { log!("se_world_leaders: {s}"); lua::push_str(l, &s); }
        Err(e) => { log!("se_world_leaders: {e}"); lua::push_str(l, &format!("error: {e}")); }
    }
    1
}

unsafe extern "C" fn se_world_leader_force(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction_from_arg(l, 1)?;
        let before = view(e, f);
        if before.leader {
            return Ok(format!("id {} is already a world leader", before.id));
        }
        if before.leaders.len() as u32 >= before.max_leaders {
            return Err(format!("all {} seats are taken: {:?}", before.max_leaders, before.leaders));
        }
        let capital = rq(f + 0xd30);
        if capital == 0 || !readable(capital, 0x100) {
            return Err("faction has no capital region (faction+0xd30)".into());
        }
        let wl = world_leader_mgr(world_of(f));
        if wl == 0 || !readable(wl, 0x130) {
            return Err("world leader manager not found".into());
        }
        log!("se_world_leader_force: id={} level={}/{} capital={:#x} leaders before {:?}", before.id, before.level, before.max, capital, before.leaders);
        (e.become_world_leader)(wl as *mut c_void, f as *mut c_void, 0, 0);
        let mid = view(e, f);
        if mid.leader {
            (e.add_world_leader_region)(wl as *mut c_void, capital as *mut c_void, 0);
        }
        let after = view(e, f);
        let msg = format!("id {}: world_leader {} -> {}, leaders now {:?}/{}", after.id, before.leader, after.leader, after.leaders, after.max_leaders);
        log!("se_world_leader_force: {msg}");
        if after.leader { Ok(msg) } else { Err(format!("engine declined the seat: {msg}")) }
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_world_leader_force: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
