//! Effect bundles: inspect and (later) redefine a bundle's effect list at runtime.
//!
//!   effect_bundles table = FUN_140911650(db_get(world+0x3b38)); record = record_base(table, key)
//!   EFFECT_BUNDLE record: +0x3c effect count, +0x40 -> entries (0x30 bytes each; from the
//!   potential-handicap apply and FUN_140e87a50: entry+8 effect record, entry+0x10 scope record,
//!   entry+0x28 advancement stage; value expected at +0x18, to be confirmed live)
//!   apply (faction): wrapper = FUN_140e6ea50(&w, record, turns); FUN_1419a2f80(faction, &w)
//!
//!   Entry (0x30, confirmed live): +0 effect record, +8 scope record, +0x10 f32 value,
//!   +0x18 vector{cap,count,data} of resolved bonus values (filled by the entry constructor
//!   FUN_140e6e260(out, effect, scope, f32 value)), +0x28 u32 advancement stage (7 in stock data).
//!   Bundle instance (0x38, FUN_140e6ea50): +0 record, +8 turns, +0x10 vector of 0x28-byte active
//!   effects, +0x20 record+0x34, +0x24 byte, **+0x28 vector of custom 0x30 entries**: when its
//!   count is not 0, FUN_140e87a50(instance, stage) builds the active effects from it instead of
//!   the record. FUN_140e81b50 (holder container add, inside FUN_1419a2f80 for factions) deep
//!   copies both vectors; the caller destroys its instance with FUN_140e71e00(+0x28) and
//!   FUN_1402fdd40(+0x10).
//!
//! spec = "effect_key|scope_key|value;effect_key|scope_key|value;..."
//! se_effect_bundle_info(q_faction, key) -> count, dump:string | nil, msg
//! se_effect_bundle_define(q_faction, key, spec) -> ok, msg   rewrite the DB record's list (this
//!     process only; every later stock apply_effect_bundle of that key uses it, on any holder)
//! se_effect_bundle_restore(q_faction, key) -> ok, msg        put the stock list back
//! se_effect_bundle_apply_custom(q_faction, key, turns, spec) -> ok, msg   apply the bundle to the
//!     faction with a per-instance effect list (the record is left alone)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

struct Engine {
    table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    record_base: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
    effects_table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    scopes_table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    alloc: unsafe extern "C" fn(usize, u32) -> *mut c_void,
    entry_ctor: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, f32) -> *mut c_void,
    instance_ctor: unsafe extern "C" fn(*mut c_void, *mut c_void, u32) -> *mut c_void,
    instance_rebuild: unsafe extern "C" fn(*mut c_void, u32),
    faction_apply: unsafe extern "C" fn(*mut c_void, *mut c_void),
    custom_dtor: unsafe extern "C" fn(*mut c_void),
    effects_dtor: unsafe extern "C" fn(*mut c_void),
}

/// Stock (count, data) of every record rewritten by se_effect_bundle_define, keyed by record.
static ORIGINAL: Mutex<Option<HashMap<usize, (u32, usize)>>> = Mutex::new(None);

const STAGE: u32 = 7;
const MAX_EFFECTS: usize = 64;

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            table: core::mem::transmute(t.get("effect_bundles_table")),
            db_get: core::mem::transmute(t.get("db_get")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
            effects_table: core::mem::transmute(t.get("effects_table")),
            scopes_table: core::mem::transmute(t.get("effect_scopes_table")),
            alloc: core::mem::transmute(t.get("engine_alloc")),
            entry_ctor: core::mem::transmute(t.get("effect_entry_ctor")),
            instance_ctor: core::mem::transmute(t.get("bundle_instance_ctor")),
            instance_rebuild: core::mem::transmute(t.get("bundle_instance_rebuild")),
            faction_apply: core::mem::transmute(t.get("faction_bundle_apply")),
            custom_dtor: core::mem::transmute(t.get("bundle_custom_dtor")),
            effects_dtor: core::mem::transmute(t.get("bundle_effects_dtor")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_effect_bundle_info", se_effect_bundle_info);
    lua::set_global_fn(l, "se_effect_bundle_define", se_effect_bundle_define);
    lua::set_global_fn(l, "se_effect_bundle_restore", se_effect_bundle_restore);
    lua::set_global_fn(l, "se_effect_bundle_apply_custom", se_effect_bundle_apply_custom);
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
unsafe fn rf(p: usize) -> f32 {
    if readable(p, 4) { core::ptr::read_unaligned(p as *const f32) } else { 0.0 }
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

unsafe fn db_of(e: &Engine, l: *mut LuaState, faction_idx: c_int) -> Result<(usize, *mut c_void), String> {
    let faction = crate::progression::faction_from_arg(l, faction_idx)?;
    let world = rq(rq(faction + 0x288) + 0x78);
    if world == 0 || !readable(world + 0x3b38, 8) {
        return Err("could not derive the model from the faction".into());
    }
    let db = (e.db_get)((world + 0x3b38) as *mut c_void);
    if db.is_null() { return Err("db manager missing".into()); }
    Ok((faction, db))
}

unsafe fn bundle_record(e: &Engine, l: *mut LuaState, faction_idx: c_int, key: &str) -> Result<usize, String> {
    let (_, db) = db_of(e, l, faction_idx)?;
    let table = (e.table)(db);
    if table.is_null() { return Err("effect_bundles table missing".into()); }
    let rec = with_string(e, key, |sp| (e.record_base)(table, sp)) as usize;
    if rec == 0 || !readable(rec, 0x48) {
        return Err(format!("no effect_bundles record named '{key}'"));
    }
    Ok(rec)
}

unsafe extern "C" fn se_effect_bundle_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    let result: Result<(u32, String), String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() { return Err("bundle key is empty".into()); }
        let rec = bundle_record(e, l, 1, &key)?;
        let count = rd(rec + 0x3c);
        let data = rq(rec + 0x40);
        let mut s = format!("record={:#x} key='{}' count={count} data={:#x} rec+0x34={}\n", rec, record_key(rec), data, rd(rec + 0x34));
        for i in 0..(count as usize).min(32) {
            let en = data + i * 0x30;
            if !readable(en, 0x30) { break; }
            let words: Vec<String> = (0..6).map(|k| format!("{:016x}", rq(en + k * 8))).collect();
            s.push_str(&format!("  [{i}] effect='{}' scope='{}' value={} bonus_values={}/{} stage={}\n      {}\n", record_key(rq(en)), record_key(rq(en + 8)), rf(en + 0x10), rd(en + 0x1c), rd(en + 0x18), rd(en + 0x28), words.join(" ")));
        }
        Ok((count, s))
    })();
    match result {
        Ok((count, s)) => {
            log!("se_effect_bundle_info:\n{s}");
            (api.pushinteger)(l, count as isize);
            lua::push_str(l, &s);
            2
        }
        Err(e) => {
            log!("se_effect_bundle_info: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

/// Parse "effect|scope|value;..." into (effect record, scope record, value) triples.
unsafe fn parse_spec(e: &Engine, db: *mut c_void, spec: &str) -> Result<Vec<(usize, usize, f32)>, String> {
    let effects = (e.effects_table)(db);
    let scopes = (e.scopes_table)(db);
    if effects.is_null() || scopes.is_null() { return Err("effects / campaign_effect_scopes table missing".into()); }
    let mut out = Vec::new();
    for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let f: Vec<&str> = part.split('|').map(str::trim).collect();
        if f.len() != 3 { return Err(format!("'{part}' is not effect|scope|value")); }
        let value: f32 = f[2].parse().map_err(|_| format!("'{}' is not a number", f[2]))?;
        if !value.is_finite() { return Err(format!("value '{}' is not finite", f[2])); }
        let er = with_string(e, f[0], |sp| (e.record_base)(effects, sp)) as usize;
        if er == 0 || !readable(er, 0x60) { return Err(format!("no effects record named '{}'", f[0])); }
        let sr = with_string(e, f[1], |sp| (e.record_base)(scopes, sp)) as usize;
        if sr == 0 || !readable(sr, 0x20) { return Err(format!("no campaign_effect_scopes record named '{}'", f[1])); }
        out.push((er, sr, value));
    }
    if out.is_empty() { return Err("the effect list is empty".into()); }
    if out.len() > MAX_EFFECTS { return Err(format!("more than {MAX_EFFECTS} effects")); }
    Ok(out)
}

/// Engine-allocated array of 0x30-byte entries built by the engine's own entry constructor.
unsafe fn build_entries(e: &Engine, list: &[(usize, usize, f32)]) -> Result<usize, String> {
    let bytes = list.len() * 0x30;
    let data = (e.alloc)(bytes, 0) as usize;
    if data == 0 { return Err("engine allocator returned null".into()); }
    core::ptr::write_bytes(data as *mut u8, 0, bytes);
    for (i, (er, sr, v)) in list.iter().enumerate() {
        let en = data + i * 0x30;
        (e.entry_ctor)(en as *mut c_void, *er as *mut c_void, *sr as *mut c_void, *v);
        core::ptr::write_unaligned((en + 0x28) as *mut u32, STAGE);
    }
    Ok(data)
}

/// Six Lua natives are called apply_effect_bundle, one per holder type; they differ in where the
/// world handle sits on `*(script_obj+0x18)` and in the holder routine they call:
///   FUN_14159a380 character (+0x250): FUN_141a36fa0(FUN_141a34960(character), w)
///   FUN_14159a650 **faction (+0x288): FUN_1419a2f80(faction, w), instance list at FACTION+0xc78**
///   FUN_14159a4f0 (+0xa0) FUN_141902e40, FUN_14159a900 (+0xa0) FUN_14140c700,
///   FUN_14159a7b0 (+0x58) FUN_141774210, FUN_14159aa60 (+0x58) FUN_141902ea0 (list at +0x1c0)
/// 0.23.0 - 0.23.2 used the last one on a FACTION, which is why they crashed or refused.
const OFF_FACTION_BUNDLES: usize = 0xc78;

unsafe fn check_faction_bundles(faction: usize) -> Result<u32, String> {
    let (base, size) = crate::process::main_module();
    let list = faction + OFF_FACTION_BUNDLES;
    if !readable(list, 0x10) { return Err("faction bundle list is not readable".into()); }
    let (cap, count, data) = (rd(list) as usize, rd(list + 4) as usize, rq(list + 8));
    let empty = cap == 0 && count == 0 && data == 0;
    if !empty && (cap == 0 || cap > 4096 || count > cap || !readable(data, cap * 0x38) || (data >= base && data < base + size)) {
        return Err(format!("faction+{:#x} is not a sane bundle vector (cap {cap}, count {count}, data {:#x})", OFF_FACTION_BUNDLES, data));
    }
    for i in 0..count {
        let rec = rq(data + i * 0x38);
        if record_key(rec).is_empty() {
            return Err(format!("bundle instance {i} of {count} has no readable record key (record {:#x})", rec));
        }
    }
    Ok(count as u32)
}

unsafe fn bool_result(l: *mut LuaState, r: Result<String, String>, who: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match r {
        Ok(m) => { log!("{who}: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("{who}: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_effect_bundle_define(l: *mut LuaState) -> c_int {
    let key = lua::to_str(l, 2);
    let spec = lua::to_str(l, 3);
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() { return Err("bundle key is empty".into()); }
        let (_, db) = db_of(e, l, 1)?;
        let rec = bundle_record(e, l, 1, &key)?;
        let list = parse_spec(e, db, &spec)?;
        let old_count = rd(rec + 0x3c);
        let old_data = rq(rec + 0x40);
        if old_count as usize > 4096 || (old_count != 0 && !readable(old_data, 0x30)) {
            return Err(format!("record {:#x} does not look like an effect bundle (count {old_count}, data {:#x})", rec, old_data));
        }
        let data = build_entries(e, &list)?;
        let mut guard = ORIGINAL.lock().map_err(|_| "state lock poisoned")?;
        let map = guard.get_or_insert_with(HashMap::new);
        // The first definition remembers the stock list (never freed: the restore native still
        // points at it). Later definitions leak the previous custom array on purpose; it is a
        // few hundred bytes per call.
        map.entry(rec).or_insert((old_count, old_data));
        core::ptr::write_unaligned((rec + 0x40) as *mut usize, data);
        core::ptr::write_unaligned((rec + 0x3c) as *mut u32, list.len() as u32);
        Ok(format!("'{key}' record {:#x}: {old_count} effects @ {:#x} -> {} effects @ {:#x}", rec, old_data, list.len(), data))
    })();
    bool_result(l, r, "se_effect_bundle_define")
}

unsafe extern "C" fn se_effect_bundle_restore(l: *mut LuaState) -> c_int {
    let key = lua::to_str(l, 2);
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let rec = bundle_record(e, l, 1, &key)?;
        let mut guard = ORIGINAL.lock().map_err(|_| "state lock poisoned")?;
        let Some((count, data)) = guard.as_mut().and_then(|m| m.remove(&rec)) else {
            return Err(format!("'{key}' was not redefined in this session"));
        };
        core::ptr::write_unaligned((rec + 0x40) as *mut usize, data);
        core::ptr::write_unaligned((rec + 0x3c) as *mut u32, count);
        Ok(format!("'{key}' record {:#x}: stock list restored ({count} effects @ {:#x})", rec, data))
    })();
    bool_result(l, r, "se_effect_bundle_restore")
}

unsafe extern "C" fn se_effect_bundle_apply_custom(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    let turns = (api.tointeger)(l, 3) as i64;
    let spec = lua::to_str(l, 4);
    let r: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() { return Err("bundle key is empty".into()); }
        if !(0..=10000).contains(&turns) { return Err(format!("turns {turns} is out of range (0 = permanent)")); }
        let (faction, db) = db_of(e, l, 1)?;
        let before = check_faction_bundles(faction)?;
        let rec = bundle_record(e, l, 1, &key)?;
        let list = parse_spec(e, db, &spec)?;
        let mut w = [0u64; 8];
        let wp = w.as_mut_ptr() as usize;
        (e.instance_ctor)(wp as *mut c_void, rec as *mut c_void, turns as u32);
        if rd(wp + 0x2c) != 0 || rq(wp + 0x30) != 0 || rq(wp) != rec {
            (e.effects_dtor)((wp + 0x10) as *mut c_void);
            return Err("fresh bundle instance does not have the expected layout; nothing applied".into());
        }
        let data = build_entries(e, &list)?;
        core::ptr::write_unaligned((wp + 0x28) as *mut u32, list.len() as u32);
        core::ptr::write_unaligned((wp + 0x2c) as *mut u32, list.len() as u32);
        core::ptr::write_unaligned((wp + 0x30) as *mut usize, data);
        (e.instance_rebuild)(wp as *mut c_void, 0);
        let active = rd(wp + 0x14);
        (e.faction_apply)(faction as *mut c_void, wp as *mut c_void);
        let after = rd(faction + OFF_FACTION_BUNDLES + 4);
        (e.custom_dtor)((wp + 0x28) as *mut c_void);
        (e.effects_dtor)((wp + 0x10) as *mut c_void);
        Ok(format!("'{key}' applied to faction {:#x} for {turns} turns with {} custom effects ({active} active after rebuild; faction bundles {before} -> {after})", faction, list.len()))
    })();
    bool_result(l, r, "se_effect_bundle_apply_custom")
}
