//! Diplomatic alliances / coalitions: name read and rename (build 1.7.2.0).
//!
//!   QUERY_DIPLOMACY_ALLIANCE script object -> ALLIANCE = *(obj+8) (natives FUN_141542920 etc.)
//!   ALLIANCE +8 cqi (CcoDiplomacyAlliance.CQI thunk), +0x18 participant object,
//!   name = FUN_141acf230(alliance): `*(alliance+0x60)` (UniString*) when non-null, else the
//!   inline UniString at alliance+0x68 (CcoDiplomacyAlliance.Name -> output vfunc +0x80).
//!   UniString = {u32 len, u32 cap, wchar_t* @+8}; short strings are stored inline (top nibble
//!   of the qword at +8 == 8, length in its top byte's low nibble, chars from +0).
//!   FUN_140663120(out, wchar*) builds a UniString; FUN_140663ea0(a, b) swaps two UniStrings.
//!
//! se_alliance_info(q_alliance, cqi) -> name:string, info:string | nil, msg
//! se_alliance_name_set(q_alliance, cqi, text, mode) -> ok, msg
//!     mode "inline" (default): clear +0x60 and swap the new text into +0x68
//!     mode "pointer": point +0x60 at a new UniString (leaves +0x68 alone)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

const OFF_CQI: usize = 0x8;
const OFF_NAME_PTR: usize = 0x60;
const OFF_NAME_INLINE: usize = 0x68;

struct Engine {
    from_wcstr: unsafe extern "C" fn(*mut c_void, *const u16) -> *mut c_void,
    swap: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            from_wcstr: core::mem::transmute(t.get("unistring_from_wcstr")),
            swap: core::mem::transmute(t.get("unistring_assign")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_alliance_info", se_alliance_info);
    lua::set_global_fn(l, "se_alliance_name_set", se_alliance_name_set);
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

/// Decode a UniString object (inline short form or heap form).
unsafe fn unistring(sp: usize) -> String {
    if !readable(sp, 16) {
        return String::new();
    }
    let tail = rq(sp + 8);
    let (len, ptr) = if (tail & 0xf000_0000_0000_0000) == 0x8000_0000_0000_0000 {
        (((tail >> 0x38) & 0xf) as usize, sp)
    } else {
        (rd(sp) as usize, tail)
    };
    if len == 0 || len > 256 || !readable(ptr, len * 2) {
        return String::new();
    }
    let w = core::slice::from_raw_parts(ptr as *const u16, len);
    String::from_utf16_lossy(w)
}

unsafe fn alliance_from_arg(l: *mut LuaState, idx: c_int, cqi: u32) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not a script object (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    let cands = [rq(rq(p) + 8), rq(rq(rq(p) + 8) + 8), rq(rq(p) + 0x18), rq(rq(rq(p) + 8) + 0x18), rq(p + 8), rq(p)];
    for c in cands {
        if c != 0 && readable(c, 0x80) && rd(c + OFF_CQI) == cqi {
            return Ok(c);
        }
    }
    Err(format!("no candidate looks like ALLIANCE cqi {cqi} (p={:#x})", p))
}

unsafe fn name_of(a: usize) -> (String, String) {
    let ptr = rq(a + OFF_NAME_PTR);
    let via_ptr = if ptr != 0 { unistring(ptr) } else { String::new() };
    let inline = unistring(a + OFF_NAME_INLINE);
    let name = if ptr != 0 { via_ptr.clone() } else { inline.clone() };
    (name, format!("alliance={:#x} cqi={} name_ptr={:#x} ('{}') inline='{}'", a, rd(a + OFF_CQI), ptr, via_ptr, inline))
}

unsafe extern "C" fn se_alliance_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match alliance_from_arg(l, 1, cqi) {
        Ok(a) => {
            let (name, info) = name_of(a);
            log!("se_alliance_info: {info}");
            lua::push_str(l, &name);
            lua::push_str(l, &info);
            2
        }
        Err(e) => {
            log!("se_alliance_info: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_alliance_name_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let text = lua::to_str(l, 3);
    let mode = lua::to_str(l, 4);
    let result: Result<String, String> = (|| {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if text.is_empty() || text.chars().count() > 64 {
            return Err("text must be 1..64 characters".into());
        }
        let a = alliance_from_arg(l, 1, cqi)?;
        let (before, info) = name_of(a);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        // A leaked 16-byte UniString built by the engine's own constructor.
        let buf: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.from_wcstr)(buf.as_mut_ptr() as *mut c_void, wide.as_ptr());
        if mode == "pointer" {
            core::ptr::write_unaligned((a + OFF_NAME_PTR) as *mut usize, buf.as_ptr() as usize);
        } else {
            core::ptr::write_unaligned((a + OFF_NAME_PTR) as *mut usize, 0);
            // swap: the inline field takes our text, our buffer keeps the old text (leaked)
            (e.swap)((a + OFF_NAME_INLINE) as *mut c_void, buf.as_mut_ptr() as *mut c_void);
        }
        let (after, _) = name_of(a);
        let msg = format!("cqi {cqi}: name '{before}' -> '{after}' ({} mode; {info})", if mode == "pointer" { "pointer" } else { "inline" });
        log!("se_alliance_name_set: {msg}");
        if after == text { Ok(msg) } else { Err(format!("name did not take: {msg}")) }
    })();
    match result {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("se_alliance_name_set: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
