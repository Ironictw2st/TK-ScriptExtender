//! Effect bundles: inspect and (later) redefine a bundle's effect list at runtime.
//!
//!   effect_bundles table = FUN_140911650(db_get(world+0x3b38)); record = record_base(table, key)
//!   EFFECT_BUNDLE record: +0x3c effect count, +0x40 -> entries (0x30 bytes each; from the
//!   potential-handicap apply and FUN_140e87a50: entry+8 effect record, entry+0x10 scope record,
//!   entry+0x28 advancement stage; value expected at +0x18, to be confirmed live)
//!   apply (faction): wrapper = FUN_140e6ea50(&w, record, turns); FUN_141902ea0(faction, &w)
//!
//! se_effect_bundle_info(q_faction, key) -> count, dump:string | nil, msg

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

struct Engine {
    table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    record_base: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void,
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            table: core::mem::transmute(t.get("effect_bundles_table")),
            db_get: core::mem::transmute(t.get("db_get")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_effect_bundle_info", se_effect_bundle_info);
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

unsafe fn bundle_record(e: &Engine, l: *mut LuaState, faction_idx: c_int, key: &str) -> Result<usize, String> {
    let faction = crate::progression::faction_from_arg(l, faction_idx)?;
    let world = rq(rq(faction + 0x288) + 0x78);
    if world == 0 || !readable(world + 0x3b38, 8) {
        return Err("could not derive the model from the faction".into());
    }
    let db = (e.db_get)((world + 0x3b38) as *mut c_void);
    if db.is_null() { return Err("db manager missing".into()); }
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
            s.push_str(&format!("  [{i}] {}\n      +8 -> '{}'  +0x10 -> '{}'  f32@+0x18={} f32@+0x1c={} f32@+0x20={} u32@+0x28={}\n", words.join(" "), record_key(rq(en + 8)), record_key(rq(en + 0x10)), rf(en + 0x18), rf(en + 0x1c), rf(en + 0x20), rd(en + 0x28)));
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
