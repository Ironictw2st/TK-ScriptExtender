//! 190E "Create your Character / Faction" spikes (build 1.7.2.0). Single-player only: the Lua
//! wrappers refuse in multiplayer; the natives themselves never branch on the local machine.
//!
//! S2 character names (notes/creator_spikes.md):
//!   CCQ_SET_CHARACTER_CUSTOM_NAME executor FUN_141827500(cmd, model):
//!     character = read tag 0x80 cqi (FUN_14154a980), part = u32, text = UniString
//!     FUN_14188c4a0(&name)               name object ctor (0x30 bytes, see below)
//!     FUN_1418c2da0(&name, &text)        name.text = text, name.id = 0
//!     FUN_141a6d840(character, part, &name, 2)   set name part, priority 2 = custom
//!     (*(model+0x11d0))->vfunc+0x10(model+0x11d0, &character)   "name changed" notification
//!   CHARACTER +0x2b8: 4 slots of 0x30 {String key @0, u64 id @0x10, UniString text @0x18,
//!     i32 part @0x28, i32 priority @0x2c}; a slot with priority 2 only takes another priority-2
//!     write. CHARACTER +0x440: display cache, part 0 @+0, 1 @+0x50, 2 @+0x28, 3 @+0x78
//!     (0x28 each: String key, u64 id, UniString text), written by FUN_141a84bf0.
//!
//! se_char_names(q_char, cqi) -> string            diagnostic dump of the slots and the cache
//! se_char_name_set(q_char, cqi, part, text) -> ok, msg

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

type P = *mut c_void;

struct Engine {
    from_wcstr: unsafe extern "C" fn(P, *const u16) -> P,
    name_ctor: unsafe extern "C" fn(P) -> P,
    name_set_text: unsafe extern "C" fn(P, P) -> P,
    set_name_part: unsafe extern "C" fn(P, i32, P, i32),
    module_lo: usize,
    module_hi: usize,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

const OFF_CQI: usize = 0x240;
const OFF_SLOTS: usize = 0x2b8;
const OFF_CACHE: usize = 0x440;
const OFF_MODEL_NOTIFY: usize = 0x11d0;

pub fn init(t: &Table) {
    let base = t.base();
    let e = unsafe {
        Engine {
            from_wcstr: core::mem::transmute(t.get("unistring_from_wcstr")),
            name_ctor: core::mem::transmute(t.get("char_name_ctor")),
            name_set_text: core::mem::transmute(t.get("char_name_set_text")),
            set_name_part: core::mem::transmute(t.get("char_set_name_part")),
            module_lo: base,
            module_hi: base + 0x4836000,
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_char_names", se_char_names);
    lua::set_global_fn(l, "se_char_name_set", se_char_name_set);
}

#[link(name = "kernel32")]
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

/// Engine string object {u32 len, u32 cap, ptr @+8}; short strings inline (top nibble of +8 = 8,
/// length in the low nibble of its top byte). `wide` = UniString (u16), else narrow (u8).
unsafe fn string_at(sp: usize, wide: bool) -> String {
    if !readable(sp, 16) {
        return "<unreadable>".into();
    }
    let tail = rq(sp + 8);
    let (len, ptr) = if (tail & 0xf000_0000_0000_0000) == 0x8000_0000_0000_0000 {
        (((tail >> 0x38) & 0xf) as usize, sp)
    } else {
        (rd(sp) as usize, tail)
    };
    let unit = if wide { 2 } else { 1 };
    if len == 0 {
        return String::new();
    }
    if len > 512 || !readable(ptr, len * unit) {
        return format!("<len {len} ptr {ptr:#x}>");
    }
    if wide {
        String::from_utf16_lossy(core::slice::from_raw_parts(ptr as *const u16, len))
    } else {
        String::from_utf8_lossy(core::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
    }
}

/// Model pointer the way recruit.rs derives it, checked by the notifier's vtable living in the exe.
unsafe fn model_of(e: &Engine, ch: usize) -> Result<usize, String> {
    let model = rq(rq(ch + 0x250) + 0x78);
    if model == 0 || !readable(model + 0x3c18, 8) {
        return Err("could not derive the model from the character".into());
    }
    let vt = rq(model + OFF_MODEL_NOTIFY);
    let f = rq(vt + 0x10);
    if vt < e.module_lo || vt >= e.module_hi || f < e.module_lo || f >= e.module_hi {
        return Err(format!("model+0x11d0 does not look like the notifier (vt {vt:#x}, fn {f:#x})"));
    }
    Ok(model)
}

unsafe fn character(l: *mut LuaState, cqi: u32) -> Result<usize, String> {
    let ch = crate::pool::character_from_arg(l, 1, cqi)?;
    if !readable(ch, OFF_CACHE + 0xa0) || rd(ch + OFF_CQI) != cqi {
        return Err(format!("character {cqi} at {ch:#x} is not readable up to the name cache"));
    }
    Ok(ch)
}

unsafe fn dump(ch: usize) -> String {
    let mut s = String::new();
    for i in 0..4 {
        let e = ch + OFF_SLOTS + i * 0x30;
        s.push_str(&format!(
            "slot{i}{{part={} prio={} key='{}' id={:#x} text='{}'}} ",
            rd(e + 0x28) as i32, rd(e + 0x2c) as i32, string_at(e, false), rq(e + 0x10), string_at(e + 0x18, true)
        ));
    }
    for (part, off) in [(0usize, 0usize), (1, 0x50), (2, 0x28), (3, 0x78)] {
        let e = ch + OFF_CACHE + off;
        s.push_str(&format!("cache{part}{{key='{}' id={:#x} text='{}'}} ", string_at(e, false), rq(e + 0x10), string_at(e + 0x18, true)));
    }
    s
}

/// se_char_names(q_char, cqi) -> string
unsafe extern "C" fn se_char_names(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let out = (|| -> Result<String, String> {
        let ch = character(l, cqi)?;
        Ok(format!("character {cqi} @ {ch:#x}: {}", dump(ch)))
    })();
    match out {
        Ok(s) => { log!("se_char_names: {s}"); lua::push_str(l, &s); }
        Err(e) => { log!("se_char_names: {e}"); lua::push_str(l, &format!("error: {e}")); }
    }
    1
}

/// se_char_name_set(q_char, cqi, part, text) -> ok, msg
/// Does what CCQ_SET_CHARACTER_CUSTOM_NAME does, from inside a model callback.
unsafe extern "C" fn se_char_name_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let part = (api.tointeger)(l, 3) as i32;
    let text = lua::to_str(l, 4);
    let out = (|| -> Result<String, String> {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if !(0..=3).contains(&part) {
            return Err(format!("part must be 0..3 (got {part})"));
        }
        if text.chars().count() > 48 || text.chars().any(|c| c.is_control()) {
            return Err("text must be at most 48 characters, no control characters".into());
        }
        let ch = character(l, cqi)?;
        let model = model_of(e, ch)?;
        let before = dump(ch);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        // Both objects are leaked on purpose (a few dozen bytes per rename): the engine copies
        // out of them, and freeing them here would mean matching the engine's allocator rules.
        let src: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.from_wcstr)(src.as_mut_ptr() as P, wide.as_ptr());
        let name: &'static mut [u8; 0x40] = Box::leak(Box::new([0u8; 0x40]));
        (e.name_ctor)(name.as_mut_ptr() as P);
        (e.name_set_text)(name.as_mut_ptr() as P, src.as_mut_ptr() as P);
        (e.set_name_part)(ch as P, part, name.as_mut_ptr() as P, 2);
        let notifier = model + OFF_MODEL_NOTIFY;
        let f: unsafe extern "C" fn(usize, *const usize) -> u64 = core::mem::transmute(rq(rq(notifier) + 0x10));
        let arg = ch;
        f(notifier, &arg);
        let after = dump(ch);
        log!("se_char_name_set: cqi {cqi} part {part} '{text}'\n  before: {before}\n  after:  {after}");
        Ok(format!("cqi {cqi} part {part} -> '{text}'"))
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_char_name_set: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}
