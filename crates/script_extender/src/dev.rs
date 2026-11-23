//! Developer: complete a technology / reform instantly (notes/context_viewer.md "Research").
//!
//! CA's own dev cheats (CcoCampaignFaction CompleteResearch / EnableAllTechnologies, the
//! technology DevEnableTech, ...) are compiled out of the release exe (ret-0 stub 0x1402df990),
//! and Lua's begin_tech_research / set_tech_research_cooldown cannot finish anything. This drives
//! the engine's own research tick instead (build 1.7.2.0, static RE 23 Nov 2026):
//!   tech manager M       = *(FACTION + 0xe58)                  (FUN_1419c1560)
//!   technologies table   = FUN_14098abf0(db_get(world + 0x3b38)); record = record_base(table, &key)
//!   tech T               = FUN_1419cc4b0(M, record)            (walks M+0x50[M+0x4c] trees)
//!   T + 8 state          : 0/1 researched (FUN_1419ca1a0), 2 researching, 3 available, more = locked
//!   M + 0x68             = tech being researched (null = none), T + 0xc progress
//!   FUN_1419d19a0(M, force, autopick) = the per-turn research tick: adds the turn's points, or with
//!     `force` the FULL cost, so the current research completes now through FUN_1419e8ee0(M, T, 0).
//!     Only with autopick (and faction+0xcd0 set) does it raise ResearchCompleted (vtable
//!     0x1434a6240) and start the next queued research, exactly as at a normal turn start.
//! begin_tech_research (FUN_1419ed6f0) only starts AVAILABLE techs (state 3) when nothing else is
//! being researched and the gold cost fits, so a locked / hidden reform is put in research directly:
//! T+8 = 2, M+0x68 = T, then the forced tick.
//!
//! Opt-in (cfg `dev_complete_tech=1`), single player only, never on the se.cv allow-list. It changes
//! the simulation, so the cfg key is part of build::sync_tag.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

type P = *mut c_void;

struct Fns {
    tech_table: unsafe extern "C" fn(P) -> P,
    tech_lookup: unsafe extern "C" fn(P, P) -> P,
    tick: unsafe extern "C" fn(P, u8, u8),
    db_get: unsafe extern "C" fn(P) -> P,
    record_base: unsafe extern "C" fn(P, *const c_void) -> P,
    string_from_cstr: unsafe extern "C" fn(P, *const c_char) -> P,
    string_dtor: unsafe extern "C" fn(P),
}
unsafe impl Send for Fns {}
unsafe impl Sync for Fns {}
static FNS: OnceLock<Fns> = OnceLock::new();

const OFF_FACTION_TECH_MGR: usize = 0xe58;
const OFF_MGR_CURRENT: usize = 0x68;
const OFF_TECH_STATE: usize = 8;

pub fn enabled() -> bool {
    crate::build::config_value("dev_complete_tech").as_deref().map(str::trim) == Some("1")
}

pub fn install(t: &Table) {
    unsafe {
        let _ = FNS.set(Fns {
            tech_table: core::mem::transmute(t.get("tech_table")),
            tech_lookup: core::mem::transmute(t.get("tech_lookup")),
            tick: core::mem::transmute(t.get("tech_research_tick")),
            db_get: core::mem::transmute(t.get("db_get")),
            record_base: core::mem::transmute(t.get("record_base")),
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
        });
    }
    if enabled() {
        log!("developer research completion enabled (dev_complete_tech=1): se.dev.complete_tech");
    }
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_dev_complete_tech", se_dev_complete_tech);
    lua::set_global_fn(l, "se_dev_tech_state", se_dev_tech_state);
}

extern "system" {
    fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

fn state_name(s: u32) -> &'static str {
    match s { 0 | 1 => "researched", 2 => "researching", 3 => "available", _ => "locked" }
}

/// (tech manager, tech object) for a faction script object + tech key.
unsafe fn resolve(f: &Fns, l: *mut LuaState) -> Result<(usize, usize), String> {
    let faction = crate::progression::faction_from_arg(l, 1)?;
    let key = lua::to_str(l, 2);
    if key.is_empty() || key.len() > 200 { return Err("tech key empty or too long".into()); }
    let mgr = rq(faction + OFF_FACTION_TECH_MGR);
    if mgr == 0 || !readable(mgr, 0x1a8) { return Err("faction has no technology manager (faction+0xe58)".into()); }
    let world = rq(rq(faction + 0x288) + 0x78);
    if world == 0 || !readable(world + 0x3b38, 8) { return Err("could not derive the model from the faction".into()); }
    let db = (f.db_get)((world + 0x3b38) as P);
    if db.is_null() { return Err("db manager missing".into()); }
    let table = (f.tech_table)(db);
    if table.is_null() { return Err("technologies table missing".into()); }
    let mut c = key.as_bytes().to_vec();
    c.push(0);
    let mut buf = [0u8; 64];
    (f.string_from_cstr)(buf.as_mut_ptr() as P, c.as_ptr() as *const c_char);
    let rec = (f.record_base)(table, buf.as_ptr() as *const c_void) as usize;
    (f.string_dtor)(buf.as_mut_ptr() as P);
    if rec == 0 { return Err(format!("no technologies record named '{key}'")); }
    let tech = (f.tech_lookup)(mgr as P, rec as P) as usize;
    if tech == 0 || !readable(tech, 0x10) {
        return Err(format!("'{key}' is not in this faction's technology trees"));
    }
    Ok((mgr, tech))
}

/// se_dev_tech_state(q_faction, tech_key) -> state name, state number | nil, error
unsafe extern "C" fn se_dev_tech_state(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let Some(f) = FNS.get() else { (api.pushnil)(l); lua::push_str(l, "not installed"); return 2 };
    match resolve(f, l) {
        Ok((_, tech)) => {
            let s = rd(tech + OFF_TECH_STATE);
            lua::push_str(l, state_name(s));
            (api.pushinteger)(l, s as isize);
            2
        }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

/// se_dev_complete_tech(q_faction, tech_key) -> ok, message
unsafe extern "C" fn se_dev_complete_tech(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let r: Result<String, String> = (|| {
        let f = FNS.get().ok_or("not installed")?;
        if !enabled() { return Err("disabled: set dev_complete_tech=1 in script_extender.cfg (developer tool)".into()); }
        let (mgr, tech) = resolve(f, l)?;
        let key = lua::to_str(l, 2);
        let before = rd(tech + OFF_TECH_STATE);
        if before <= 1 { return Ok(format!("'{key}' was already researched")); }
        let cur = rq(mgr + OFF_MGR_CURRENT);
        if cur != 0 && cur != tech {
            return Err(format!("the faction is researching another technology (state 2 at {cur:#x}); finish or cancel it first"));
        }
        // put it into research (what begin_tech_research does for an available tech, without the
        // availability / cost gates), then the forced tick completes it through the engine's path
        core::ptr::write_unaligned((tech + OFF_TECH_STATE) as *mut u32, 2);
        core::ptr::write_unaligned((mgr + OFF_MGR_CURRENT) as *mut usize, tech);
        // autopick = 1 as at a turn start: only then does the tick raise the ResearchCompleted event
        // (gated on faction+0xcd0 && autopick, 0x1419d1b1d) and start the next queued research
        (f.tick)(mgr as P, 1, 1);
        let after = rd(tech + OFF_TECH_STATE);
        let msg = format!("'{key}': {} -> {}", state_name(before), state_name(after));
        log!("se_dev_complete_tech: {msg}");
        if after > 1 { return Err(format!("{msg} (the tick did not complete it)")); }
        Ok(msg)
    })();
    match r {
        Ok(m) => { (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}
