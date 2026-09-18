//! Region slot buildings (build 1.7.2.0).
//!
//! Layout (from the QUERY_SLOT / MODIFY_SLOT natives and the CCQ_REGION_BUILDING_* executors):
//!   SLOT + 0x318 -> building manager M (has a vtable); SLOT + 0x320 byte = locked
//!   M + 0x20 -> current BUILDING B or null (FUN_14038bec0 "root")
//!   B + 0x20 -> building_levels record (+0x14e can_be_damaged), B + 0x28 health, B + 0x2c max
//!   FUN_141cc03c0(B) -> health percent; FUN_141cc0410(B, percent, force) sets it (100 = repaired)
//!   M vtable: +0x28 pay to complete next turn (gate +0xa0), +0x38 destroy(1,0,0), +0x40 repair,
//!             +0x130 list constructible levels (&vec{cap,count,data}, 1,1,0,0,0,0; 0x30-byte
//!             entries, entry+0 = building_levels record)
//!   FUN_141b0d6f0(M, record) = CCQ_REGION_BUILDING_CONSTRUCT (queues construction; the record
//!   must be in the +0x130 list, so upgrades and conversions go through the same call)
//!   building_levels table = FUN_14082a5a0(db_get(world+0x3b38)); record_base(table, &String)
//!
//! Natives (q_slot = QUERY_SLOT script object):
//!   se_slot_info(q_slot) -> has_building, level_key, health, can_damage, info
//!   se_slot_candidates(q_slot, only_valid, all_chains) -> "key,key,..."
//!   se_slot_damage(q_slot, percent) / se_slot_repair(q_slot, free) / se_slot_destroy(q_slot)
//!   se_slot_construct(q_slot, q_faction, level_key, force, all_chains, free, turns)
//!   se_slot_pay_to_complete(q_slot)
//!   List entry (0x30): +0 record, +8 manager, +0x10 cost, +0x14 turns, +0x18 reason bits
//!   (0x2000/0x4000 = cannot afford), +0x1c is-upgrade, +0x20 f32 1.0, +0x24 secondary cost.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

const OFF_SLOT_MANAGER: usize = 0x318;
const OFF_SLOT_LOCKED: usize = 0x320;
const OFF_MANAGER_BUILDING: usize = 0x20;
const OFF_BUILDING_RECORD: usize = 0x20;

struct Engine {
    construct: unsafe extern "C" fn(*mut c_void, *mut c_void),
    health_get: unsafe extern "C" fn(*mut c_void) -> i64,
    health_set: unsafe extern "C" fn(*mut c_void, u32, u8),
    can_damage: unsafe extern "C" fn(*mut c_void) -> u8,
    table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    record_base: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
    free: unsafe extern "C" fn(*mut c_void),
    base: usize,
    size: usize,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let (base, size) = crate::process::main_module();
    let e = unsafe {
        Engine {
            construct: core::mem::transmute(t.get("building_construct")),
            health_get: core::mem::transmute(t.get("building_health_get")),
            health_set: core::mem::transmute(t.get("building_health_set")),
            can_damage: core::mem::transmute(t.get("building_can_damage")),
            table: core::mem::transmute(t.get("building_levels_table")),
            db_get: core::mem::transmute(t.get("db_get")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
            free: core::mem::transmute(t.get("engine_free")),
            base,
            size,
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_slot_info", se_slot_info);
    lua::set_global_fn(l, "se_slot_candidates", se_slot_candidates);
    lua::set_global_fn(l, "se_slot_damage", se_slot_damage);
    lua::set_global_fn(l, "se_slot_repair", se_slot_repair);
    lua::set_global_fn(l, "se_slot_destroy", se_slot_destroy);
    lua::set_global_fn(l, "se_slot_construct", se_slot_construct);
    lua::set_global_fn(l, "se_slot_pay_to_complete", se_slot_pay_to_complete);
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
unsafe fn vf(obj: usize, off: usize) -> usize {
    rq(rq(obj) + off)
}

unsafe fn ca_string(sp: usize) -> String {
    if !readable(sp, 16) {
        return String::new();
    }
    // Short strings live inline: the qword at +8 has its top nibble == 8 and the characters
    // start at +0 (seen on building_levels key "3k_city_3").
    if rq(sp + 8) >> 60 == 8 {
        let raw = core::slice::from_raw_parts(sp as *const u8, 15);
        let n = raw.iter().position(|&c| c == 0).unwrap_or(15);
        let ok = n > 0 && raw[..n].iter().all(|c| c.is_ascii_graphic());
        return if ok { String::from_utf8_lossy(&raw[..n]).into_owned() } else { String::new() };
    }
    let len = rd(sp) as usize;
    let ptr = rq(sp + 8);
    if len == 0 || len > 128 || !readable(ptr, len) {
        return String::new();
    }
    String::from_utf8_lossy(core::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
}

unsafe fn record_key(rec: usize) -> String {
    if rec == 0 || !readable(rec, 0x20) {
        return String::new();
    }
    let s = ca_string(rq(rec + 8));
    if !s.is_empty() { return s; }
    ca_string(rec + 8)
}

unsafe fn with_string<R>(e: &Engine, s: &str, f: impl FnOnce(*const c_void) -> R) -> R {
    let mut c = s.as_bytes().to_vec();
    c.push(0);
    let mut buf = [0u8; 64];
    (e.string_from_cstr)(buf.as_mut_ptr() as *mut c_void, c.as_ptr() as *const c_char);
    let r = f(buf.as_ptr() as *const c_void);
    (e.string_dtor)(buf.as_mut_ptr() as *mut c_void);
    r
}

struct Slot {
    slot: usize,
    manager: usize,
    building: usize,
}

/// SLOT from a QUERY_SLOT script object: the candidate whose +0x318 manager has a vtable inside
/// the exe image and whose current building (if any) points at a record.
unsafe fn slot_from_arg(e: &Engine, l: *mut LuaState, idx: c_int) -> Result<Slot, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    let cands = [rq(rq(rq(p) + 8) + 0x18), rq(rq(p) + 0x18), rq(rq(p + 8) + 0x18), rq(p + 0x18), rq(p), p];
    for c in cands {
        if c != 0 && readable(c, 0x330) {
            let m = rq(c + OFF_SLOT_MANAGER);
            if m != 0 && readable(m, 0x40) {
                let vt = rq(m);
                if vt >= e.base && vt < e.base + e.size {
                    let b = rq(m + OFF_MANAGER_BUILDING);
                    if b == 0 || (readable(b, 0x40) && rq(b + OFF_BUILDING_RECORD) != 0) {
                        return Ok(Slot { slot: c, manager: m, building: b });
                    }
                }
            }
        }
    }
    Err(format!("no candidate looks like a SLOT (p={:#x})", p))
}

unsafe fn describe(e: &Engine, s: &Slot) -> String {
    let key = if s.building != 0 { record_key(rq(s.building + OFF_BUILDING_RECORD)) } else { String::new() };
    let health = if s.building != 0 { (e.health_get)(s.building as *mut c_void) } else { -1 };
    format!("slot={:#x} manager={:#x} (vtable rva {:#x}) building={:#x} key='{}' health={} locked={}", s.slot, s.manager, rq(s.manager).wrapping_sub(e.base), s.building, key, health, rd(s.slot + OFF_SLOT_LOCKED) & 0xff)
}

unsafe extern "C" fn se_slot_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let result: Result<(Slot, String, i64, bool, String), String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let s = slot_from_arg(e, l, 1)?;
        let key = if s.building != 0 { record_key(rq(s.building + OFF_BUILDING_RECORD)) } else { String::new() };
        let health = if s.building != 0 { (e.health_get)(s.building as *mut c_void) } else { 0 };
        let can = s.building != 0 && (e.can_damage)(s.building as *mut c_void) != 0;
        let info = describe(e, &s);
        Ok((s, key, health, can, info))
    })();
    match result {
        Ok((s, key, health, can, info)) => {
            log!("se_slot_info: {info}");
            (api.pushboolean)(l, (s.building != 0) as c_int);
            lua::push_str(l, &key);
            (api.pushinteger)(l, health as isize);
            (api.pushboolean)(l, can as c_int);
            lua::push_str(l, &info);
            5
        }
        Err(e) => {
            log!("se_slot_info: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

#[repr(C)]
struct Vec3 {
    cap: u32,
    count: u32,
    data: *mut c_void,
}

// arg6 (superchain filter) is a pointer-sized argument, the others are bytes.
type VfCandidates = unsafe extern "C" fn(*mut c_void, *mut Vec3, u32, u32, u32, usize, u32, u32);

/// Run `f` over the manager's constructible-level entries (0x30 bytes each, entry+0 = record).
/// M vfunc +0x130 -> +0x128 (FUN_141b207b0): arg3 only_valid (1 = return nothing as soon as the
/// slot has a blocking reason; 0 = list everything), arg5 all_chains (0 = upgrades/conversions of
/// the current building, 1 = every chain the slot can hold), arg6 superchain filter, arg8 ignore
/// the siege check. The construct virtual (+0x10, FUN_141b0d980) takes one of these entries.
unsafe fn with_candidates<R>(e: &Engine, s: &Slot, only_valid: bool, all_chains: bool, f: impl FnOnce(&[(usize, usize, String)]) -> R) -> R {
    let mut v = Vec3 { cap: 0, count: 0, data: core::ptr::null_mut() };
    let list: VfCandidates = core::mem::transmute(vf(s.manager, 0x130));
    list(s.manager as *mut c_void, &mut v, only_valid as u32, 1, all_chains as u32, 0, 0, 0);
    let mut entries = Vec::new();
    for i in 0..(v.count as usize).min(512) {
        let entry = v.data as usize + i * 0x30;
        let rec = rq(entry);
        let key = record_key(rec);
        entries.push((entry, rec, if key.is_empty() { format!("{:#x}", rec) } else { key }));
    }
    let r = f(&entries);
    if !v.data.is_null() {
        (e.free)(v.data);
    }
    r
}

unsafe fn candidates(e: &Engine, s: &Slot, only_valid: bool, all_chains: bool) -> Vec<String> {
    with_candidates(e, s, only_valid, all_chains, |entries| {
        for (entry, _, key) in entries.iter().take(12) {
            let words: Vec<String> = (0..6).map(|k| format!("{:016x}", rq(*entry + k * 8))).collect();
            log!("  candidate {key}: {}", words.join(" "));
        }
        entries.iter().map(|(_, _, k)| k.clone()).collect()
    })
}

unsafe extern "C" fn se_slot_candidates(l: *mut LuaState) -> c_int {
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let s = slot_from_arg(e, l, 1)?;
        let api = lua::api().ok_or("lua api missing")?;
        let only_valid = (api.toboolean)(l, 2) != 0;
        let all_chains = (api.toboolean)(l, 3) != 0;
        let list = candidates(e, &s, only_valid, all_chains);
        log!("se_slot_candidates(only_valid={only_valid}, all_chains={all_chains}): {} -> {} candidates: {}", describe(e, &s), list.len(), list.join(", "));
        Ok(list.join(","))
    })();
    match result {
        Ok(s) => { lua::push_str(l, &s); 1 }
        Err(e) => { log!("se_slot_candidates: {e}"); lua::push_str(l, ""); lua::push_str(l, &e); 2 }
    }
}

unsafe fn bool_result(l: *mut LuaState, r: Result<String, String>, who: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match r {
        Ok(m) => { log!("{who}: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("{who}: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_slot_damage(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let pct = (api.tointeger)(l, 2) as i64;
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if !(0..=100).contains(&pct) { return Err(format!("percent {pct} is outside 0..100")); }
        let s = slot_from_arg(e, l, 1)?;
        if s.building == 0 { return Err("slot has no building".into()); }
        if (e.can_damage)(s.building as *mut c_void) == 0 { return Err("this building cannot be damaged (can_be_damaged = false)".into()); }
        let before = (e.health_get)(s.building as *mut c_void);
        // MODIFY_SLOT:damage_building: new = max(pct, health) - pct
        let hp = before.max(0) as u32;
        let new = hp.max(pct as u32) - pct as u32;
        (e.health_set)(s.building as *mut c_void, new, 0);
        let after = (e.health_get)(s.building as *mut c_void);
        Ok(format!("{}: health {before} -> {after} (damage {pct})", describe(e, &s)))
    })();
    bool_result(l, r, "se_slot_damage")
}

unsafe extern "C" fn se_slot_repair(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let free = (api.toboolean)(l, 2) != 0;
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let s = slot_from_arg(e, l, 1)?;
        if s.building == 0 { return Err("slot has no building".into()); }
        let before = (e.health_get)(s.building as *mut c_void);
        if free {
            (e.health_set)(s.building as *mut c_void, 100, 0);
        } else {
            let f: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(vf(s.manager, 0x40));
            f(s.manager as *mut c_void);
        }
        let after = (e.health_get)(s.building as *mut c_void);
        Ok(format!("{}: health {before} -> {after} ({})", describe(e, &s), if free { "direct, free" } else { "engine repair command" }))
    })();
    bool_result(l, r, "se_slot_repair")
}

unsafe extern "C" fn se_slot_destroy(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let s = slot_from_arg(e, l, 1)?;
        if s.building == 0 { return Err("slot has no building".into()); }
        if rd(s.slot + OFF_SLOT_LOCKED) & 0xff != 0 { return Err("slot is locked (slot+0x320)".into()); }
        let before = describe(e, &s);
        let f: unsafe extern "C" fn(*mut c_void, u32, u32, u32) = core::mem::transmute(vf(s.manager, 0x38));
        f(s.manager as *mut c_void, 1, 0, 0);
        let after = rq(s.manager + OFF_MANAGER_BUILDING);
        Ok(format!("{before} -> building now {:#x}", after))
    })();
    bool_result(l, r, "se_slot_destroy")
}

unsafe extern "C" fn se_slot_construct(l: *mut LuaState) -> c_int {
    let key = lua::to_str(l, 3);
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() { return Err("level_key is empty".into()); }
        let s = slot_from_arg(e, l, 1)?;
        let faction = crate::progression::faction_from_arg(l, 2)?;
        let world = rq(rq(faction + 0x288) + 0x78);
        if world == 0 || !readable(world + 0x3b38, 8) { return Err("could not derive the model from the faction".into()); }
        let db = (e.db_get)((world + 0x3b38) as *mut c_void);
        if db.is_null() { return Err("db manager missing".into()); }
        let table = (e.table)(db);
        if table.is_null() { return Err("building_levels table missing".into()); }
        let rec = with_string(e, &key, |sp| (e.record_base)(table, sp)) as usize;
        if rec == 0 || !readable(rec, 0x20) { return Err(format!("no building_levels record named '{key}'")); }
        let api = lua::api().ok_or("lua api missing")?;
        let force = (api.toboolean)(l, 4) != 0;
        let all_chains = (api.toboolean)(l, 5) != 0;
        let free = (api.toboolean)(l, 6) != 0;
        let turns = (api.tointeger)(l, 7) as i64;
        if turns > 100 { return Err(format!("turns {turns} is out of range")); }
        // FUN_141b0d980 stores the new construction item at M+0x10 without looking at the old one.
        if rq(s.manager + 0x10) != 0 { return Err(format!("this slot already has a construction in progress (manager+0x10 = {:#x}); cancel it first", rq(s.manager + 0x10))); }
        let before = describe(e, &s);
        // force = list with only_valid off (blocked options included) and hand the entry to the
        // construct virtual directly, which is what CCQ_REGION_BUILDING_CONSTRUCT does after its
        // own (only_valid) lookup.
        with_candidates(e, &s, !force, all_chains, |entries| {
            match entries.iter().find(|(_, r, _)| *r == rec) {
                Some((entry, _, _)) => {
                    let words: Vec<String> = (0..6).map(|k| format!("{:016x}", rq(*entry + k * 8))).collect();
                    log!("se_slot_construct: entry {}", words.join(" "));
                    // Work on a copy: +0x10 cost and +0x24/+0x28 secondary cost are what
                    // FUN_141b106a0 charges, +0x14 is the construction time in turns.
                    let mut item = [0u8; 0x30];
                    core::ptr::copy_nonoverlapping(*entry as *const u8, item.as_mut_ptr(), 0x30);
                    let ip = item.as_mut_ptr() as usize;
                    if free {
                        core::ptr::write_unaligned((ip + 0x10) as *mut u32, 0);
                        core::ptr::write_unaligned((ip + 0x24) as *mut u32, 0);
                        core::ptr::write_unaligned((ip + 0x28) as *mut u32, 0);
                    }
                    if turns > 0 {
                        core::ptr::write_unaligned((ip + 0x14) as *mut u32, turns as u32);
                    }
                    let construct: unsafe extern "C" fn(*mut c_void, *mut c_void) = core::mem::transmute(vf(s.manager, 0x10));
                    construct(s.manager as *mut c_void, ip as *mut c_void);
                    Ok(format!("{before}: construction of '{key}' issued (force={force}, all_chains={all_chains}, free={free}, cost {} -> {}, turns {} -> {}, reasons {:#x})",
                        rd(*entry + 0x10), rd(ip + 0x10), rd(*entry + 0x14), rd(ip + 0x14), rd(*entry + 0x18)))
                }
                None => {
                    let keys: Vec<String> = entries.iter().map(|(_, _, k)| k.clone()).collect();
                    Err(format!("'{key}' is not in this slot's list (force={force}, all_chains={all_chains}); {} candidates: {}", keys.len(), keys.join(", ")))
                }
            }
        })
    })();
    bool_result(l, r, "se_slot_construct")
}

unsafe extern "C" fn se_slot_pay_to_complete(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let s = slot_from_arg(e, l, 1)?;
        let can: unsafe extern "C" fn(*mut c_void) -> u8 = core::mem::transmute(vf(s.manager, 0xa0));
        if can(s.manager as *mut c_void) == 0 { return Err("engine says this slot cannot pay to complete now (no construction in progress?)".into()); }
        let pay: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(vf(s.manager, 0x28));
        pay(s.manager as *mut c_void);
        Ok(format!("{}: pay-to-complete-next-turn issued", describe(e, &s)))
    })();
    bool_result(l, r, "se_slot_pay_to_complete")
}
