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

fn strength() -> f32 {
    f32::from_bits(STRENGTH.load(Ordering::Relaxed))
}

unsafe extern "C" fn chooser_detour(ch: *mut c_void, a: *mut c_void, policy: *mut c_void, chain: *mut c_void, w: f32) {
    let Some(h) = CHOOSER.get() else { return };
    let _g = crate::crash::enter("ai_skill_chooser");
    let d = rq(rq(ch as usize + 0x260));
    let prev = CUR_DETAILS.with(|c| c.replace(d));
    let prev_c = CUR_CANDS.with(|c| c.replace(0));
    h.call(ch, a, policy, chain, w);
    CUR_DETAILS.with(|c| c.set(prev));
    CUR_CANDS.with(|c| c.set(prev_c));
}

unsafe extern "C" fn cands_detour(tree: *mut c_void, out: *mut c_void) -> *mut c_void {
    let Some(h) = CANDS.get() else { return core::ptr::null_mut() };
    let r = h.call(tree, out);
    let d = CUR_DETAILS.with(|c| c.get());
    if d != 0 && tree as usize == d + 0x98 {
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

pub unsafe fn register(l: *mut LuaState) {
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
    lua::push_str(l, &format!("installed={};strength={};picks={};nudged={};fits={fits};roles={roles}",
        INSTALLED.load(Ordering::SeqCst) as u8, strength(), PICKS.load(Ordering::Relaxed), NUDGED.load(Ordering::Relaxed)));
    1
}
