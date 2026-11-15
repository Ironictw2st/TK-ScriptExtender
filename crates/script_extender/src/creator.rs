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
    str_assign: unsafe extern "C" fn(P, P) -> P,
    ustr_assign: unsafe extern "C" fn(P, P) -> P,
    str_from_cstr: unsafe extern "C" fn(P, *const u8) -> P,
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
            str_assign: core::mem::transmute(t.get("string_assign")),
            ustr_assign: core::mem::transmute(t.get("unistring_assign_copy")),
            str_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            module_lo: base,
            module_hi: base + 0x4836000,
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_char_names", se_char_names);
    lua::set_global_fn(l, "se_char_name_set", se_char_name_set);
    lua::set_global_fn(l, "se_faction_cosmetics", se_faction_cosmetics);
    lua::set_global_fn(l, "se_faction_name_set", se_faction_name_set);
    lua::set_global_fn(l, "se_faction_flag_set", se_faction_flag_set);
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

// ------------------------------------------------------------------------------------------
// S4 / S5: faction name and flag (notes/creator_spikes.md)
//   FACTION +0xbf8 current name object {String key, u64 id @+0x10, UniString text @+0x18}
//     (CcoCampaignFaction.Name FUN_143028600: *(F+0xc08) if non-null, else &F+0xc10)
//   FACTION +0xc20 pending name object: FUN_1419f5910 (model tick) moves current -> +0xc48
//     (previous) and pending -> current when pending is non-empty, differs, and F+0xee8 == 0
//   FACTION +0xf20 FACTION_FLAG_AND_COLOURS (0x48): String flag dir (CcoCampaignFaction.
//     FactionFlagDir), String @+0x10, RGB f32 x3 @+0x20 / +0x2c / +0x38. Saved per faction.
// ------------------------------------------------------------------------------------------

const F_NAME: usize = 0xbf8;
const F_PENDING: usize = 0xc20;
const F_PREVIOUS: usize = 0xc48;
const F_NAME_BLOCKED: usize = 0xee8;
const F_FLAG: usize = 0xf20;

unsafe fn faction(l: *mut LuaState) -> Result<usize, String> {
    let f = crate::progression::faction_from_arg(l, 1)?;
    if !readable(f, F_FLAG + 0x48) {
        return Err(format!("faction {f:#x} is not readable up to the flag block"));
    }
    Ok(f)
}

unsafe fn name_obj(o: usize) -> String {
    let id = rq(o + 0x10);
    let via_id = if id != 0 && readable(id, 16) { string_at(id, true) } else { String::new() };
    format!("{{key='{}' id={id:#x}('{via_id}') text='{}'}}", string_at(o, false), string_at(o + 0x18, true))
}

unsafe fn faction_dump(f: usize) -> String {
    let fl = f + F_FLAG;
    let col = |o: usize| -> String {
        let r = core::ptr::read_unaligned((fl + o) as *const [f32; 3]);
        format!("({:.3},{:.3},{:.3})", r[0], r[1], r[2])
    };
    format!(
        "name={} pending={} previous={} blocked={} flag_dir='{}' flag2='{}' colours={} {} {}",
        name_obj(f + F_NAME), name_obj(f + F_PENDING), name_obj(f + F_PREVIOUS), rd(f + F_NAME_BLOCKED) & 0xff,
        string_at(fl, false), string_at(fl + 0x10, false), col(0x20), col(0x2c), col(0x38)
    )
}

/// se_faction_cosmetics(q_faction) -> string
unsafe extern "C" fn se_faction_cosmetics(l: *mut LuaState) -> c_int {
    let out = (|| -> Result<String, String> {
        let f = faction(l)?;
        Ok(format!("faction @ {f:#x}: {}", faction_dump(f)))
    })();
    match out {
        Ok(s) => { log!("se_faction_cosmetics: {s}"); lua::push_str(l, &s); }
        Err(e) => { log!("se_faction_cosmetics: {e}"); lua::push_str(l, &format!("error: {e}")); }
    }
    1
}

/// se_faction_name_set(q_faction, text, mode) -> ok, msg
///   mode "pending" (default): the engine applies it on the next model tick (keeps the previous name)
///   mode "direct": overwrite the current name object now
unsafe extern "C" fn se_faction_name_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let text = lua::to_str(l, 2);
    let mode = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if text.is_empty() || text.chars().count() > 48 || text.chars().any(|c| c.is_control()) {
            return Err("text must be 1..48 characters, no control characters".into());
        }
        let f = faction(l)?;
        let before = faction_dump(f);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let src: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.from_wcstr)(src.as_mut_ptr() as P, wide.as_ptr());
        let name: &'static mut [u8; 0x40] = Box::leak(Box::new([0u8; 0x40]));
        (e.name_ctor)(name.as_mut_ptr() as P);
        (e.name_set_text)(name.as_mut_ptr() as P, src.as_mut_ptr() as P);
        let direct = mode == "direct";
        let target = if direct { f + F_NAME } else { f + F_PENDING };
        (e.str_assign)(target as P, name.as_mut_ptr() as P);
        core::ptr::write_unaligned((target + 0x10) as *mut usize, 0);
        (e.ustr_assign)((target + 0x18) as P, name.as_mut_ptr().add(0x18) as P);
        let after = faction_dump(f);
        let how = if direct { "direct" } else { "pending, applied on the next model tick" };
        log!("se_faction_name_set: '{text}' ({how})\n  before: {before}\n  after:  {after}");
        let blocked = rd(f + F_NAME_BLOCKED) & 0xff;
        let warn = if blocked != 0 && !direct { "; warning: +0xee8 is set, the engine will not apply a pending name" } else { "" };
        Ok(format!("'{text}' written ({how}){warn}"))
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_faction_name_set: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

/// se_faction_flag_set(q_faction, flag_dir, r1,g1,b1, r2,g2,b2, r3,g3,b3) -> ok, msg
/// flag_dir "" keeps the path; colours are 0..255; a missing or negative triple is kept.
unsafe extern "C" fn se_faction_flag_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let dir = lua::to_str(l, 2);
    let out = (|| -> Result<String, String> {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction(l)?;
        if dir.len() > 200 || dir.bytes().any(|b| !(0x20..0x7f).contains(&b)) {
            return Err("flag_dir must be printable ASCII, at most 200 bytes".into());
        }
        let before = faction_dump(f);
        let fl = f + F_FLAG;
        if !dir.is_empty() {
            let mut c = dir.clone().into_bytes();
            c.push(0);
            let tmp: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
            (e.str_from_cstr)(tmp.as_mut_ptr() as P, c.as_ptr());
            (e.str_assign)(fl as P, tmp.as_mut_ptr() as P);
        }
        for (i, off) in [0x20usize, 0x2c, 0x38].iter().enumerate() {
            let base = 3 + i as c_int * 3;
            if (api.type_)(l, base) != 3 || (api.type_)(l, base + 2) != 3 { continue; } // LUA_TNUMBER
            let r = (api.tonumber)(l, base) as f32;
            if r < 0.0 { continue; }
            let g = (api.tonumber)(l, base + 1) as f32;
            let b = (api.tonumber)(l, base + 2) as f32;
            let v = [r, g, b].map(|x| x.clamp(0.0, 255.0) / 255.0);
            core::ptr::write_unaligned((fl + off) as *mut [f32; 3], v);
        }
        let after = faction_dump(f);
        log!("se_faction_flag_set:\n  before: {before}\n  after:  {after}");
        Ok(after)
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_faction_flag_set: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}
