//! Read/write fields of loaded DB records by table slot (aicamp.16, AI construction budget for MCT).
//! The DB manager (db_get(model+0x3b38)) caches each loaded table at a fixed offset; the lazy getter
//! for cai_personalities_budget_allocations_table stores it at db+0x120c0 (FUN_14083f4b0 chunk:
//! `*(db + 0x120c0) = table`). record_base(table, &String(key)) -> record.
//!
//! se_db_rec_dump(q_faction, slot_offset, key, n_dwords) -> "i32,i32,..." | nil, msg   (read-only)
//! se_db_rec_set_i32(q_faction, slot_offset, key, byte_offset, value) -> old | nil, msg
//!   The FIRST value ever seen at each (slot, key, offset) is kept for the whole game session (DB records
//!   outlive campaign loads; Lua states do not): se_db_rec_restore_all(q_faction) -> n puts them all back.
//! Known slots (1.7.2.0): 0x120c0 cai_personalities_budget_allocations (construction cap +0x28,
//!   alloc +0x2c); 0x1ca8 cai_personality_financial_context_controls.
use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static ORIGINAL: Mutex<Option<HashMap<(usize, String, usize), i32>>> = Mutex::new(None);

struct Engine {
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    record_base: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
}
static ENGINE: OnceLock<Engine> = OnceLock::new();

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            db_get: core::mem::transmute(t.get("db_get")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_db_rec_dump", se_db_rec_dump);
    lua::set_global_fn(l, "se_db_rec_set_i32", se_db_rec_set_i32);
    lua::set_global_fn(l, "se_db_rec_restore_all", se_db_rec_restore_all);
}

/// record address for (faction arg 1, slot offset arg 2, key arg 3)
pub unsafe fn record(l: *mut LuaState, slot: usize, key: &str) -> Result<usize, String> {
    let e = ENGINE.get().ok_or("engine table missing")?;
    if key.is_empty() || key.len() > 120 { return Err("bad key".into()); }
    if slot == 0 || slot > 0x40000 || slot % 8 != 0 { return Err("bad slot offset".into()); }
    let faction = crate::progression::faction_from_arg(l, 1)?;
    let world = rq(rq(faction + 0x288) + 0x78);
    if world == 0 || !readable(world + 0x3b38, 8) { return Err("could not derive the model from the faction".into()); }
    let db = (e.db_get)((world + 0x3b38) as *mut c_void) as usize;
    if db == 0 || !readable(db + slot, 8) { return Err("db manager missing".into()); }
    let table = rq(db + slot);
    if table == 0 || !readable(table, 0x20) { return Err(format!("table at db+{slot:#x} not loaded")); }
    let mut c = key.as_bytes().to_vec();
    c.push(0);
    let mut buf = [0u8; 64];
    (e.string_from_cstr)(buf.as_mut_ptr() as *mut c_void, c.as_ptr() as *const c_char);
    let rec = (e.record_base)(table as *mut c_void, buf.as_ptr() as *const c_void) as usize;
    (e.string_dtor)(buf.as_mut_ptr() as *mut c_void);
    if rec == 0 || !readable(rec, 0x40) { return Err(format!("no record '{key}'")); }
    Ok(rec)
}

unsafe extern "C" fn se_db_rec_dump(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let slot = (api.tointeger)(l, 2) as usize;
    let key = lua::to_str(l, 3);
    let n = ((api.tointeger)(l, 4) as usize).clamp(1, 256);
    match record(l, slot, &key) {
        Ok(rec) if readable(rec, n * 4) => {
            let v: Vec<String> = (0..n).map(|i| (core::ptr::read_unaligned((rec + i * 4) as *const i32)).to_string()).collect();
            lua::push_str(l, &v.join(","));
            1
        }
        Ok(_) => { (api.pushnil)(l); lua::push_str(l, "record shorter than requested"); 2 }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

unsafe extern "C" fn se_db_rec_set_i32(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let slot = (api.tointeger)(l, 2) as usize;
    let key = lua::to_str(l, 3);
    let off = (api.tointeger)(l, 4) as usize;
    let val = (api.tointeger)(l, 5) as i32;
    if off % 4 != 0 || off >= 0x400 { (api.pushnil)(l); lua::push_str(l, "bad byte offset"); return 2; }
    match record(l, slot, &key) {
        Ok(rec) if readable(rec + off, 4) => {
            let p = (rec + off) as *mut i32;
            let old = core::ptr::read_unaligned(p);
            if let Ok(mut g) = ORIGINAL.lock() {
                g.get_or_insert_with(HashMap::new).entry((slot, key.clone(), off)).or_insert(old);
            }
            core::ptr::write_unaligned(p, val);
            log!("se_db_rec_set_i32: db+{slot:#x} '{key}' +{off:#x}: {old} -> {val}");
            (api.pushinteger)(l, old as isize);
            1
        }
        Ok(_) => { (api.pushnil)(l); lua::push_str(l, "offset not readable"); 2 }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

unsafe extern "C" fn se_db_rec_restore_all(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let items: Vec<((usize, String, usize), i32)> = ORIGINAL.lock().ok()
        .and_then(|g| g.as_ref().map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())).unwrap_or_default();
    let mut n = 0;
    for ((slot, key, off), v) in items {
        if let Ok(rec) = record(l, slot, &key) {
            if readable(rec + off, 4) {
                core::ptr::write_unaligned((rec + off) as *mut i32, v);
                n += 1;
            }
        }
    }
    if n > 0 { log!("se_db_rec_restore_all: {n} values restored"); }
    (api.pushinteger)(l, n as isize);
    1
}
