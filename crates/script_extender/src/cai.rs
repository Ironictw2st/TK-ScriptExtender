//! Campaign AI personality read/write (build 1.7.2.0), from the native
//! MODIFY_CAMPAIGN_AI:cai_force_personality_change (FUN_1415a1520 -> FUN_141b94700):
//!
//!   CAI manager = *(script_obj + 0x18); cai faction = FUN_1418b20e0(mgr+8, &String(faction_key))
//!   personality component = *(cai_faction + 0x1b0)  (FUN_1411ea540)
//!     +0x10 current personality key (CA::String, inline copy)
//!     +0x30 pointer to a cell holding the personality record; key = inline String at record+8
//!     +0x38 selector (group entries used by the random re-roll FUN_141d25fd0 / FUN_141d54230)
//!   The re-roll ends with: cell -> +0x30, string_assign(comp+0x10, record+8),
//!   FUN_141b94560(*mgr = AI world, cai_faction) (re-evaluates the faction against every other one).
//!   Personality records come from the DB table returned by FUN_140844730(db_get(model+0x3b38))
//!   ("cai_personalities_table"), looked up with record_base(table, &String(key)).
//!
//! se_cai_personality_get(q_cai, faction_key) -> key:string, current_cell_key:string | nil, msg
//! se_cai_personality_set(q_cai, q_faction, faction_key, personality_key) -> ok, msg

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

struct Engine {
    faction_by_key: unsafe extern "C" fn(*mut c_void, *const c_void) -> usize,
    table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    record_base: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
    string_assign: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    apply: unsafe extern "C" fn(*mut c_void, *mut c_void),
    by_record: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize,
    apply_component: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            faction_by_key: core::mem::transmute(t.get("cai_faction_by_key")),
            table: core::mem::transmute(t.get("cai_personalities_table")),
            db_get: core::mem::transmute(t.get("db_get")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
            string_assign: core::mem::transmute(t.get("string_assign")),
            apply: core::mem::transmute(t.get("cai_apply_personality")),
            by_record: core::mem::transmute(t.get("cai_personality_by_record")),
            apply_component: core::mem::transmute(t.get("cai_apply_component")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_cai_personality_get", se_cai_personality_get);
    lua::set_global_fn(l, "se_cai_personality_set", se_cai_personality_set);
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

/// Inline CA::String {u32 len, u32 cap, char* @+8} -> Rust string.
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

/// Key of a personality record: inline String at +8, else String* at +8.
unsafe fn record_key(rec: usize) -> String {
    let s = ca_string(rec + 8);
    if !s.is_empty() { return s; }
    ca_string(rq(rec + 8))
}

/// Run f with a temporary CA::String built from `s`.
unsafe fn with_string<R>(e: &Engine, s: &str, f: impl FnOnce(*const c_void) -> R) -> R {
    let mut c = s.as_bytes().to_vec();
    c.push(0);
    let mut buf = [0u8; 64];
    (e.string_from_cstr)(buf.as_mut_ptr() as *mut c_void, c.as_ptr() as *const c_char);
    let r = f(buf.as_ptr() as *const c_void);
    (e.string_dtor)(buf.as_mut_ptr() as *mut c_void);
    r
}

/// CAI manager from the MODIFY_CAMPAIGN_AI script object; validated by resolving `key`.
unsafe fn manager_and_faction(e: &Engine, l: *mut LuaState, idx: c_int, key: &str) -> Result<(usize, usize), String> {
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    let cands = [rq(rq(p) + 0x18), rq(rq(rq(p) + 8) + 0x18), rq(rq(p + 8) + 0x18), rq(p + 0x18), rq(p), p];
    for c in cands {
        if c != 0 && readable(c, 0x40) && rq(c) > 0x10000 {
            let holder = rq(c + 8);
            if holder != 0 && readable(holder, 0x120) {
                let f = with_string(e, key, |s| (e.faction_by_key)(holder as *mut c_void, s));
                if f != 0 && readable(f, 0x1c0) {
                    return Ok((c, f));
                }
            }
        }
    }
    Err(format!("no candidate looks like the CAI manager, or faction '{key}' is unknown to it (p={:#x})", p))
}

struct View {
    faction: usize,
    component: usize,
    key: String,
    cell: usize,
    record: usize,
    record_key: String,
}

unsafe fn view(cai_faction: usize) -> Result<View, String> {
    let comp = rq(cai_faction + 0x1b0);
    if comp == 0 || !readable(comp, 0x40) {
        return Err("cai faction has no personality component (+0x1b0)".into());
    }
    let cell = rq(comp + 0x30);
    let record = rq(cell);
    Ok(View { faction: cai_faction, component: comp, key: ca_string(comp + 0x10), cell, record, record_key: if record != 0 { record_key(record) } else { String::new() } })
}

unsafe extern "C" fn se_cai_personality_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    let result: Result<View, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() { return Err("faction_key is empty".into()); }
        let (_mgr, f) = manager_and_faction(e, l, 1, &key)?;
        view(f)
    })();
    match result {
        Ok(v) => {
            log!("se_cai_personality_get: {key}: cai_faction={:#x} component={:#x} key='{}' cell={:#x} record={:#x} record_key='{}'", v.faction, v.component, v.key, v.cell, v.record, v.record_key);
            lua::push_str(l, &v.key);
            lua::push_str(l, &v.record_key);
            2
        }
        Err(e) => {
            log!("se_cai_personality_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_cai_personality_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 3);
    let pkey = lua::to_str(l, 4);
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() || pkey.is_empty() { return Err("faction_key / personality_key must be non-empty".into()); }
        let (mgr, f) = manager_and_faction(e, l, 1, &key)?;
        let faction = crate::progression::faction_from_arg(l, 2)?;
        let world = rq(rq(faction + 0x288) + 0x78);
        if world == 0 || !readable(world + 0x3b38, 8) {
            return Err("could not derive the model from the faction".into());
        }
        let db = (e.db_get)((world + 0x3b38) as *mut c_void);
        if db.is_null() { return Err("db manager missing".into()); }
        let table = (e.table)(db);
        if table.is_null() { return Err("cai_personalities table missing".into()); }
        let rec = with_string(e, &pkey, |s| (e.record_base)(table, s)) as usize;
        if rec == 0 || !readable(rec, 0x20) {
            return Err(format!("no cai_personalities record named '{pkey}'"));
        }
        let before = view(f)?;
        log!("se_cai_personality_set: {key}: '{}' (object {:#x} '{}') -> '{pkey}' (record {:#x} '{}')", before.key, before.record, before.record_key, rec, record_key(rec));
        // The component's +0x30 must be the engine's runtime personality object for the record
        // (a bare record cell crashed the next reader at object+0xb0). The engine resolves it on
        // load through the registry at *(*(*(cai_faction+0xc0)+0xd8)+0xa70) (FUN_141d502f0).
        // FUN_141d25fd0 calls the re-evaluation as FUN_141b94560(*mgr, cai_faction): the first
        // field of the CAI manager is the AI world object that owns the faction lists (+0xa58).
        let ai_world = rq(mgr);
        if ai_world == 0 || !readable(ai_world + 0xa80, 8) {
            return Err(format!("CAI manager {:#x} has no AI world object at +0", mgr));
        }
        // The registry (FUN_141d502f0: FUN_141babe00(x) = *(x+0xa70)) is expected in the AI world
        // beside the faction lists. Self-check: it must map the faction's current record back to
        // the faction's current personality object before anything is written.
        if before.record == 0 || before.cell == 0 {
            return Err("faction has no current personality object to validate the registry against".into());
        }
        let mut registry = 0usize;
        for cand in [rq(ai_world + 0xa70), rq(rq(f + 0xc0) + 0xa70)] {
            if cand != 0 && readable(cand, 0x40) {
                let cur = (e.by_record)(cand as *mut c_void, before.record as *mut c_void);
                log!("se_cai_personality_set: registry candidate {:#x}: current record -> {:#x} (expected {:#x})", cand, cur, before.cell);
                if cur == before.cell { registry = cand; break; }
            }
        }
        if registry == 0 {
            return Err(format!("no registry candidate reproduces the current personality object {:#x}; refusing", before.cell));
        }
        let obj = (e.by_record)(registry as *mut c_void, rec as *mut c_void);
        if obj == 0 || !readable(obj, 0xc0) || rq(obj) != rec || rq(obj + 0xb0) == 0 {
            return Err(format!("registry returned no usable personality object for '{pkey}' (got {:#x})", obj));
        }
        log!("se_cai_personality_set: registry={:#x} personality object={:#x} (+0xb0={:#x})", registry, obj, rq(obj + 0xb0));
        core::ptr::write_unaligned((before.component + 0x30) as *mut usize, obj);
        (e.string_assign)((before.component + 0x10) as *mut c_void, (rec + 8) as *const c_void);
        (e.apply)(ai_world as *mut c_void, f as *mut c_void);
        let after = view(f)?;
        let msg = format!("{key}: personality '{}' -> '{}' (record key '{}')", before.key, after.key, after.record_key);
        log!("se_cai_personality_set: {msg}");
        Ok(msg)
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_cai_personality_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
