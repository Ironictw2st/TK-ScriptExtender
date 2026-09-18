//! Character experience and assignment read primitives (build 1.7.2.0), from the natives:
//!   current_experience  FUN_141542640 -> FUN_1415b4230: *(u32*)(DETAILS + 0xcc)
//!   active_assignment   FUN_141541fb0 -> FUN_141587440: DETAILS + 0xca8 = ASSIGNMENT*
//!   assignment_record_key FUN_14159b7c0: *(ASSIGNMENT + 0x18) = record, record + 8 = key String*
//!   DETAILS = **(CHARACTER + 0x260) (FUN_141a34960)
//!
//! se_char_xp_get(q, cqi)        -> xp:int, dump:string   (dump = details 0xc0..0xe0 as words)
//! se_assignment_dump(q, cqi)    -> string  (assignment pointer, record key, 0x100-byte hexdump)
//! Both are layout-discovery aids for phases 4 and 5; the assignment region offset is chosen
//! from the live dump.

use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};

const OFF_DETAILS_XP: usize = 0xcc;
const OFF_DETAILS_ASSIGNMENT: usize = 0xca8;

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

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_char_xp_get", se_char_xp_get);
    lua::set_global_fn(l, "se_assignment_dump", se_assignment_dump);
    lua::set_global_fn(l, "se_assignment_info", se_assignment_info);
}

/// Province key of an ASSIGNMENT (live-verified 2026-09-17): the CCO ProvinceContext handler
/// FUN_142ec0e70 reads *(*(assignment+0x28)) + 0xb8 = PROVINCE; the QUERY_REGION province_name
/// native (FUN_141601e50 -> FUN_141927670) reads the key as the inline String at
/// *(*(province+0x20)) + 8.
unsafe fn assignment_province_key(a: usize) -> String {
    let handle = rq(a + 0x28);
    let target = rq(handle);
    if target == 0 || !readable(target, 0xc0) {
        return String::new();
    }
    let province = rq(target + 0xb8);
    if province == 0 || !readable(province, 0x30) {
        return String::new();
    }
    let r = rq(province + 0x20);
    ca_string(rq(r) + 8)
}

/// se_assignment_info(q, cqi) -> key:string, province_key:string, state:int, round:int | nil, msg
unsafe extern "C" fn se_assignment_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let out: Result<(String, String, u32, u32), String> = (|| {
        let d = details_of(l, cqi)?;
        let a = rq(d + OFF_DETAILS_ASSIGNMENT);
        if a == 0 || !readable(a, 0x100) {
            return Err("character has no assignment object".into());
        }
        let rec = rq(a + 0x18);
        let key = ca_string(rq(rec + 8));
        Ok((key, assignment_province_key(a), rd(a + 8) & 0xff, rd(a + 0x10)))
    })();
    match out {
        Ok((key, prov, state, round)) => {
            log!("se_assignment_info: cqi {cqi} key={key} province={prov} state={state} round={round}");
            lua::push_str(l, &key);
            lua::push_str(l, &prov);
            (api.pushinteger)(l, state as isize);
            (api.pushinteger)(l, round as isize);
            4
        }
        Err(e) => {
            log!("se_assignment_info: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
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

unsafe fn details_of(l: *mut LuaState, cqi: u32) -> Result<usize, String> {
    let ch = crate::pool::character_from_arg(l, 1, cqi)?;
    let d = rq(rq(ch + 0x260));
    if d == 0 || !readable(d, 0xd00) {
        return Err("character has no details object".into());
    }
    Ok(d)
}

unsafe extern "C" fn se_char_xp_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match details_of(l, cqi) {
        Ok(d) => {
            let xp = rd(d + OFF_DETAILS_XP);
            let words: Vec<String> = (0..8).map(|i| format!("{:08x}", rd(d + 0xc0 + i * 4))).collect();
            let dump = format!("details={:#x} words@0xc0: {}", d, words.join(" "));
            log!("se_char_xp_get: cqi {cqi} xp={xp} {dump}");
            (api.pushinteger)(l, xp as isize);
            lua::push_str(l, &dump);
            2
        }
        Err(e) => {
            log!("se_char_xp_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_assignment_dump(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let out: Result<String, String> = (|| {
        let d = details_of(l, cqi)?;
        let a = rq(d + OFF_DETAILS_ASSIGNMENT);
        if a == 0 || !readable(a, 0x100) {
            return Ok(format!("details={:#x} assignment=null", d));
        }
        let rec = rq(a + 0x18);
        let key = ca_string(rq(rec + 8));
        let mut s = format!("details={:#x} assignment={:#x} record={:#x} key={key}\n", d, a, rec);
        for row in 0..16 {
            let words: Vec<String> = (0..2).map(|i| format!("{:016x}", rq(a + row * 16 + i * 8))).collect();
            s.push_str(&format!("  +{:03x}: {}\n", row * 16, words.join(" ")));
        }
        Ok(s)
    })();
    match out {
        Ok(s) => { log!("se_assignment_dump: cqi {cqi}\n{s}"); lua::push_str(l, &s); }
        Err(e) => { log!("se_assignment_dump: {e}"); lua::push_str(l, &format!("error: {e}")); }
    }
    1
}
