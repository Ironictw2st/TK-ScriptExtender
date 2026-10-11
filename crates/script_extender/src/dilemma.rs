//! Dilemma choices: the real choice key behind `DilemmaChoiceMadeEvent:choice()` (build 1.7.2.0).
//!
//! `context:choice()` (FUN_1414a1f30) returns `*(u32*)(event+0x90)`: the position of the
//! clicked choice in the generated dilemma's OWN choice list, which is not in FIRST..FOURTH
//! order (seen: SECOND,FIRST,FOURTH,THIRD and FIRST,THIRD,SECOND). The record's key is not
//! exposed to Lua.
//!
//! CCQ_MAKE_DILEMMA_CHOICE (handler FUN_141b42f60) -> FUN_14144c680(pending, index): looks up
//! the generated dilemma instance I, stores the index at I+0x48 and fires the event on its own
//! stack frame:
//!   event vtable  = exe+0x3484938 (slot 2 returns "DilemmaChoiceMadeEvent")
//!   event+0x28    = copy of I (FUN_141475ec0); I+0 = dilemma record (record+8 = key string)
//!   event+0x64    = choice count, event+0x68 = choice entry pointers (copy of I+0x3c / I+0x40)
//!   event+0x90    = chosen index (= choice())
//!   entry+0x30    = choice key enum: 0 FIRST, 1 SECOND, 2 THIRD, 3 FOURTH (FUN_140ca2b20); the
//!                   dilemma panel matches its CcoCdirEventsDilemmaChoiceDetailRecord ids on it
//! The script context is a userdata whose block holds the event pointer (FUN_141601f80).
//! The event lives on the engine's stack: the context is only valid inside the listener call.
//!
//! se_dilemma_choice(context) -> key:string, index:int, order:{keys in engine order} | nil, msg
//!   Read only; the data is model state, the same on every client.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState, LUA_TLIGHTUSERDATA, LUA_TUSERDATA};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

const OFF_COUNT: usize = 0x64;
const OFF_ENTRIES: usize = 0x68;
const OFF_CHOICE: usize = 0x90;
const OFF_ENTRY_KEY: usize = 0x30;
const KEYS: [&str; 4] = ["FIRST", "SECOND", "THIRD", "FOURTH"];

static EVENT_VTABLE: OnceLock<usize> = OnceLock::new();

pub fn init(t: &Table) {
    let _ = EVENT_VTABLE.set(t.get("dilemma_choice_event_vtable"));
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_dilemma_choice", se_dilemma_choice);
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
unsafe fn rb(p: usize) -> Option<u8> {
    if readable(p, 1) { Some(core::ptr::read_unaligned(p as *const u8)) } else { None }
}

unsafe fn event_from_arg(l: *mut LuaState, idx: c_int) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let vt = *EVENT_VTABLE.get().ok_or("dilemma event vtable not verified")?;
    let ty = (api.type_)(l, idx);
    if ty != LUA_TLIGHTUSERDATA && ty != LUA_TUSERDATA {
        return Err(format!("arg {idx} is not an event context (lua type {ty})"));
    }
    let p = (api.touserdata)(l, idx) as usize;
    for c in [rq(p), p, rq(p + 8)] {
        if c != 0 && rq(c) == vt {
            return Ok(c);
        }
    }
    Err("not a DilemmaChoiceMadeEvent context (call it inside the listener)".into())
}

/// (chosen key, chosen index, keys in engine order)
unsafe fn read_choice(ev: usize) -> Result<(&'static str, u32, Vec<&'static str>), String> {
    let count = rd(ev + OFF_COUNT);
    let entries = rq(ev + OFF_ENTRIES);
    let chosen = rd(ev + OFF_CHOICE);
    if count == 0 || count > 16 || !readable(entries, count as usize * 8) {
        return Err(format!("unexpected choice list (count {count}, data {entries:#x})"));
    }
    let mut order = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let e = rq(entries + i * 8);
        let k = if e != 0 { rb(e + OFF_ENTRY_KEY) } else { None };
        match k {
            Some(k) if (k as usize) < KEYS.len() => order.push(KEYS[k as usize]),
            other => return Err(format!("choice entry {i} ({e:#x}) has no valid key ({other:?})")),
        }
    }
    let key = *order
        .get(chosen as usize)
        .ok_or_else(|| format!("choice() {chosen} outside the list of {count}"))?;
    Ok((key, chosen, order))
}

unsafe extern "C" fn se_dilemma_choice(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match event_from_arg(l, 1).and_then(|ev| read_choice(ev)) {
        Ok((key, idx, order)) => {
            lua::push_str(l, key);
            (api.pushinteger)(l, idx as isize);
            (api.createtable)(l, order.len() as c_int, 0);
            for (i, k) in order.iter().enumerate() {
                lua::push_str(l, k);
                (api.rawseti)(l, -2, i as c_int + 1);
            }
            3
        }
        Err(e) => {
            log!("se_dilemma_choice: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}
