//! Campaign-AI nudges (feat/ai-campaign). The owner's rule (9 Nov 2026): "all of these should be nudges
//! and tweaks to the AI, just controllable by MCT". Nothing here decides for the AI; it scales the
//! scores the CAI computes itself before the CAI's own weighted random pick (model RNG, so every
//! machine of a multiplayer game picks the same as long as the published data is the same).
//!
//! Skills (notes/AI_CAMPAIGN.md "Skill nudge"):
//!   FUN_141b7c4a0(character, ?, policy, chain*, f32) = the skill chooser of the BDI task
//!     CAI_BDI_CHARACTER_SKILL_MANAGEMENT_FOR_CHARACTER. Detour: remember DETAILS = **(char+0x260)
//!     in a thread-local for the duration of the call.
//!   FUN_141a62a00(tree, out) = available nodes of a tree (out = {u32 cap, u32 count, data}, 0x40-byte
//!     entries, entry+0 = node*). Detour: inside the chooser, remember `out`.
//!   FUN_141b597e0(scores{u32 cap, u32 count, f32*}, functor, rng, flag) = weighted pick. Detour: inside
//!     the chooser and when the score count equals the candidate count, scores[i] *= bias(node i):
//!     bias = 1 + (strength - 1) * fit(skill key, role of DETAILS), fit in 0..1 from se.ai_effects.
//!
//! Natives: se_nudge_skill_fit(chunk) "key=g,gov,min;..." (adds), se_nudge_skill_role(q, cqi, role)
//! (0 general, 1 governor, 2 minister, -1 forget), se_nudge_skill_strength(x) (1 = off, max 4),
//! se_nudge_skill_stats() -> "installed=..;picks=..;nudged=..;fits=..;roles=..".
//! cfg `ai_skills=0` keeps the detours out (in build::sync_tag).

use crate::addrs::Table;
use crate::buildings::record_key;
use crate::lua::{self, LuaState};
use crate::log;
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

type Chooser = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, f32);
type Cands = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type Pick = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32) -> *mut c_void;

static CHOOSER: OnceLock<GenericDetour<Chooser>> = OnceLock::new();
static CANDS: OnceLock<GenericDetour<Cands>> = OnceLock::new();
static PICK: OnceLock<GenericDetour<Pick>> = OnceLock::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);
static STRENGTH: AtomicU32 = AtomicU32::new(0x3f800000); // f32 1.0 = off until script sets it
static PICKS: AtomicU64 = AtomicU64::new(0);
static NUDGED: AtomicU64 = AtomicU64::new(0);
static FIT: Mutex<Option<HashMap<String, [f32; 3]>>> = Mutex::new(None);
static ROLE: Mutex<Option<HashMap<usize, u8>>> = Mutex::new(None);

thread_local! {
    static CUR_DETAILS: Cell<usize> = const { Cell::new(0) };
    static CUR_CANDS: Cell<usize> = const { Cell::new(0) };
    static IN_CHOOSER: Cell<bool> = const { Cell::new(false) };
}
static CHOOSER_CALLS: AtomicU64 = AtomicU64::new(0);

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

fn strength() -> f32 {
    f32::from_bits(STRENGTH.load(Ordering::Relaxed))
}

unsafe extern "C" fn chooser_detour(ch: *mut c_void, a: *mut c_void, policy: *mut c_void, chain: *mut c_void, w: f32) {
    let Some(h) = CHOOSER.get() else { return };
    let _g = crate::crash::enter("ai_skill_chooser");
    // The first argument is the CAI's character wrapper (from the CAI character list), NOT the model
    // character: **(arg+0x260) is not DETAILS (aicamp.7 live: picks = 0). DETAILS is taken from the tree
    // the candidate builder is called with inside this chooser (tree = DETAILS + 0x98).
    let _ = ch;
    let prev = IN_CHOOSER.with(|c| c.replace(true));
    let prev_d = CUR_DETAILS.with(|c| c.replace(0));
    let prev_c = CUR_CANDS.with(|c| c.replace(0));
    CHOOSER_CALLS.fetch_add(1, Ordering::Relaxed);
    h.call(ch, a, policy, chain, w);
    IN_CHOOSER.with(|c| c.set(prev));
    CUR_DETAILS.with(|c| c.set(prev_d));
    CUR_CANDS.with(|c| c.set(prev_c));
}

unsafe extern "C" fn cands_detour(tree: *mut c_void, out: *mut c_void) -> *mut c_void {
    let Some(h) = CANDS.get() else { return core::ptr::null_mut() };
    let r = h.call(tree, out);
    if IN_CHOOSER.with(|c| c.get()) {
        CUR_DETAILS.with(|c| c.set((tree as usize).wrapping_sub(0x98)));
        CUR_CANDS.with(|c| c.set(out as usize));
    }
    r
}

unsafe extern "C" fn pick_detour(scores: *mut c_void, functor: *mut c_void, rng: *mut c_void, flag: u32) -> *mut c_void {
    let Some(h) = PICK.get() else { return core::ptr::null_mut() };
    let d = CUR_DETAILS.with(|c| c.get());
    let cands = CUR_CANDS.with(|c| c.get());
    if d != 0 && cands != 0 {
        PICKS.fetch_add(1, Ordering::Relaxed);
        let s = strength();
        if s > 1.0 {
            let _g = crate::crash::enter("ai_skill_pick");
            nudge(d, cands, scores as usize, s);
        }
    }
    h.call(scores, functor, rng, flag)
}

unsafe fn nudge(d: usize, cands: usize, scores: usize, s: f32) {
    let role = match ROLE.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&d).copied())) {
        Some(r) if r < 3 => r as usize,
        _ => return,
    };
    let (n, cd) = (rd(cands + 4) as usize, rq(cands + 8));
    let (m, sd) = (rd(scores + 4) as usize, rq(scores + 8));
    if n == 0 || n != m || n > 256 || !readable(cd, n * 0x40) || !readable(sd, n * 4) {
        return;
    }
    let Ok(g) = FIT.lock() else { return };
    let Some(fit) = g.as_ref() else { return };
    let mut changed = false;
    for i in 0..n {
        let node = rq(cd + i * 0x40);
        let skill = rq(rq(node) + 0x78);
        let key = record_key(skill);
        if let Some(f) = fit.get(&key) {
            let b = 1.0 + (s - 1.0) * f[role].clamp(0.0, 1.0);
            let p = (sd + i * 4) as *mut f32;
            let v = core::ptr::read_unaligned(p);
            if v.is_finite() && v > 0.0 {
                core::ptr::write_unaligned(p, v * b);
                changed = true;
            }
        }
    }
    if changed {
        NUDGED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn install(t: &Table) {
    if !crate::build::ai_policy_enabled("ai_skills") {
        log!("ai skill nudge disabled by script_extender.cfg (ai_skills=0)");
        return;
    }
    unsafe {
        let c: Chooser = core::mem::transmute(t.get("ai_skill_chooser"));
        let k: Cands = core::mem::transmute(t.get("skill_tree_available"));
        let p: Pick = core::mem::transmute(t.get("cai_weighted_pick"));
        // SAFETY: anchor-verified prologues without RIP-relative bytes (register spills / push / sub rsp).
        let (Ok(dc), Ok(dk), Ok(dp)) = (GenericDetour::new(c, chooser_detour), GenericDetour::new(k, cands_detour), GenericDetour::new(p, pick_detour)) else {
            log!("ai skill nudge: failed to create the detours");
            return;
        };
        let _ = CHOOSER.set(dc);
        let _ = CANDS.set(dk);
        let _ = PICK.set(dp);
        // innermost first; the chooser (which arms the other two) last
        if let Err(e) = crate::freeze::enable_detour("cai_weighted_pick", "ai_skills", p as usize, PICK.get().unwrap()) {
            log!("ai skill nudge: failed to enable cai_weighted_pick: {e}");
            return;
        }
        if let Err(e) = crate::freeze::enable_detour("skill_tree_available", "ai_skills", k as usize, CANDS.get().unwrap()) {
            log!("ai skill nudge: failed to enable skill_tree_available: {e}");
            return;
        }
        if let Err(e) = crate::freeze::enable_detour("ai_skill_chooser", "ai_skills", c as usize, CHOOSER.get().unwrap()) {
            log!("ai skill nudge: failed to enable ai_skill_chooser: {e}");
            return;
        }
        INSTALLED.store(true, Ordering::SeqCst);
        log!("ai skill nudge installed (strength 1.0 = inert until the script sets it)");
    }
}

// ---------------------------------------------------------------------------------------------
// Equipment (notes/AI_CAMPAIGN.md "Equipment nudge"):
//   CAI_BDI_CHARACTER_ANCILLARY_MANAGER update FUN_141b1a3e0 calls, per character in priority order,
//   FUN_141ae6560(DETAILS+0xd10 = character CEO management, faction CEO manager, category, ctx): per
//   category it sorts the slots by their item's value and the candidates by value, and equips a
//   candidate where its value is higher. value = FUN_1418bc5e0(ceo) = ceos.priority of the CEO record
//   (ceo+0x20 -> +0x40 record, +0x18). Nudge: inside FUN_141ae6560, value *= s^d with
//   d = w[role] - max(other roles) (-1..1) from the CEO's role weights: items that suit the
//   character rise, misfits (battle gear on an unarmed leader) fall. Strength 1 = off.
// ---------------------------------------------------------------------------------------------

type Equip = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, *mut c_void);
type CeoValue = unsafe extern "C" fn(*mut c_void) -> u32;
static EQUIP: OnceLock<GenericDetour<Equip>> = OnceLock::new();
static CEO_VALUE: OnceLock<GenericDetour<CeoValue>> = OnceLock::new();
static CEO_INSTALLED: AtomicBool = AtomicBool::new(false);
static CEO_STRENGTH: AtomicU32 = AtomicU32::new(0x3f800000);
static CEO_FIT: Mutex<Option<HashMap<String, [f32; 3]>>> = Mutex::new(None);
static CEO_CALLS: AtomicU64 = AtomicU64::new(0);
static CEO_CHANGED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static EQUIP_DETAILS: Cell<usize> = const { Cell::new(0) };
}
static CEO_MISS_LOGGED: AtomicU64 = AtomicU64::new(0);

// Diagnostic (aicamp.8): who spends AI characters' skill points? FUN_141a630d0(tree, preset list) adds points
// along a preset node list; count and log the first calls (tree, implied DETAILS = tree - 0x98, role known?).
type Preset = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;
static PRESET: OnceLock<GenericDetour<Preset>> = OnceLock::new();
static PRESET_CALLS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn preset_detour(tree: *mut c_void, list: *mut c_void) -> u64 {
    let Some(h) = PRESET.get() else { return 0 };
    let n = PRESET_CALLS.fetch_add(1, Ordering::Relaxed);
    let t = tree as usize;
    let pts_before = rd(t + 0x28);
    let r = h.call(tree, list);
    if n < 20 {
        let d = t.wrapping_sub(0x98);
        let role = ROLE.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&d).copied()));
        log!("skill preset diag #{n}: tree={t:#x} details?={d:#x} role={role:?} list_count={} points {} -> {} ret={r}",
            rd(list as usize + 4), pts_before, rd(t + 0x28));
    }
    r
}

pub fn install_preset_diag(t: &Table) {
    if !crate::build::ai_policy_enabled("ai_skills") {
        return;
    }
    unsafe {
        let f: Preset = core::mem::transmute(t.get("skill_preset_apply"));
        // SAFETY: anchor-verified prologue (mov [rsp+0x10],rdx; push rbp/rsi/r13; mov rbp,rsp): no RIP-relative bytes.
        let Ok(d) = GenericDetour::new(f, preset_detour) else { return };
        let _ = PRESET.set(d);
        match crate::freeze::enable_detour("skill_preset_apply", "ai_skills", f as usize, PRESET.get().unwrap()) {
            Ok(()) => log!("skill preset diag installed"),
            Err(e) => log!("skill preset diag: {e}"),
        }
    }
}

unsafe extern "C" fn equip_detour(mgr: *mut c_void, fac: *mut c_void, cat: i32, ctx: *mut c_void) {
    let Some(h) = EQUIP.get() else { return };
    let _g = crate::crash::enter("ai_equip");
    let prev = EQUIP_DETAILS.with(|c| c.replace((mgr as usize).wrapping_sub(0xd10)));
    h.call(mgr, fac, cat, ctx);
    EQUIP_DETAILS.with(|c| c.set(prev));
}

unsafe extern "C" fn ceo_value_detour(ceo: *mut c_void) -> u32 {
    let Some(h) = CEO_VALUE.get() else { return 0 };
    let v = h.call(ceo);
    let d = EQUIP_DETAILS.with(|c| c.get());
    if d == 0 || v == 0 {
        return v;
    }
    let s = f32::from_bits(CEO_STRENGTH.load(Ordering::Relaxed));
    if s <= 1.0 {
        return v;
    }
    CEO_CALLS.fetch_add(1, Ordering::Relaxed);
    // [V live 9 Nov] ceo+0x20 = the CEO record (key = CA::String at +0); ceo+0x20+0x40 = its RARITY record,
    // +0x18 = the rarity value: vanilla equips purely by rarity. Inside the routine every value is scaled x100
    // (matched or not) so the role factor keeps precision and all compared values stay on one scale.
    const SCALE: f32 = 100.0;
    let base = (v as f32) * SCALE;
    let role = ROLE.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&d).copied()));
    let key = crate::buildings::ca_string(rq(ceo as usize + 0x20));
    let fit = CEO_FIT.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).copied()));
    let factor = match (role, fit) {
        (Some(r), Some(w)) if r < 3 => {
            let r = r as usize;
            let other = (0..3).filter(|&i| i != r).map(|i| w[i]).fold(0.0f32, f32::max);
            s.powf((w[r] - other).clamp(-1.0, 1.0))
        }
        (r, f) => {
            if CEO_MISS_LOGGED.fetch_add(1, Ordering::Relaxed) < 10 {
                log!("ai_equip diag: no fit for details={d:#x} role={r:?} key='{key}' fit_known={}", f.is_some());
            }
            1.0
        }
    };
    let nv = (base * factor).round().max(1.0);
    if factor != 1.0 {
        CEO_CHANGED.fetch_add(1, Ordering::Relaxed);
    }
    if nv.is_finite() && nv < 4.0e9 { nv as u32 } else { v }
}

pub fn install_equipment(t: &Table) {
    if !crate::build::ai_policy_enabled("ai_ceos") {
        log!("ai equipment nudge disabled by script_extender.cfg (ai_ceos=0)");
        return;
    }
    unsafe {
        let e: Equip = core::mem::transmute(t.get("ai_equip_character"));
        let v: CeoValue = core::mem::transmute(t.get("ceo_value"));
        // SAFETY: anchor-verified; FUN_141ae6560 starts with register spills, FUN_1418bc5e0 with two
        // register loads (no RIP-relative bytes; 12-byte getter + int3 padding).
        let (Ok(de), Ok(dv)) = (GenericDetour::new(e, equip_detour), GenericDetour::new(v, ceo_value_detour)) else {
            log!("ai equipment nudge: failed to create the detours");
            return;
        };
        let _ = EQUIP.set(de);
        let _ = CEO_VALUE.set(dv);
        if let Err(err) = crate::freeze::enable_detour("ceo_value", "ai_ceos", v as usize, CEO_VALUE.get().unwrap()) {
            log!("ai equipment nudge: failed to enable ceo_value: {err}");
            return;
        }
        if let Err(err) = crate::freeze::enable_detour("ai_equip_character", "ai_ceos", e as usize, EQUIP.get().unwrap()) {
            log!("ai equipment nudge: failed to enable ai_equip_character: {err}");
            return;
        }
        CEO_INSTALLED.store(true, Ordering::SeqCst);
        log!("ai equipment nudge installed (strength 1.0 = inert until the script sets it)");
    }
}

/// se_nudge_ceo_fit(chunk) "key=g,gov,min;..." ("" clears) ; se_nudge_ceo_strength(x) ; se_nudge_ceo_stats()
unsafe extern "C" fn se_nudge_ceo_fit(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let s = lua::to_str(l, 1);
    let n = parse_fits(&CEO_FIT, &s);
    (api.pushinteger)(l, n as isize);
    1
}

unsafe extern "C" fn se_nudge_ceo_strength(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let x = (api.tonumber)(l, 1);
    let x = if x.is_finite() { x.clamp(1.0, 4.0) } else { 1.0 };
    CEO_STRENGTH.store(x.to_bits(), Ordering::SeqCst);
    (api.pushnumber)(l, x);
    1
}

unsafe extern "C" fn se_nudge_ceo_stats(l: *mut LuaState) -> c_int {
    let fits = CEO_FIT.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0);
    lua::push_str(l, &format!("installed={};strength={};values={};changed={};fits={fits};misses={};preset_calls={}", CEO_INSTALLED.load(Ordering::SeqCst) as u8,
        f32::from_bits(CEO_STRENGTH.load(Ordering::Relaxed)), CEO_CALLS.load(Ordering::Relaxed), CEO_CHANGED.load(Ordering::Relaxed),
        CEO_MISS_LOGGED.load(Ordering::Relaxed), PRESET_CALLS.load(Ordering::Relaxed)));
    1
}

fn parse_fits(m: &Mutex<Option<HashMap<String, [f32; 3]>>>, s: &str) -> usize {
    let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    if s.is_empty() {
        map.clear();
    }
    let mut n = 0;
    for item in s.split(';') {
        let Some((k, v)) = item.split_once('=') else { continue };
        let f: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse::<f32>().ok()).collect();
        if f.len() == 3 && !k.is_empty() {
            map.insert(k.trim().to_string(), [f[0], f[1], f[2]]);
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------------------------
// Court (notes/AI_CAMPAIGN.md "Court / governor nudge"): the CAI's post assignment scores a candidate
// for a post by one of his 5 attribute-based role values, FUN_141b33f00(role_analysis, out[5 x f32],
// DETAILS) (cached per character, rank / age scaled). Vanilla ignores the minister effects of his
// traits / items / skills. Nudge: the 5 returned values x (1 + (strength - 1) x v), v = 0..1 published
// per character from Lua (se.ai_court: personality lean, main income type, his minister effects).
// ---------------------------------------------------------------------------------------------

type RoleValues = unsafe extern "C" fn(*mut c_void, *mut f32, *mut c_void) -> *mut f32;
static ROLE_VALUES: OnceLock<GenericDetour<RoleValues>> = OnceLock::new();
static COURT_INSTALLED: AtomicBool = AtomicBool::new(false);
static COURT_STRENGTH: AtomicU32 = AtomicU32::new(0x3f800000);
static COURT_VALUE: Mutex<Option<HashMap<usize, f32>>> = Mutex::new(None);
static COURT_CALLS: AtomicU64 = AtomicU64::new(0);
static COURT_CHANGED: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn role_values_detour(ra: *mut c_void, out: *mut f32, details: *mut c_void) -> *mut f32 {
    let Some(h) = ROLE_VALUES.get() else { return out };
    let r = h.call(ra, out, details);
    let s = f32::from_bits(COURT_STRENGTH.load(Ordering::Relaxed));
    if s <= 1.0 || out.is_null() {
        return r;
    }
    COURT_CALLS.fetch_add(1, Ordering::Relaxed);
    let v = match COURT_VALUE.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&(details as usize)).copied())) {
        Some(v) if v > 0.0 => v.min(1.0),
        _ => return r,
    };
    let m = 1.0 + (s - 1.0) * v;
    if readable(out as usize, 20) {
        for i in 0..5 {
            let p = out.add(i);
            let x = core::ptr::read_unaligned(p);
            if x.is_finite() {
                core::ptr::write_unaligned(p, x * m);
            }
        }
        COURT_CHANGED.fetch_add(1, Ordering::Relaxed);
    }
    r
}

pub fn install_court(t: &Table) {
    if !crate::build::ai_policy_enabled("ai_court") {
        log!("ai court nudge disabled by script_extender.cfg (ai_court=0)");
        return;
    }
    unsafe {
        let f: RoleValues = core::mem::transmute(t.get("cai_role_values"));
        // SAFETY: anchor-verified prologue (register spills, pushes): no RIP-relative bytes.
        let Ok(d) = GenericDetour::new(f, role_values_detour) else {
            log!("ai court nudge: failed to create the detour");
            return;
        };
        let _ = ROLE_VALUES.set(d);
        if let Err(e) = crate::freeze::enable_detour("cai_role_values", "ai_court", f as usize, ROLE_VALUES.get().unwrap()) {
            log!("ai court nudge: failed to enable cai_role_values: {e}");
            return;
        }
        COURT_INSTALLED.store(true, Ordering::SeqCst);
        log!("ai court nudge installed (strength 1.0 = inert until the script sets it)");
    }
}

/// se_nudge_court_value(q, cqi, v): 0..1 court value of a character (0 / negative forgets him).
unsafe extern "C" fn se_nudge_court_value(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let v = (api.tonumber)(l, 3);
    let ok = match crate::skills::tree_of(l, cqi) {
        Ok((d, _)) => {
            let mut g = COURT_VALUE.lock().unwrap_or_else(|p| p.into_inner());
            let m = g.get_or_insert_with(HashMap::new);
            if m.len() > 20000 { m.clear(); }
            if v.is_finite() && v > 0.0 { m.insert(d, v.min(1.0)); } else { m.remove(&d); }
            true
        }
        Err(_) => false,
    };
    (api.pushboolean)(l, ok as c_int);
    1
}

unsafe extern "C" fn se_nudge_court_strength(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let x = (api.tonumber)(l, 1);
    let x = if x.is_finite() { x.clamp(1.0, 4.0) } else { 1.0 };
    COURT_STRENGTH.store(x.to_bits(), Ordering::SeqCst);
    (api.pushnumber)(l, x);
    1
}

unsafe extern "C" fn se_nudge_court_stats(l: *mut LuaState) -> c_int {
    let n = COURT_VALUE.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0);
    lua::push_str(l, &format!("installed={};strength={};calls={};changed={};values={n}", COURT_INSTALLED.load(Ordering::SeqCst) as u8,
        f32::from_bits(COURT_STRENGTH.load(Ordering::Relaxed)), COURT_CALLS.load(Ordering::Relaxed), COURT_CHANGED.load(Ordering::Relaxed)));
    1
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_nudge_court_value", se_nudge_court_value);
    lua::set_global_fn(l, "se_nudge_court_strength", se_nudge_court_strength);
    lua::set_global_fn(l, "se_nudge_court_stats", se_nudge_court_stats);
    lua::set_global_fn(l, "se_nudge_ceo_fit", se_nudge_ceo_fit);
    lua::set_global_fn(l, "se_nudge_ceo_strength", se_nudge_ceo_strength);
    lua::set_global_fn(l, "se_nudge_ceo_stats", se_nudge_ceo_stats);
    lua::set_global_fn(l, "se_nudge_skill_fit", se_nudge_skill_fit);
    lua::set_global_fn(l, "se_nudge_skill_role", se_nudge_skill_role);
    lua::set_global_fn(l, "se_nudge_skill_strength", se_nudge_skill_strength);
    lua::set_global_fn(l, "se_nudge_skill_stats", se_nudge_skill_stats);
}

/// se_nudge_skill_fit(chunk): "key=g,gov,min;key=...". Adds / replaces entries; "" clears all.
unsafe extern "C" fn se_nudge_skill_fit(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let s = lua::to_str(l, 1);
    let mut g = FIT.lock().unwrap_or_else(|p| p.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    if s.is_empty() {
        m.clear();
    }
    let mut n = 0;
    for item in s.split(';') {
        let Some((k, v)) = item.split_once('=') else { continue };
        let f: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse::<f32>().ok()).collect();
        if f.len() == 3 && !k.is_empty() {
            m.insert(k.trim().to_string(), [f[0], f[1], f[2]]);
            n += 1;
        }
    }
    (api.pushinteger)(l, n as isize);
    1
}

/// se_nudge_skill_role(q, cqi, role): 0 general, 1 governor, 2 minister, anything else forgets him.
unsafe extern "C" fn se_nudge_skill_role(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let role = (api.tointeger)(l, 3);
    let ok = match crate::skills::tree_of(l, cqi) {
        Ok((d, _)) => {
            let mut g = ROLE.lock().unwrap_or_else(|p| p.into_inner());
            let m = g.get_or_insert_with(HashMap::new);
            if m.len() > 20000 { m.clear(); }
            if (0..3).contains(&role) { m.insert(d, role as u8); } else { m.remove(&d); }
            true
        }
        Err(_) => false,
    };
    (api.pushboolean)(l, ok as c_int);
    1
}

/// se_nudge_skill_strength(x): 1 = off, up to 4.
unsafe extern "C" fn se_nudge_skill_strength(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let x = (api.tonumber)(l, 1);
    let x = if x.is_finite() { x.clamp(1.0, 4.0) } else { 1.0 };
    STRENGTH.store(x.to_bits(), Ordering::SeqCst);
    (api.pushnumber)(l, x);
    1
}

unsafe extern "C" fn se_nudge_skill_stats(l: *mut LuaState) -> c_int {
    let fits = FIT.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0);
    let roles = ROLE.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0);
    lua::push_str(l, &format!("installed={};strength={};chooser={};picks={};nudged={};fits={fits};roles={roles}",
        INSTALLED.load(Ordering::SeqCst) as u8, strength(), CHOOSER_CALLS.load(Ordering::Relaxed), PICKS.load(Ordering::Relaxed), NUDGED.load(Ordering::Relaxed)));
    1
}
