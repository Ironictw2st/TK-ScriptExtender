//! Unit recruitment primitives (build 1.7.2.0), from the decompile of the
//! CCQ_CHARACTER_RETINUE_SLOT_RECRUIT_UNIT executor (FUN_141b47f00) and the retinue-slot
//! recruitment interface (vtable 0x14349c270):
//!
//!   iface->vfunc[0x11] (+0x88)(iface, &ItemVec, flag)   build the slot's item list; flag 1 =
//!                             recruitable only (what the model uses), 0 = everything with the
//!                             lock reason bits set (466 items on a stock retinue slot)
//!   iface->vfunc[0x10] (+0x80)(iface, item) -> u8       start recruiting an item; refuses if the
//!                             interface's reasons (vfunc +0x58) or the item's reasons are non-zero,
//!                             checks/deducts cost, creates the in-progress record, fires events
//!   item (0x98 bytes, vtable 0x14349c220): vfunc[1] -> &info {record*, u32 cost @+8, u32 turns
//!                             @+0xc}, vfunc[2] -> &reasons (u32), vfunc[0](item, 1) deletes it
//!   unit record for a key: record_base(land_units_table(db_get(model+0x3b38)), &String(key)),
//!                             exactly as the executor does; model = *(*(char+0x250)+0x78) with
//!                             char = *(*(iface+8)+0x48)+0x48 (the slot's commanding character).
//!
//! se_slot_items(q)                     -> string: every item with key (best effort), cost, turns,
//!                                         reasons; the first record is hex-dumped for layout work
//! se_recruit_unit(q, unit_key, mode)   -> ok, message. mode bit 1 = clear the item's reasons
//!                                         first (force), bit 2 = zero its cost (free).
//!                                         unit_key "" = the engine's disband (null-record item).
//! `q` is a QUERY_PERSISTENT_RETINUE_SLOT_RECRUITMENT script object.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

struct Engine {
    disband_units: unsafe extern "C" fn(*mut ItemVec, *mut c_void) -> u8,
    unit_is_valid: unsafe extern "C" fn(*mut c_void) -> u8,
    free: unsafe extern "C" fn(*mut c_void),
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    land_units_table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    record_base: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
    iface_vtable: usize,
    item_vtable: usize,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            disband_units: core::mem::transmute(t.get("disband_units")),
            unit_is_valid: core::mem::transmute(t.get("unit_is_valid")),
            free: core::mem::transmute(t.get("engine_free")),
            db_get: core::mem::transmute(t.get("db_get")),
            land_units_table: core::mem::transmute(t.get("land_units_table")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
            iface_vtable: t.get("recruit_iface_vtable"),
            item_vtable: t.get("recruit_item_vtable"),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_slot_items", se_slot_items);
    lua::set_global_fn(l, "se_recruit_unit", se_recruit_unit);
    lua::set_global_fn(l, "se_unit_info", se_unit_info);
    lua::set_global_fn(l, "se_disband_unit", se_disband_unit);
}

/// Engine UNIT from a QUERY_UNIT script object (see units.rs); `how` kept for the log format.
unsafe fn unit_from_arg(_e: &Engine, l: *mut LuaState, idx: c_int) -> Result<(usize, &'static str), String> {
    crate::units::unit_from_arg(l, idx).map(|u| (u, "vtable"))
}

/// Model pointer from a CHARACTER script object + cqi cross-check (see pool.rs layout).
unsafe fn model_from_character_arg(l: *mut LuaState, idx: c_int, cqi: u32) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let p = (api.touserdata)(l, idx) as usize;
    let ch = rq(rq(rq(p) + 8) + 0x18);
    if ch == 0 || !readable(ch, 0x300) || rd(ch + 0x240) != cqi {
        return Err(format!("character arg does not match cqi {cqi}"));
    }
    let model = rq(rq(ch + 0x250) + 0x78);
    if model == 0 || !readable(model + 0x3c18, 8) {
        return Err("could not derive the model from the character".into());
    }
    Ok(model)
}

/// se_unit_info(q_unit) -> string   (diagnostic: pointer, presumed cqi, force, validity)
unsafe extern "C" fn se_unit_info(l: *mut LuaState) -> c_int {
    let out: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let (u, how) = unit_from_arg(e, l, 1)?;
        Ok(format!("unit={:#x} via {how} id@+8={} faction@+90={:#x} men={}/{} valid_flag={} vt={:#x}", u, rd(u + 8), rq(u + 0x90), rd(u + 0xac), rd(u + 0xa8), (e.unit_is_valid)(u as *mut c_void), rq(u)))
    })();
    match out {
        Ok(s) => { log!("se_unit_info: {s}"); lua::push_str(l, &s); }
        Err(er) => { log!("se_unit_info: {er}"); lua::push_str(l, &format!("error: {er}")); }
    }
    1
}

/// se_disband_unit(q_unit, q_character, char_cqi) -> ok:boolean, message:string
/// Calls the engine routine behind CCQ_DISBAND_UNIT with a one-element unit vector.
unsafe extern "C" fn se_disband_unit(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 3) as u32;
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let (u, how) = unit_from_arg(e, l, 1)?;
        let model = model_from_character_arg(l, 2, cqi)?;
        log!("se_disband_unit: unit={:#x} (via {how}, id@+8={}) faction@+90={:#x} model={:#x}", u, rd(u + 8), rq(u + 0x90), model);
        let mut ptrs: [usize; 1] = [u];
        let mut vec = ItemVec { cap: 1, count: 1, data: ptrs.as_mut_ptr() as *mut *mut c_void };
        let r = (e.disband_units)(&mut vec, model as *mut c_void);
        Ok(format!("disband_units returned {r} for unit {:#x} (id {})", u, rd(u + 8)))
    })();
    match result {
        Ok(m) => { log!("se_disband_unit: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_disband_unit: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
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

#[repr(C)]
struct ItemVec {
    cap: u32,
    count: u32,
    data: *mut *mut c_void,
}

type VfBuildList = unsafe extern "C" fn(*mut c_void, *mut ItemVec, u8);
type VfStart = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8;
type VfThis = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type VfBool = unsafe extern "C" fn(*mut c_void) -> u8;
type VfDelete = unsafe extern "C" fn(*mut c_void, u32);

unsafe fn vf(obj: usize, slot: usize) -> usize {
    rq(rq(obj) + slot * 8)
}

unsafe fn object_from_arg(l: *mut LuaState, idx: c_int, want_vtable: usize) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    let cands = [
        ("p", p),
        ("*p", rq(p)),
        ("*(p+8)+0x18", rq(rq(p + 8) + 0x18)),
        ("*(*p+8)+0x18", rq(rq(rq(p) + 8) + 0x18)),
        ("*(p+0x18)", rq(p + 0x18)),
        ("*(*p+0x18)", rq(rq(p) + 0x18)),
    ];
    for (how, c) in cands.iter() {
        if *c != 0 && readable(*c, 0x40) && rq(*c) == want_vtable {
            log!("object_from_arg: matched via {how} -> {:#x}", c);
            return Ok(*c);
        }
    }
    let dump: Vec<String> = cands.iter().map(|(h, c)| format!("{h}={:#x}(vt={:#x})", c, if *c != 0 && readable(*c, 8) { rq(*c) } else { 0 })).collect();
    Err(format!("no candidate has vtable {:#x}: {}", want_vtable, dump.join(" ")))
}

/// Model pointer reachable from the recruitment interface (see module doc).
unsafe fn model_from_iface(iface: usize) -> usize {
    // FUN_141934050: FUN_141a5c200(*(*(iface[1] + 0x48) + 0x48)) -> the argument is a CHARACTER
    let holder = rq(rq(iface + 8) + 0x48);
    let character = rq(holder + 0x48);
    if character == 0 || !readable(character, 0x300) {
        return 0;
    }
    rq(rq(character + 0x250) + 0x78)
}

/// Look the unit record up the way the executor does. Returns null when unknown.
unsafe fn unit_record(e: &Engine, model: usize, key: &str) -> usize {
    if model == 0 || !readable(model + 0x3b38, 8) {
        return 0;
    }
    let db = (e.db_get)((model + 0x3b38) as *mut c_void);
    if db.is_null() {
        return 0;
    }
    let table = (e.land_units_table)(db);
    if table.is_null() {
        return 0;
    }
    let mut ckey = key.as_bytes().to_vec();
    ckey.push(0);
    let mut sbuf = [0u8; 64]; // CA::String is much smaller; keep slack
    (e.string_from_cstr)(sbuf.as_mut_ptr() as *mut c_void, ckey.as_ptr() as *const c_char);
    let rec = (e.record_base)(table, sbuf.as_ptr() as *const c_void) as usize;
    (e.string_dtor)(sbuf.as_mut_ptr() as *mut c_void);
    rec
}

fn ascii(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 3 || bytes.len() > 96 || !bytes.iter().all(|c| (0x20..0x7f).contains(c)) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Best-effort key of a DB record: try a few plausible layouts, return "?" otherwise.
pub(crate) unsafe fn record_key(rec: usize) -> String {
    if rec == 0 || !readable(rec, 0x30) {
        return "?".into();
    }
    // (a) verified live on 1.7.2.0: record+8 -> CA::String* {u32 len, u32 cap, char* @+8}
    let sp = rq(rec + 8);
    if readable(sp, 16) {
        let len = rd(sp) as usize;
        let ptr = rq(sp + 8);
        if len > 0 && len <= 96 && readable(ptr, len) {
            if let Some(s) = ascii(core::slice::from_raw_parts(ptr as *const u8, len)) { return s; }
        }
    }
    // (b) char* at +8 / +0x10 / +0x18
    for off in [8usize, 0x10, 0x18] {
        let p = rq(rec + off);
        if readable(p, 64) {
            let bytes = core::slice::from_raw_parts(p as *const u8, 64);
            let n = bytes.iter().position(|&c| c == 0).unwrap_or(64);
            if let Some(s) = ascii(&bytes[..n]) { return s; }
        }
    }
    // (c) inline chars at +8
    if readable(rec + 8, 64) {
        let bytes = core::slice::from_raw_parts((rec + 8) as *const u8, 64);
        let n = bytes.iter().position(|&c| c == 0).unwrap_or(64);
        if let Some(s) = ascii(&bytes[..n]) { return s; }
    }
    "?".into()
}

unsafe fn hexdump(p: usize, n: usize) -> String {
    if !readable(p, n) {
        return "<unreadable>".into();
    }
    core::slice::from_raw_parts(p as *const u8, n).iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

struct Item {
    ptr: usize,
    record: usize,
    key: String,
    cost: u32,
    turns: u32,
    reasons_ptr: usize,
    reasons: u32,
}

unsafe fn describe_item(it: usize) -> Item {
    let info = (core::mem::transmute::<usize, VfThis>(vf(it, 1)))(it as *mut c_void) as usize;
    let reasons_ptr = (core::mem::transmute::<usize, VfThis>(vf(it, 2)))(it as *mut c_void) as usize;
    let record = rq(info);
    Item { ptr: it, record, key: record_key(record), cost: rd(info + 8), turns: rd(info + 0xc), reasons_ptr, reasons: rd(reasons_ptr) }
}

unsafe fn build_items(e: &Engine, iface: usize, include_locked: bool) -> (ItemVec, Vec<Item>) {
    let mut vec = ItemVec { cap: 0, count: 0, data: core::ptr::null_mut() };
    let f: VfBuildList = core::mem::transmute(vf(iface, 0x11));
    f(iface as *mut c_void, &mut vec, if include_locked { 0 } else { 1 });
    let mut items = Vec::new();
    for i in 0..vec.count as usize {
        let it = rq(vec.data as usize + i * 8);
        if it != 0 && readable(it, 0x98) && rq(it) == e.item_vtable {
            items.push(describe_item(it));
        }
    }
    (vec, items)
}

unsafe fn free_items(e: &Engine, vec: &ItemVec) {
    for i in 0..vec.count as usize {
        let it = rq(vec.data as usize + i * 8);
        if it != 0 && readable(it, 8) {
            (core::mem::transmute::<usize, VfDelete>(vf(it, 0)))(it as *mut c_void, 1);
        }
    }
    if !vec.data.is_null() {
        (e.free)(vec.data as *mut c_void);
    }
}

/// se_slot_items(q) -> string
unsafe extern "C" fn se_slot_items(l: *mut LuaState) -> c_int {
    let out: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let iface = object_from_arg(l, 1, e.iface_vtable)?;
        let can: VfBool = core::mem::transmute(vf(iface, 6));
        let model = model_from_iface(iface);
        let (vec, items) = build_items(e, iface, true);
        let mut s = format!("iface={:#x} model={:#x} can_recruit={} items={}\n", iface, model, can(iface as *mut c_void), items.len());
        if let Some(first) = items.first() {
            s.push_str(&format!("  first record {:#x}: {}\n", first.record, hexdump(first.record, 0x40)));
        }
        let mut unlocked = 0;
        for it in &items {
            if it.reasons == 0 { unlocked += 1; }
            s.push_str(&format!("  {} rec={:#x} cost={} turns={} reasons={:#x}\n", it.key, it.record, it.cost, it.turns, it.reasons));
        }
        s.push_str(&format!("  ({unlocked} recruitable now)\n"));
        free_items(e, &vec);
        Ok(s)
    })();
    match out {
        Ok(s) => { log!("se_slot_items:\n{s}"); lua::push_str(l, &s); }
        Err(er) => { log!("se_slot_items: {er}"); lua::push_str(l, &format!("error: {er}")); }
    }
    1
}

/// se_recruit_unit(q, unit_key, mode) -> ok:boolean, message:string
unsafe extern "C" fn se_recruit_unit(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let mut klen: usize = 0;
    let kptr = (api.tolstring)(l, 2, &mut klen);
    let key = if kptr.is_null() { String::new() } else { String::from_utf8_lossy(core::slice::from_raw_parts(kptr as *const u8, klen)).into_owned() };
    let mode = (api.tointeger)(l, 3) as u32;
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        // An empty key is the engine's own "disband": the UI DisbandUnit handler (FUN_143010da0)
        // issues the same slot command with a null unit record and the executor (FUN_141b47f00)
        // starts the slot's null-record item, which empties the slot.
        let disband = key.is_empty();
        let iface = object_from_arg(l, 1, e.iface_vtable)?;
        let can: VfBool = core::mem::transmute(vf(iface, 6));
        let can_now = can(iface as *mut c_void);
        let model = model_from_iface(iface);
        let want = if disband { 0 } else { unit_record(e, model, &key) };
        log!("se_recruit_unit: model={:#x} record for '{}' = {:#x}", model, key, want);
        if want == 0 && !disband {
            return Err(format!("no land_units record named '{key}' (model {:#x})", model));
        }
        let (vec, items) = build_items(e, iface, true);
        let found = items.iter().find(|it| it.record == want);
        let outcome = match found {
            None => {
                if disband {
                    return Err(format!("this slot's {} items contain no empty-slot (null record) entry", items.len()));
                }
                let keys: Vec<String> = items.iter().filter(|i| i.key != "?").map(|i| i.key.clone()).collect();
                Err(format!("'{key}' (record {:#x}) is not among this slot's {} items; known keys: {}", want, items.len(), keys.join(", ")))
            }
            Some(it) => {
                log!("se_recruit_unit: iface can_recruit={} item {} rec={:#x} cost={} turns={} reasons={:#x} mode={mode}", can_now, it.key, it.record, it.cost, it.turns, it.reasons);
                if mode & 1 != 0 && it.reasons != 0 && readable(it.reasons_ptr, 4) {
                    core::ptr::write_unaligned(it.reasons_ptr as *mut u32, 0);
                }
                if mode & 2 != 0 && it.cost != 0 {
                    let info = (core::mem::transmute::<usize, VfThis>(vf(it.ptr, 1)))(it.ptr as *mut c_void) as usize;
                    core::ptr::write_unaligned((info + 8) as *mut u32, 0);
                }
                let start: VfStart = core::mem::transmute(vf(iface, 0x10));
                let r = start(iface as *mut c_void, it.ptr as *mut c_void);
                let what = if disband { "<empty slot / disband>".to_string() } else { key.clone() };
                if r != 0 {
                    Ok(format!("recruiting {what} (cost {}, turns {}, reasons were {:#x})", if mode & 2 != 0 { 0 } else { it.cost }, it.turns, it.reasons))
                } else {
                    Err(format!("engine declined to start recruiting {what} (iface can_recruit={}, reasons {:#x}, cost {})", can_now, it.reasons, it.cost))
                }
            }
        };
        free_items(e, &vec);
        outcome
    })();
    match result {
        Ok(m) => { log!("se_recruit_unit: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_recruit_unit: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}
