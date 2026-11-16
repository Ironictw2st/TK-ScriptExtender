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
    init_colours(t);
    init_loc(t);
    init_items(t);
    init_tree(t);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_char_names", se_char_names);
    lua::set_global_fn(l, "se_char_name_set", se_char_name_set);
    lua::set_global_fn(l, "se_faction_cosmetics", se_faction_cosmetics);
    lua::set_global_fn(l, "se_faction_name_set", se_faction_name_set);
    lua::set_global_fn(l, "se_faction_flag_set", se_faction_flag_set);
    lua::set_global_fn(l, "se_faction_cosmetics_clear", se_faction_cosmetics_clear);
    lua::set_global_fn(l, "se_faction_colours_get", se_faction_colours_get);
    lua::set_global_fn(l, "se_faction_colours_set", se_faction_colours_set);
    lua::set_global_fn(l, "se_faction_colours_reset", se_faction_colours_reset);
    lua::set_global_fn(l, "se_char_art2", se_char_art2);
    lua::set_global_fn(l, "se_char_birth_set", se_char_birth_set);
    lua::set_global_fn(l, "se_char_birth_get", se_char_birth_get);
    lua::set_global_fn(l, "se_loc_get", se_loc_get);
    lua::set_global_fn(l, "se_loc_set", se_loc_set);
    lua::set_global_fn(l, "se_item_rows", se_item_rows);
    lua::set_global_fn(l, "se_custom_item_look", se_custom_item_look);
    lua::set_global_fn(l, "se_skill_tree_info", se_skill_tree_info);
    lua::set_global_fn(l, "se_skill_tree_set", se_skill_tree_set);
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
//   FACTION +0xbf0 factions DB record (key at record+8; static for the whole game session)
//   FACTION +0xbf8 current name object {String key, u64 id @+0x10, UniString text @+0x18}
//     (CcoCampaignFaction.Name FUN_143028600: *(F+0xc08) if non-null, else &F+0xc10)
//   FACTION +0xc20 pending name object: FUN_1419f5910 (model tick) moves current -> +0xc48
//     (previous) and pending -> current when pending is non-empty, differs, and F+0xee8 == 0
//   FACTION +0xf20 FACTION_FLAG_AND_COLOURS (0x48): String flag dir (CcoCampaignFaction.
//     FactionFlagDir), String @+0x10, RGB f32 x3 @+0x20 / +0x2c / +0x38. Saved per faction, but
//     rebuilt from the DB record (FUN_141435830: record+0x78 path, +0xa8/+0xb4/+0xc0 colours).
//   FUN_1419f3df0(F) = faction cosmetics refresh (load, leader change, campaign group change...):
//     sets the pending name from the cosmetic override / leader name and resets the flag dir.
//
// Persistence (cfg `creator_cosmetics`, default on): a detour on the refresh re-asserts every
// registered custom name / flag after the original ran. The registry is keyed by the DB record
// pointer, so a FACTION reallocated by a reload can never be confused with another one. The
// registry only fills from the SP-only natives, so the hook is inert in multiplayer.
// ------------------------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::Mutex;
use retour::GenericDetour;

const F_RECORD: usize = 0xbf0;
const F_NAME: usize = 0xbf8;
const F_PENDING: usize = 0xc20;
const F_PREVIOUS: usize = 0xc48;
const F_NAME_BLOCKED: usize = 0xee8;
const F_FLAG: usize = 0xf20;

#[derive(Clone, Default)]
struct Custom {
    name: Option<String>,
    dir: Option<String>,
    colours: [Option<[f32; 3]>; 3],
}

static CUSTOM: Mutex<Option<HashMap<usize, Custom>>> = Mutex::new(None);

type Refresh = unsafe extern "C" fn(P) -> usize;
static REFRESH: OnceLock<GenericDetour<Refresh>> = OnceLock::new();

fn registry() -> std::sync::MutexGuard<'static, Option<HashMap<usize, Custom>>> {
    match CUSTOM.lock() { Ok(g) => g, Err(p) => p.into_inner() }
}

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("creator_cosmetics") {
        log!("creator cosmetics hook off (creator_cosmetics=0): custom faction names / flags must be re-applied after every load");
        return;
    }
    // SAFETY: anchor-verified prologue `mov rax,rsp; push rbp; push rdi; lea rbp,[rax-0x5f];
    // sub rsp,0xe8`: no RIP-relative operand in the relocated bytes.
    unsafe {
        let target: Refresh = core::mem::transmute(t.get("faction_cosmetics_refresh"));
        let Ok(d) = GenericDetour::new(target, refresh_detour) else {
            log!("creator cosmetics hook: could not create the detour");
            return;
        };
        let _ = REFRESH.set(d);
        let Some(d) = REFRESH.get() else { return };
        if let Err(e) = crate::freeze::enable_detour("faction_cosmetics_refresh", "creator_cosmetics", target as usize, d) {
            log!("creator cosmetics hook: could not enable the detour ({e}); off");
            return;
        }
    }
    log!("creator cosmetics hook installed: registered custom faction names / flags survive the engine's cosmetics refresh");
}

unsafe extern "C" fn refresh_detour(f: P) -> usize {
    let Some(h) = REFRESH.get() else { return 0 };
    let r = h.call(f);
    let _g = crate::crash::enter_quiet("faction_cosmetics_refresh");
    let fu = f as usize;
    if fu == 0 || !readable(fu, F_FLAG + 0x48) { return r; }
    let rec = rq(fu + F_RECORD);
    let custom = { registry().as_ref().and_then(|m| m.get(&rec).cloned()) };
    if let (Some(c), Some(e)) = (custom, ENGINE.get()) {
        apply(e, fu, &c, false);
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if crate::chatty(&N, 20) { log!("creator cosmetics: re-applied after the engine refresh of faction {fu:#x}: {}", faction_dump(fu)); }
    }
    r
}

unsafe fn write_name(e: &Engine, f: usize, text: &str, direct: bool) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    // Leaked on purpose (a few dozen bytes per write): the engine copies out of them.
    let src: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
    (e.from_wcstr)(src.as_mut_ptr() as P, wide.as_ptr());
    let name: &'static mut [u8; 0x40] = Box::leak(Box::new([0u8; 0x40]));
    (e.name_ctor)(name.as_mut_ptr() as P);
    (e.name_set_text)(name.as_mut_ptr() as P, src.as_mut_ptr() as P);
    let target = if direct { f + F_NAME } else { f + F_PENDING };
    (e.str_assign)(target as P, name.as_mut_ptr() as P);
    core::ptr::write_unaligned((target + 0x10) as *mut usize, 0);
    (e.ustr_assign)((target + 0x18) as P, name.as_mut_ptr().add(0x18) as P);
}

unsafe fn write_flag(e: &Engine, f: usize, dir: Option<&str>, colours: &[Option<[f32; 3]>; 3]) {
    let fl = f + F_FLAG;
    if let Some(d) = dir {
        let mut c = d.as_bytes().to_vec();
        c.push(0);
        let tmp: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.str_from_cstr)(tmp.as_mut_ptr() as P, c.as_ptr());
        (e.str_assign)(fl as P, tmp.as_mut_ptr() as P);
    }
    for (i, off) in [0x20usize, 0x2c, 0x38].iter().enumerate() {
        if let Some(v) = colours[i] {
            core::ptr::write_unaligned((fl + off) as *mut [f32; 3], v);
        }
    }
}

/// Write every part of a registration. The name goes through the pending slot (the engine applies
/// it on the next model tick and keeps the previous name); it is skipped when it already shows.
unsafe fn apply(e: &Engine, f: usize, c: &Custom, direct: bool) {
    if let Some(n) = &c.name {
        let current = string_at(f + F_NAME + 0x18, true);
        let pending = string_at(f + F_PENDING + 0x18, true);
        if direct || current != *n || (!pending.is_empty() && pending != *n) {
            write_name(e, f, n, direct);
        }
    }
    if c.dir.is_some() || c.colours.iter().any(|x| x.is_some()) {
        write_flag(e, f, c.dir.as_deref(), &c.colours);
    }
}

fn register_custom(rec: usize, update: impl FnOnce(&mut Custom)) -> Custom {
    let mut g = registry();
    let m = g.get_or_insert_with(HashMap::new);
    let c = m.entry(rec).or_default();
    update(c);
    c.clone()
}

unsafe fn faction(l: *mut LuaState) -> Result<usize, String> {
    let f = crate::progression::faction_from_arg(l, 1)?;
    if !readable(f, F_FLAG + 0x48) {
        return Err(format!("faction {f:#x} is not readable up to the flag block"));
    }
    if rq(f + F_RECORD) == 0 {
        return Err(format!("faction {f:#x} has no DB record"));
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
    let rec = rq(f + F_RECORD);
    let registered = registry().as_ref().map(|m| m.contains_key(&rec)).unwrap_or(false);
    format!(
        "name={} pending={} previous={} blocked={} flag_dir='{}' flag2='{}' colours={} {} {} record={rec:#x} registered={registered} hook={}",
        name_obj(f + F_NAME), name_obj(f + F_PENDING), name_obj(f + F_PREVIOUS), rd(f + F_NAME_BLOCKED) & 0xff,
        string_at(fl, false), string_at(fl + 0x10, false), col(0x20), col(0x2c), col(0x38), REFRESH.get().is_some()
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
///   text "" removes the registration (the next engine refresh restores the engine's name)
/// Either way the name is registered and re-asserted after every engine cosmetics refresh.
unsafe extern "C" fn se_faction_name_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let text = lua::to_str(l, 2);
    let mode = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let e = ENGINE.get().ok_or("engine table missing")?;
        if text.chars().count() > 48 || text.chars().any(|c| c.is_control()) {
            return Err("text must be at most 48 characters, no control characters".into());
        }
        let f = faction(l)?;
        let rec = rq(f + F_RECORD);
        if text.is_empty() {
            register_custom(rec, |c| c.name = None);
            return Ok("custom name removed (the engine's name returns on its next refresh)".into());
        }
        let before = faction_dump(f);
        let direct = mode == "direct";
        let c = register_custom(rec, |c| c.name = Some(text.clone()));
        apply(e, f, &Custom { name: c.name.clone(), ..Default::default() }, direct);
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
/// Registered like the name: re-asserted after every engine cosmetics refresh.
unsafe extern "C" fn se_faction_flag_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let dir = lua::to_str(l, 2);
    let out = (|| -> Result<String, String> {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let f = faction(l)?;
        if dir.len() > 200 || dir.bytes().any(|b| !(0x20..0x7f).contains(&b)) {
            return Err("flag_dir must be printable ASCII, at most 200 bytes".into());
        }
        let mut cols: [Option<[f32; 3]>; 3] = [None; 3];
        for i in 0..3 {
            let base = 3 + i as c_int * 3;
            if (api.type_)(l, base) != 3 || (api.type_)(l, base + 2) != 3 { continue; } // LUA_TNUMBER
            let r = (api.tonumber)(l, base) as f32;
            if r < 0.0 { continue; }
            let g = (api.tonumber)(l, base + 1) as f32;
            let b = (api.tonumber)(l, base + 2) as f32;
            cols[i] = Some([r, g, b].map(|x| x.clamp(0.0, 255.0) / 255.0));
        }
        let before = faction_dump(f);
        let rec = rq(f + F_RECORD);
        let c = register_custom(rec, |c| {
            if !dir.is_empty() { c.dir = Some(dir.clone()); }
            for i in 0..3 { if cols[i].is_some() { c.colours[i] = cols[i]; } }
        });
        write_flag(e, f, c.dir.as_deref(), &c.colours);
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

/// se_faction_cosmetics_clear(q_faction) -> ok, msg : forget the registration (name + flag)
unsafe extern "C" fn se_faction_cosmetics_clear(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let out = (|| -> Result<String, String> {
        let f = faction(l)?;
        let rec = rq(f + F_RECORD);
        let had = registry().as_mut().and_then(|m| m.remove(&rec)).is_some();
        Ok(format!("registration {}", if had { "removed; the engine's values return on its next refresh" } else { "was not set" }))
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

// ------------------------------------------------------------------------------------------
// Faction colours (the 190E Faction Color Editor's DLL functions, folded into the SE; based on
// Alex Zhao's Faction Color Editor, Workshop 3839399323, full permission). notes/colour_editor_natives.md
//   campaign: factions DB record (F+0xbf0) +0xa8 / +0xb4 / +0xc0 = 3 RGB f32 triples (0..1);
//     FUN_141435830 copies them into the FACTION flag block (+0xf20 +0x20/+0x2c/+0x38) on the
//     engine's refresh / load. We write the record (so the engine's own refresh yields them),
//     the live block, and register them with the cosmetics refresh detour.
//   battle: faction_uniform_colours table (FUN_1409216a0(db), db = db_get(model+0x3b38));
//     list {count +0x14, data +0x18} of rows {vtable, factions record* @+8, RGB f32 x3 @+0x10}.
//     Read when unit models are built (next battle).
// Reset restores the values seen at the first write of this game session (the DB is pristine
// at game start).
// ------------------------------------------------------------------------------------------

const REC_COLOURS: usize = 0xa8;
const UNIFORM_COLOURS: usize = 0x10;

static ORIGINALS: Mutex<Option<HashMap<usize, [[f32; 3]; 3]>>> = Mutex::new(None);

struct DbFns {
    db_get: unsafe extern "C" fn(P) -> P,
    uniform_table: unsafe extern "C" fn(P) -> P,
}
static DB: OnceLock<DbFns> = OnceLock::new();

pub fn init_colours(t: &Table) {
    let _ = DB.set(unsafe {
        DbFns {
            db_get: core::mem::transmute(t.get("db_get")),
            uniform_table: core::mem::transmute(t.get("faction_uniform_colours_table")),
        }
    });
}

unsafe fn read3(a: usize) -> [[f32; 3]; 3] {
    let mut o = [[0f32; 3]; 3];
    for i in 0..3 { o[i] = core::ptr::read_unaligned((a + i * 12) as *const [f32; 3]); }
    o
}

fn sane(c: &[[f32; 3]; 3]) -> bool {
    c.iter().flatten().all(|v| v.is_finite() && (-0.01..=1.01).contains(v))
}

/// The faction_uniform_colours row of a faction (by its factions record pointer).
unsafe fn uniform_row(f: usize) -> Result<usize, String> {
    let d = DB.get().ok_or("db table missing")?;
    let rec = rq(f + F_RECORD);
    let model = rq(rq(f + 0x288) + 0x78);
    if model == 0 || !readable(model + 0x3b38, 8) { return Err("no model behind the faction".into()); }
    let db = (d.db_get)((model + 0x3b38) as P) as usize;
    if db == 0 { return Err("no database manager".into()); }
    let table = (d.uniform_table)(db as P) as usize;
    if table == 0 || !readable(table, 0x20) { return Err("faction_uniform_colours table not loaded".into()); }
    let n = rd(table + 0x14) as usize;
    let data = rq(table + 0x18);
    if n == 0 || n > 0x10000 || !readable(data, n * 8) { return Err(format!("odd uniform table (count {n})")); }
    for i in 0..n {
        let row = rq(data + i * 8);
        if row != 0 && readable(row, UNIFORM_COLOURS + 36) && rq(row + 8) == rec {
            return Ok(row + UNIFORM_COLOURS);
        }
    }
    Err("this faction has no faction_uniform_colours row".into())
}

/// Address of the 3 colour triples of a group.
unsafe fn colour_slot(f: usize, group: &str) -> Result<usize, String> {
    let a = match group {
        "campaign" => rq(f + F_RECORD) + REC_COLOURS,
        "battle" => uniform_row(f)?,
        _ => return Err("group must be \"campaign\" or \"battle\"".into()),
    };
    if !readable(a, 36) { return Err("colour block not readable".into()); }
    let c = read3(a);
    if !sane(&c) { return Err(format!("colour block at {a:#x} does not hold 0..1 floats; refusing")); }
    Ok(a)
}

fn remember_original(a: usize, c: [[f32; 3]; 3]) {
    let mut g = match ORIGINALS.lock() { Ok(g) => g, Err(p) => p.into_inner() };
    g.get_or_insert_with(HashMap::new).entry(a).or_insert(c);
}

unsafe fn push_triples(l: *mut LuaState, c: &[[f32; 3]; 3]) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    for t in c { for v in t { (api.pushnumber)(l, (v * 255.0).round()); } }
    9
}

/// se_faction_colours_get(q_faction, group) -> r1,g1,b1, r2,g2,b2, r3,g3,b3 (0..255) | nil, msg
unsafe extern "C" fn se_faction_colours_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let group = lua::to_str(l, 2);
    match (|| -> Result<[[f32; 3]; 3], String> {
        let f = faction(l)?;
        Ok(read3(colour_slot(f, &group)?))
    })() {
        Ok(c) => push_triples(l, &c),
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

/// se_faction_colours_set(q_faction, group, r1,g1,b1, r2,g2,b2, r3,g3,b3) -> ok, msg
/// 0..255; a missing or negative triple is kept.
unsafe extern "C" fn se_faction_colours_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let group = lua::to_str(l, 2);
    let out = (|| -> Result<String, String> {
        let f = faction(l)?;
        let a = colour_slot(f, &group)?;
        let before = read3(a);
        remember_original(a, before);
        let mut c = before;
        let mut set: [Option<[f32; 3]>; 3] = [None; 3];
        for i in 0..3 {
            let base = 3 + i as c_int * 3;
            if (api.type_)(l, base) != 3 || (api.type_)(l, base + 2) != 3 { continue; }
            let r = (api.tonumber)(l, base) as f32;
            if r < 0.0 { continue; }
            let v = [r, (api.tonumber)(l, base + 1) as f32, (api.tonumber)(l, base + 2) as f32].map(|x| x.clamp(0.0, 255.0) / 255.0);
            c[i] = v;
            set[i] = Some(v);
        }
        for i in 0..3 { core::ptr::write_unaligned((a + i * 12) as *mut [f32; 3], c[i]); }
        if group == "campaign" {
            // live: the FACTION flag block, and keep it through the engine's refresh
            if let Some(e) = ENGINE.get() { write_flag(e, f, None, &set); }
            register_custom(rq(f + F_RECORD), |cu| for i in 0..3 { if set[i].is_some() { cu.colours[i] = set[i]; } });
        }
        let msg = format!("{group} colours of faction {f:#x} at {a:#x}: {:?} -> {:?}", before, c);
        log!("se_faction_colours_set: {msg}");
        Ok(msg)
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_faction_colours_set: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

/// se_faction_colours_reset(q_faction, group) -> ok, msg : the values before the first change
/// of this game session (and the campaign registration is dropped)
unsafe extern "C" fn se_faction_colours_reset(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let group = lua::to_str(l, 2);
    let out = (|| -> Result<String, String> {
        let f = faction(l)?;
        let a = colour_slot(f, &group)?;
        let orig = { match ORIGINALS.lock() { Ok(g) => g, Err(p) => p.into_inner() }.as_ref().and_then(|m| m.get(&a).copied()) };
        let Some(o) = orig else { return Ok(format!("{group} colours were not changed this session")) };
        for i in 0..3 { core::ptr::write_unaligned((a + i * 12) as *mut [f32; 3], o[i]); }
        if group == "campaign" {
            if let Some(e) = ENGINE.get() { write_flag(e, f, None, &[Some(o[0]), Some(o[1]), Some(o[2])]); }
            register_custom(rq(f + F_RECORD), |cu| cu.colours = [None; 3]);
        }
        Ok(format!("{group} colours reset to {:?}", o))
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_faction_colours_reset: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

// ------------------------------------------------------------------------------------------
// S3: art set record key helper (the art swap itself is se_char_art2 / +0x548).
//   CHARACTER +0x550 = campaign_character_art_sets record (portrait path FUN_141a71f00 reads its
//   per-age list {count +0x14, data +0x18, 0x58-byte entries}); record key String at record+8.
//   Table getter FUN_14085fc40(db) ("campaign_character_art_sets_table"), lookup record_base(table, &String).
// ------------------------------------------------------------------------------------------

unsafe fn art_key(rec: usize) -> String {
    if rec == 0 || !readable(rec, 0x20) { return String::new(); }
    let k = string_at(rec + 8, false);
    if !k.is_empty() && !k.starts_with('<') { return k; }
    string_at(rq(rec + 8), false)
}

// ------------------------------------------------------------------------------------------
// S3 v2: the art object at CHARACTER+0x548 (= CHARACTER_ART_SET_INFO+0x58). FUN_141a45aa0(info,
// saved_key, model) resolves it from a String key saved with the character, through the hash map
// *(model+0x3bc0): buckets {count +0x64 (bucket array has count entries), heads +0x68}, nodes
// {next +8, key +0x10, object +0x20}; object +0x38 -> record whose key is at +0x18.
// ------------------------------------------------------------------------------------------

unsafe fn art_obj_key(obj: usize) -> String {
    if obj == 0 || !readable(obj, 0x40) { return String::new(); }
    let rec = rq(obj + 0x38);
    if rec == 0 || !readable(rec, 0x28) { return String::new(); }
    for (off, wide) in [(0x18usize, true), (0x18, false), (0x8, false), (0x10, false)] {
        let s = string_at(rec + off, wide);
        if !s.is_empty() && !s.starts_with('<') && s.chars().all(|c| (' '..='~').contains(&c)) { return s; }
    }
    String::new()
}

/// Walk every node of the art map; returns (key, object) pairs (bounded).
unsafe fn art_map(model: usize) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let m = rq(model + 0x3bc0);
    if m == 0 || !readable(m, 0x78) { return out; }
    let n = rd(m + 0x64) as usize;
    let heads = rq(m + 0x68);
    let sentinel = m + 0x50;
    if n == 0 || n > 0x10000 || !readable(heads, n * 8) { return out; }
    let mut seen = std::collections::HashSet::new();
    let mut node = rq(heads);
    let mut guard = 0;
    // nodes form one list threaded through the buckets (next at +8), ending at the sentinel
    while node != 0 && node != sentinel && guard < 20000 && readable(node, 0x28) {
        if !seen.insert(node) { break; }
        let obj = rq(node + 0x20);
        out.push((art_obj_key(obj), obj));
        node = rq(node + 8);
        guard += 1;
    }
    out
}

/// se_char_art2(q_char, cqi, key) -> ok, msg ; key "" reports; "?list" lists the first keys;
/// otherwise sets CHARACTER+0x548 to the art object with that key.
unsafe extern "C" fn se_char_art2(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let key = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let ch = character(l, cqi)?;
        let model = rq(rq(ch + 0x250) + 0x78);
        if model == 0 || !readable(model + 0x3bc8, 8) { return Err("no model".into()); }
        let cur = rq(ch + 0x548);
        let cur_key = art_obj_key(cur);
        if key.is_empty() {
            return Ok(format!("character {cqi}: +0x548 art obj {cur:#x} key '{cur_key}' | +0x550 {:#x} '{}'", rq(ch + 0x550), art_key(rq(ch + 0x550))));
        }
        let map = art_map(model);
        if key == "?list" {
            let v: Vec<String> = map.iter().take(60).map(|(k, _)| k.clone()).collect();
            return Ok(format!("{} art objects: {}", map.len(), v.join(" ")));
        }
        // A key can map to several art objects (one per campaign_character_art_sets row, e.g.
        // Himiko: subculture '' and 3k_ironic_wa). Prefer the one whose record names the
        // character's own subculture; fall back to the first.
        let cands: Vec<&usize> = map.iter().filter(|(k, _)| *k == key).map(|(_, o)| o).collect();
        if cands.is_empty() { return Err(format!("no art object '{key}' ({} known)", map.len())); }
        let sub = char_subculture(ch);
        let pick = if cands.len() > 1 && !sub.is_empty() {
            cands.iter().copied().find(|o| record_mentions(rq(**o + 0x38), &sub)).unwrap_or(cands[0])
        } else { cands[0] };
        let obj = pick;
        let choice = if cands.len() > 1 { format!(" ({} objects for the key, subculture '{sub}' -> {:#x})", cands.len(), *obj) } else { String::new() };
        // The map only holds art objects (keyed by their own record key), so the entry itself is the
        // type check. The old guard compared the first qword of the two objects, which is not a
        // vtable: it refused valid sets (190e_custom_portrait_2_f, 16 Nov).
        if !readable(*obj, 0x40) || art_obj_key(*obj) != key {
            return Err(format!("art object for '{key}' does not read back its key; refusing"));
        }
        core::ptr::write_unaligned((ch + 0x548) as *mut usize, *obj);
        let msg = format!("character {cqi}: art obj '{cur_key}' -> '{key}'{choice}");
        log!("se_char_art2: {msg}");
        Ok(msg)
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_char_art2: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

/// Subculture key of the character's faction: FACTION = **(CHARACTER+0x270), factions record at
/// FACTION+0xbf0, subculture record at record+0x20, key at +8 (skill tree builder arguments).
unsafe fn char_subculture(ch: usize) -> String {
    let fp = rq(ch + 0x270);
    if fp == 0 || !readable(fp, 8) { return String::new(); }
    let f = rq(fp);
    if f == 0 || !readable(f + F_RECORD, 8) { return String::new(); }
    let rec = rq(f + F_RECORD);
    if rec == 0 || !readable(rec + 0x20, 8) { return String::new(); }
    let sr = rq(rec + 0x20);
    if sr == 0 || !readable(sr, 0x20) { return String::new(); }
    string_at(sr + 8, false)
}

/// true when an art-set record holds `want` as a String or as a record pointer whose key (+8) is it
unsafe fn record_mentions(rec: usize, want: &str) -> bool {
    if rec == 0 || !readable(rec, 0xc0) { return false; }
    let mut o = 0x20usize;
    while o < 0xc0 {
        if string_at(rec + o, false) == want { return true; }
        let p = rq(rec + o);
        if p > 0x10000 && readable(p, 0x20) && string_at(p + 8, false) == want { return true; }
        o += 8;
    }
    false
}

/// se_char_birth_get(q_char, cqi) -> birth_year | nil, msg  (CHARACTER+0x4e0 i32, read only)
unsafe extern "C" fn se_char_birth_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match character(l, cqi) {
        Ok(ch) => {
            let y = rd(ch + 0x4e0) as i32;
            if (1..=2000).contains(&y) { (api.pushinteger)(l, y as _); 1 } else { (api.pushnil)(l); lua::push_str(l, &format!("birth field holds {y}")); 2 }
        }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

/// se_char_birth_set(q_char, cqi, birth_year) -> ok, msg  (CHARACTER+0x4e0 i32)
unsafe extern "C" fn se_char_birth_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let year = (api.tointeger)(l, 3) as i32;
    let out = (|| -> Result<String, String> {
        let ch = character(l, cqi)?;
        let before = rd(ch + 0x4e0) as i32;
        if !(1..=2000).contains(&before) { return Err(format!("birth field holds {before}; refusing")); }
        if !(1..=2000).contains(&year) { return Err("birth year out of range".into()); }
        core::ptr::write_unaligned((ch + 0x4e0) as *mut i32, year);
        Ok(format!("character {cqi}: birth year {before} -> {year}"))
    })();
    match out {
        Ok(m) => { log!("se_char_birth_set: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

// ------------------------------------------------------------------------------------------
// S7: localised text of the creator's custom slots (item names). notes/custom_items.md
//   loc STRING_TABLE = *(db + 0x15578) (FUN_140986060 loads every *.loc into it; stored by
//   FUN_1407eb230 / FUN_140934e20), db = db_get(model+0x3b38).
//   STRING_TABLE::set(table, &String key, &UniString text, bool) exported at RVA 0x3a3700;
//   STRING_TABLE::possibly_null_string_get(table, &String key) -> UniString* | null at 0x394480.
// Only keys containing "190e_custom_" may be written (named slots, never arbitrary loc).
// ------------------------------------------------------------------------------------------

const DB_LOC_TABLE: usize = 0x15578;

struct LocFns {
    db_get: unsafe extern "C" fn(P) -> P,
    set: unsafe extern "C" fn(P, P, P, u8),
    get: unsafe extern "C" fn(P, P) -> P,
}
static LOC: OnceLock<LocFns> = OnceLock::new();

pub fn init_loc(t: &Table) {
    let _ = LOC.set(unsafe {
        LocFns {
            db_get: core::mem::transmute(t.get("db_get")),
            set: core::mem::transmute(t.get("string_table_set")),
            get: core::mem::transmute(t.get("string_table_get")),
        }
    });
}

unsafe fn loc_table(l: *mut LuaState) -> Result<usize, String> {
    let lf = LOC.get().ok_or("loc table missing")?;
    let f = crate::progression::faction_from_arg(l, 1)?;
    let model = rq(rq(f + 0x288) + 0x78);
    if model == 0 || !readable(model + 0x3b38, 8) { return Err("no model".into()); }
    let db = (lf.db_get)((model + 0x3b38) as P) as usize;
    if db == 0 || !readable(db + DB_LOC_TABLE, 8) { return Err("no database manager".into()); }
    let t = rq(db + DB_LOC_TABLE);
    if t == 0 || !readable(t, 0x20) { return Err("loc table not loaded".into()); }
    Ok(t)
}

fn custom_loc_key(k: &str) -> bool {
    k.len() <= 160 && k.contains("190e_custom_") && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// se_loc_get(q_faction, key) -> text | nil   (read-only; any key)
unsafe extern "C" fn se_loc_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    let out = (|| -> Result<String, String> {
        let lf = LOC.get().ok_or("loc table missing")?;
        let e = ENGINE.get().ok_or("engine table missing")?;
        let t = loc_table(l)?;
        if key.is_empty() || key.len() > 200 { return Err("bad key".into()); }
        let mut c = key.as_bytes().to_vec(); c.push(0);
        let ks: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.str_from_cstr)(ks.as_mut_ptr() as P, c.as_ptr());
        let r = (lf.get)(t as P, ks.as_mut_ptr() as P) as usize;
        if r == 0 { return Err(format!("no loc entry '{key}'")); }
        Ok(string_at(r, true))
    })();
    match out {
        Ok(s) => { lua::push_str(l, &s); 1 }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

/// se_loc_set(q_faction, key, text) -> ok, msg   (keys containing "190e_custom_" only)
unsafe extern "C" fn se_loc_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    let text = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let lf = LOC.get().ok_or("loc table missing")?;
        let e = ENGINE.get().ok_or("engine table missing")?;
        if !custom_loc_key(&key) { return Err("only loc keys containing 190e_custom_ may be written".into()); }
        if text.chars().count() > 120 || text.chars().any(|c| c.is_control()) { return Err("text must be at most 120 characters, no control characters".into()); }
        let t = loc_table(l)?;
        let mut c = key.as_bytes().to_vec(); c.push(0);
        let ks: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.str_from_cstr)(ks.as_mut_ptr() as P, c.as_ptr());
        let mut w: Vec<u16> = text.encode_utf16().collect(); w.push(0);
        let us: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.from_wcstr)(us.as_mut_ptr() as P, w.as_ptr());
        let before = { let r = (lf.get)(t as P, ks.as_mut_ptr() as P) as usize; if r == 0 { "<none>".to_string() } else { string_at(r, true) } };
        // last argument: 0 = overwrite an existing entry. STRING_TABLE::set (0x1403a3700) with a
        // non-zero flag keeps an existing entry's text (it copies the entry onto itself), so 1 only
        // ever inserted new keys (creator full-test finding, 15 Nov).
        (lf.set)(t as P, ks.as_mut_ptr() as P, us.as_mut_ptr() as P, 0);
        let after = { let r = (lf.get)(t as P, ks.as_mut_ptr() as P) as usize; if r == 0 { "<none>".to_string() } else { string_at(r, true) } };
        let msg = format!("loc '{key}': '{before}' -> '{after}'");
        log!("se_loc_set: {msg}");
        if after == text { Ok(msg) } else { Err(format!("did not take: {msg}")) }
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_loc_set: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

// ------------------------------------------------------------------------------------------
// S7: item look tables, read-only layout probe (next slot). Lazy DB getters:
//   ceos_to_equipment_variants_table FUN_1408cd390(db), ceo_to_ui_display_junctions_table
//   FUN_1408ccc10(db) (cache db+0x55f8). Table: list {count +0x14, data +0x18} of record pointers.
// se_item_rows(q_faction, table "variants"|"ui_display", ceo_key) -> dump of matching rows' String
// fields (offset=text). Only these two tables; read only.
// ------------------------------------------------------------------------------------------

struct ItemFns {
    db_get: unsafe extern "C" fn(P) -> P,
    variants: unsafe extern "C" fn(P) -> P,
    ui_display: unsafe extern "C" fn(P) -> P,
}
static ITEMS: OnceLock<ItemFns> = OnceLock::new();

pub fn init_items(t: &Table) {
    let _ = ITEMS.set(unsafe {
        ItemFns {
            db_get: core::mem::transmute(t.get("db_get")),
            variants: core::mem::transmute(t.get("ceo_variants_table")),
            ui_display: core::mem::transmute(t.get("ceo_ui_display_table")),
        }
    });
}

unsafe fn record_strings(rec: usize, span: usize) -> Vec<(usize, String)> {
    let mut v = Vec::new();
    let mut o = 8usize;
    while o + 16 <= span {
        let s = string_at(rec + o, false);
        if !s.is_empty() && !s.starts_with('<') && s.len() < 200 && s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            v.push((o, s));
        }
        o += 8;
    }
    v
}

/// se_item_rows(q_faction, table, ceo_key) -> string
unsafe extern "C" fn se_item_rows(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let which = lua::to_str(l, 2);
    let key = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let it = ITEMS.get().ok_or("items table missing")?;
        let f = crate::progression::faction_from_arg(l, 1)?;
        let model = rq(rq(f + 0x288) + 0x78);
        if model == 0 || !readable(model + 0x3b38, 8) { return Err("no model".into()); }
        let db = (it.db_get)((model + 0x3b38) as P);
        if db.is_null() { return Err("no db".into()); }
        let table = match which.as_str() {
            "variants" => (it.variants)(db) as usize,
            "ui_display" => (it.ui_display)(db) as usize,
            _ => return Err("table must be variants or ui_display".into()),
        };
        if table == 0 || !readable(table, 0x20) { return Err("table not loaded".into()); }
        let n = rd(table + 0x14) as usize;
        let data = rq(table + 0x18);
        if n == 0 || n > 200000 || !readable(data, n * 8) { return Err(format!("odd table (count {n})")); }
        let mut out = format!("{which}: {n} rows;");
        let mut hits = 0;
        for i in 0..n {
            let rec = rq(data + i * 8);
            if rec == 0 || !readable(rec, 0x100) { continue; }
            let fields = record_strings(rec, 0x100);
            // records may hold the ceo key directly or behind a pointer to the ceos record
            let direct = fields.iter().any(|(_, s)| *s == key);
            let mut via = String::new();
            for o in (8..0x100usize).step_by(8) {
                let p = rq(rec + o);
                if p > 0x10000 && readable(p, 0x20) {
                    let k = string_at(p + 8, false);
                    if k == key { via = format!(" key via ptr +{o:#x}"); break; }
                }
            }
            if direct || !via.is_empty() {
                hits += 1;
                let fs: Vec<String> = fields.iter().map(|(o, s)| format!("+{o:#x}='{s}'")).collect();
                out.push_str(&format!(" | row {i} rec {rec:#x} vt {:#x}{via}: {}", rq(rec), fs.join(" ")));
                if hits >= 4 { break; }
            }
        }
        if hits == 0 { out.push_str(" no row matches"); }
        Ok(out)
    })();
    match out {
        Ok(s) => { log!("se_item_rows: {s}"); lua::push_str(l, &s); 1 }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

// ------------------------------------------------------------------------------------------
// S8: skill trees (notes/skill_trees.md). Tree T = DETAILS + 0x98, DETAILS = **(CHARACTER+0x260).
//   T+0x38 String node-set key (saved; the load rebuilds the tree from it).
//   rows: T+4 count, T+8 data (0x10 each: +4 node count, +8 nodes); node 0x40: +0 node record.
//   node record (character_skill_nodes): key String @+8 (to confirm), skill record @+0x78 (skills.rs
//   reads level details at (*node+0x78)+0xa0).
//   Builder FUN_141a65390(T, FACTION, nodeset_rec, &faction_key, &subculture_key, obj+0x88,
//   *(obj+0x38), u8 a, u8 b, &String) with obj = *(*(FACTION+0x288)+0xd0), faction record =
//   FACTION+0xbf0 (key +8; subculture record *(rec+0x20), key +8). Same call FUN_141a65ff0 (new
//   character) and FUN_141a66230 (respec, on an existing tree) make.
// ------------------------------------------------------------------------------------------

const DET_TREE: usize = 0x98;
const TREE_KEY: usize = 0x38;

struct TreeFns {
    db_get: unsafe extern "C" fn(P) -> P,
    nodesets: unsafe extern "C" fn(P) -> P,
    record_base: unsafe extern "C" fn(P, *const c_void) -> P,
    build: unsafe extern "C" fn(P, P, P, P, P, P, usize, u8, u8, P),
}
static TREE: OnceLock<TreeFns> = OnceLock::new();

pub fn init_tree(t: &Table) {
    let _ = TREE.set(unsafe {
        TreeFns {
            db_get: core::mem::transmute(t.get("db_get")),
            nodesets: core::mem::transmute(t.get("skill_node_sets_table")),
            record_base: core::mem::transmute(t.get("record_base")),
            build: core::mem::transmute(t.get("skill_tree_build")),
        }
    });
}

unsafe fn tree_of(ch: usize) -> Result<usize, String> {
    let det = rq(rq(ch + 0x260));
    if det == 0 || !readable(det + DET_TREE, 0x60) { return Err("character has no details / tree".into()); }
    Ok(det + DET_TREE)
}

unsafe fn rec_key(rec: usize) -> String {
    if rec == 0 || !readable(rec, 0x20) { return String::new(); }
    let k = string_at(rec + 8, false);
    if !k.is_empty() && !k.starts_with('<') { k } else { string_at(rq(rec + 8), false) }
}

/// se_skill_tree_info(q_char, cqi) -> string  (read-only: node set key + node -> skill keys)
unsafe extern "C" fn se_skill_tree_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let out = (|| -> Result<String, String> {
        let ch = character(l, cqi)?;
        let t = tree_of(ch)?;
        let mut s = format!("character {cqi}: node set '{}'", string_at(t + TREE_KEY, false));
        let rows = rd(t + 4) as usize;
        let rdata = rq(t + 8);
        if rows > 64 || (rows > 0 && !readable(rdata, rows * 0x10)) { return Err("odd tree rows".into()); }
        let mut n = 0;
        for r in 0..rows {
            let row = rdata + r * 0x10;
            let cnt = rd(row + 4) as usize;
            let nodes = rq(row + 8);
            if cnt > 64 || (cnt > 0 && !readable(nodes, cnt * 0x40)) { continue; }
            for c in 0..cnt {
                let nd = nodes + c * 0x40;
                let rec = rq(nd);
                let skill = if rec != 0 && readable(rec + 0x78, 8) { rq(rec + 0x78) } else { 0 };
                s.push_str(&format!(" | r{r}c{c} node '{}' skill '{}' pts {}", rec_key(rec), rec_key(skill), rd(nd + 0x10) & 0xff));
                n += 1;
                if n >= 60 { break; }
            }
        }
        Ok(s)
    })();
    match out {
        Ok(m) => { log!("se_skill_tree_info: {m}"); lua::push_str(l, &m); 1 }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

/// se_skill_tree_set(q_char, cqi, node_set_key) -> ok, msg   (rebuild the tree; creation-time use)
unsafe extern "C" fn se_skill_tree_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let key = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let tf = TREE.get().ok_or("tree table missing")?;
        let e = ENGINE.get().ok_or("engine table missing")?;
        if key.is_empty() || key.len() > 120 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { return Err("bad node set key".into()); }
        let ch = character(l, cqi)?;
        let t = tree_of(ch)?;
        let fac = rq(rq(ch + 0x270));
        if fac == 0 || !readable(fac + 0xbf8, 8) { return Err("character has no faction".into()); }
        let frec = rq(fac + 0xbf0);
        if frec == 0 || !readable(frec + 0x28, 8) { return Err("faction has no DB record".into()); }
        let sub = rq(frec + 0x20);
        if sub == 0 || !readable(sub + 0x18, 8) { return Err("faction record has no subculture".into()); }
        let obj = rq(rq(fac + 0x288) + 0xd0);
        if obj == 0 || !readable(obj + 0x98, 8) { return Err("no campaign object".into()); }
        let model = rq(rq(ch + 0x250) + 0x78);
        if model == 0 || !readable(model + 0x3b38, 8) { return Err("no model".into()); }
        let db = (tf.db_get)((model + 0x3b38) as P);
        if db.is_null() { return Err("no db".into()); }
        let table = (tf.nodesets)(db);
        if table.is_null() { return Err("node set table not loaded".into()); }
        let mut c = key.as_bytes().to_vec(); c.push(0);
        let ks: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.str_from_cstr)(ks.as_mut_ptr() as P, c.as_ptr());
        let rec = (tf.record_base)(table, ks.as_ptr() as *const c_void) as usize;
        // record_base returns null for an unknown key; node-set records keep their key elsewhere
        // than +8, so check the vtable lies in the exe instead of comparing the key text
        let (mb, ms) = crate::process::main_module();
        if rec == 0 || !readable(rec, 0x40) || rq(rec) < mb || rq(rec) >= mb + ms { return Err(format!("no node set '{key}'")); }
        let before = string_at(t + TREE_KEY, false);
        (e.str_assign)((t + TREE_KEY) as P, ks.as_mut_ptr() as P);
        let empty: &'static mut [u8; 16] = Box::leak(Box::new([0u8; 16]));
        (e.str_from_cstr)(empty.as_mut_ptr() as P, b"\0".as_ptr());
        (tf.build)(t as P, fac as P, rec as P, (frec + 8) as P, (sub + 8) as P, (obj + 0x88) as P, rq(obj + 0x38), 1, 0, empty.as_mut_ptr() as P);
        let msg = format!("character {cqi}: skill tree '{before}' -> '{}'", string_at(t + TREE_KEY, false));
        log!("se_skill_tree_set: {msg}");
        Ok(msg)
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_skill_tree_set: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

// ------------------------------------------------------------------------------------------
// S7: copy an existing item's look into a custom item slot (notes/custom_items.md).
//   ceos_to_equipment_variants rows: 0x90 bytes {vtable, String ceo key @+8, look fields @+0x18..+0x90
//   (record pointers to meshes / weapons / mounts / animations, one row per game mode)}.
//   ceo_to_ui_display rows: 0x38 bytes {vtable, key @+8, String female image @+0x18, male @+0x28}.
// Only slot keys containing "190e_custom_" are written. Rows are matched in table order (game modes
// appear in the same order for every CEO); if the source has fewer rows, its first row is used.
// A source row holding a heap String inside the copied range is refused (a raw copy would share it).
// ------------------------------------------------------------------------------------------

const VAR_ROW: usize = 0x90;
const VAR_COPY_FROM: usize = 0x18;

unsafe fn table_rows(table: usize) -> Vec<usize> {
    let mut v = Vec::new();
    if table == 0 || !readable(table, 0x20) { return v; }
    let n = rd(table + 0x14) as usize;
    let data = rq(table + 0x18);
    if n == 0 || n > 200000 || !readable(data, n * 8) { return v; }
    for i in 0..n {
        let r = rq(data + i * 8);
        if r != 0 { v.push(r); }
    }
    v
}

/// true when [from, to) of `rec` contains something that looks like a heap String with text
unsafe fn has_heap_string(rec: usize, from: usize, to: usize) -> bool {
    let mut o = from;
    while o + 16 <= to {
        let len = rd(rec + o) as usize;
        let ptr = rq(rec + o + 8);
        if len > 0 && len < 512 && (ptr >> 60) != 8 && ptr > 0x10000 && readable(ptr, len) {
            let b = core::slice::from_raw_parts(ptr as *const u8, len);
            if b.iter().all(|c| (0x20..0x7f).contains(c)) { return true; }
        }
        o += 8;
    }
    false
}

/// se_custom_item_look(q_faction, slot_ceo_key, source_ceo_key) -> ok, msg
unsafe extern "C" fn se_custom_item_look(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let slot = lua::to_str(l, 2);
    let src = lua::to_str(l, 3);
    let out = (|| -> Result<String, String> {
        let it = ITEMS.get().ok_or("items table missing")?;
        let e = ENGINE.get().ok_or("engine table missing")?;
        if !slot.contains("190e_custom_") || !valid_ceo(&slot) { return Err("slot key must contain 190e_custom_".into()); }
        if !valid_ceo(&src) || src == slot { return Err("bad source key".into()); }
        let f = crate::progression::faction_from_arg(l, 1)?;
        let model = rq(rq(f + 0x288) + 0x78);
        if model == 0 || !readable(model + 0x3b38, 8) { return Err("no model".into()); }
        let db = (it.db_get)((model + 0x3b38) as P);
        if db.is_null() { return Err("no db".into()); }
        // look rows
        let rows = table_rows((it.variants)(db) as usize);
        let srows: Vec<usize> = rows.iter().copied().filter(|r| readable(*r, VAR_ROW) && string_at(r + 8, false) == src).collect();
        let drows: Vec<usize> = rows.iter().copied().filter(|r| readable(*r, VAR_ROW) && string_at(r + 8, false) == slot).collect();
        // Items without a 3D look (accessories, followers) have no variant rows: only the icon is
        // copied then. A slot WITH variant rows needs a source with variant rows.
        let mut copied_rows = 0usize;
        if !drows.is_empty() {
            if srows.is_empty() { return Err(format!("'{src}' has no equipment variant rows to copy into '{slot}'")); }
            for (i, d) in drows.iter().enumerate() {
                let s = srows[i.min(srows.len() - 1)];
                if rq(*d) != rq(s) { return Err("row types differ; refusing".into()); }
                if has_heap_string(s, VAR_COPY_FROM, VAR_ROW) { return Err(format!("source row of '{src}' holds text fields in the copied range; refusing")); }
            }
            for (i, d) in drows.iter().enumerate() {
                let s = srows[i.min(srows.len() - 1)];
                core::ptr::copy_nonoverlapping((s + VAR_COPY_FROM) as *const u8, (d + VAR_COPY_FROM) as *mut u8, VAR_ROW - VAR_COPY_FROM);
            }
            copied_rows = drows.len();
        }
        // icon rows
        let urows = table_rows((it.ui_display)(db) as usize);
        let us = urows.iter().copied().find(|r| readable(*r, 0x38) && string_at(r + 8, false) == src);
        let ud = urows.iter().copied().find(|r| readable(*r, 0x38) && string_at(r + 8, false) == slot);
        let icon = match (us, ud) {
            (Some(s), Some(d)) => {
                (e.str_assign)((d + 0x18) as P, (s + 0x18) as P);
                (e.str_assign)((d + 0x28) as P, (s + 0x28) as P);
                "icons copied"
            }
            (None, _) => "source has no ui_display row (icon unchanged)",
            (_, None) => "slot has no ui_display row (icon unchanged)",
        };
        if copied_rows == 0 && us.is_none() { return Err(format!("nothing to copy: '{src}' has no look or icon rows")); }
        let msg = format!("look of '{src}' copied into '{slot}' ({copied_rows} variant rows; {icon})");
        log!("se_custom_item_look: {msg}");
        Ok(msg)
    })();
    match out {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(er) => { log!("se_custom_item_look: {er}"); (api.pushboolean)(l, 0); lua::push_str(l, &er); }
    }
    2
}

fn valid_ceo(k: &str) -> bool {
    !k.is_empty() && k.len() <= 160 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
