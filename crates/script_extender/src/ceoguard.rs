//! Stale CEO-manager contexts in the character panel (crash on right-clicking an item card).
//!
//! Report (Discord 1560503636541308958, 0.45.0): a character joins the faction, the player
//! right-clicks their equipment the same turn, and the game sometimes faults reading
//! 0xFFFFFFFFFFFFFFFF in FUN_1418a9ce0 (a hash-map lookup) called from FUN_142f4a010 =
//! slot 25 of CcoCampaignActiveCeoManager ("ceo_right_click_to_equip"). The map it reads is
//! already freed and reused (header = a repeating 0x00A09E9F pattern in the dump).
//!
//!   CcoCampaignCharacter.CeoManagerContext = FUN_142e8e440(char_cco):
//!       X = *(char_cco+0x50) = CHARACTER; details = **(X+0x260); c = *(details+0xd20);
//!       L = *(c+0x20) (FUN_14038bec0); the context is built by FUN_142f2d100(buf, L):
//!       { +0 vtable 0x14382f190, +8 L, +0x10 L->vfunc[0x38](L) }
//!   +0x10 is derived ONCE, when the context is built. The UI keeps the context; if the
//!   character's CEO container / map is rebuilt meanwhile, every method that reads +0x10 works
//!   on freed memory.
//!
//! Fix ("refresh"): the getter detour remembers which character each L came from (pointer +
//! cqi + object vtable, checked again before use). The context's class-specific methods are
//! reached only through the context's vtable; their vtable entries are replaced (in process
//! memory, anchor-checked) by thunks that re-derive the character's live L and map first and
//! re-point the context when they changed. When the character cannot be verified, the method
//! runs unchanged (the game's own behaviour). UI only: nothing here reaches the model, so the
//! cfg key `ceo_ui_refresh` (default 1) is not part of the multiplayer sync tag.

use crate::addrs::Table;
use crate::lua::{self, LuaState};
use crate::log;
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

type P = *mut c_void;
type Getter = unsafe extern "C" fn(P) -> P;
/// Passed through as 8 integer arguments: four in registers, the rest read from the caller's
/// frame and handed on unchanged (a callee with fewer parameters ignores them). No method of
/// this class takes floating-point arguments.
type Method = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize, usize) -> usize;

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
}
const PAGE_READWRITE: u32 = 0x04;

unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

/// Every class-specific slot of the CcoCampaignActiveCeoManager vtable (the shared generic
/// slots are left alone), with the RVA each must hold before it is replaced. 0.46.0-beta.3 test
/// build 1 wrapped only the ten +0x10 readers; slot 3 (FUN_142f73140) reads +8 and faulted on a
/// panel left open while its character left and rejoined the faction.
const SLOTS: [(usize, usize); 21] = [
    (2, 0x2f4e3e0), (3, 0x2f73140), (9, 0x2f65550), (10, 0x2f7d550), (16, 0x2f7ca70),
    (17, 0x2f6ab60), (19, 0x2f64c60), (20, 0x2f4ab50), (21, 0x2f64bb0), (22, 0x2f449d0),
    (23, 0x2f446d0), (24, 0x2f49c00), (25, 0x2f4a010), (26, 0x2f4a1f0), (27, 0x2f64fa0),
    (28, 0x2f64e20), (29, 0x2f650d0), (30, 0x2f44a20), (31, 0x2f7a730), (32, 0x2f72cd0),
    (33, 0x2f96670),
];

static GETTER: OnceLock<GenericDetour<Getter>> = OnceLock::new();
static VTABLE: AtomicUsize = AtomicUsize::new(0);
static ORIG: [AtomicUsize; 21] = [const { AtomicUsize::new(0) }; 21];
/// L -> (CHARACTER, cqi, CHARACTER vtable) as seen by the getter
static OWNERS: Mutex<Option<HashMap<usize, (usize, u32, usize)>>> = Mutex::new(None);
static LAST_CCO: AtomicUsize = AtomicUsize::new(0);
pub static CALLS: AtomicU64 = AtomicU64::new(0);
pub static REFRESHED: AtomicU64 = AtomicU64::new(0);
pub static UNVERIFIED: AtomicU64 = AtomicU64::new(0);
static LOGGED: AtomicU32 = AtomicU32::new(0);

/// The character's live CEO container L and its map, by the getter's own path.
unsafe fn live_of(x: usize) -> Option<(usize, usize)> {
    let details = rq(rq(x + 0x260));
    if details == 0 || !readable(details + 0xd20, 8) { return None; }
    let c = rq(details + 0xd20);
    if c == 0 || !readable(c + 0x20, 8) { return None; }
    let l = rq(c + 0x20);
    if l == 0 || !readable(l, 8) { return None; }
    let vt = rq(l);
    let f = rq(vt + 0x38);
    if f == 0 { return None; }
    let get_map: unsafe extern "C" fn(usize) -> usize = core::mem::transmute(f);
    let m = get_map(l);
    if m == 0 { None } else { Some((l, m)) }
}

unsafe fn remember(l: usize, x: usize) {
    if l == 0 || !readable(x, 0x280) { return; }
    let mut g = OWNERS.lock().unwrap_or_else(|p| p.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    if map.len() > 8192 { map.clear(); }
    map.insert(l, (x, rd(x + 0x240), rq(x)));
}

unsafe extern "C" fn getter_detour(char_cco: P) -> P {
    let Some(h) = GETTER.get() else { return core::ptr::null_mut() };
    let r = h.call(char_cco);
    let x = rq(char_cco as usize + 0x50);
    if x != 0 {
        if let Some((l, _)) = live_of(x) { remember(l, x); }
    }
    r
}

/// Before a context method runs: re-point a stale context at the character's live container.
unsafe fn refresh(cco: usize) {
    CALLS.fetch_add(1, Ordering::Relaxed);
    if !readable(cco, 0x18) || rq(cco) != VTABLE.load(Ordering::Relaxed) { return; }
    LAST_CCO.store(cco, Ordering::Relaxed);
    let l = rq(cco + 8);
    let owner = OWNERS.lock().unwrap_or_else(|p| p.into_inner()).as_ref().and_then(|m| m.get(&l).copied());
    let Some((x, cqi, xvt)) = owner else { return };
    // the character must still be the one recorded: same object vtable, same cqi, alive
    if !readable(x, 0x280) || rq(x) != xvt || rd(x + 0x240) != cqi || rd(x + 0x278) > 2 {
        UNVERIFIED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some((l2, m2)) = live_of(x) else { UNVERIFIED.fetch_add(1, Ordering::Relaxed); return };
    let m = rq(cco + 0x10);
    if l2 == l && m2 == m { return; }
    core::ptr::write_unaligned((cco + 8) as *mut usize, l2);
    core::ptr::write_unaligned((cco + 0x10) as *mut usize, m2);
    if l2 != l { remember(l2, x); }
    let n = REFRESHED.fetch_add(1, Ordering::Relaxed) + 1;
    if crate::chatty(&LOGGED, 20) {
        log!("ceo context refresh #{n}: cqi {cqi} context {cco:#x} container {l:#x} -> {l2:#x}, map {m:#x} -> {m2:#x}");
    }
}

macro_rules! thunk {
    ($name:ident, $i:expr) => {
        unsafe extern "C" fn $name(a: usize, b: usize, c: usize, d: usize, e: usize, f: usize, g: usize, h: usize) -> usize {
            let _g = crate::crash::enter_quiet("ceo_context_method");
            refresh(a);
            let m: Method = core::mem::transmute(ORIG[$i].load(Ordering::Relaxed));
            m(a, b, c, d, e, f, g, h)
        }
    };
}
thunk!(t0, 0); thunk!(t1, 1); thunk!(t2, 2); thunk!(t3, 3); thunk!(t4, 4); thunk!(t5, 5); thunk!(t6, 6);
thunk!(t7, 7); thunk!(t8, 8); thunk!(t9, 9); thunk!(t10, 10); thunk!(t11, 11); thunk!(t12, 12); thunk!(t13, 13);
thunk!(t14, 14); thunk!(t15, 15); thunk!(t16, 16); thunk!(t17, 17); thunk!(t18, 18); thunk!(t19, 19); thunk!(t20, 20);
const THUNKS: [Method; 21] = [t0, t1, t2, t3, t4, t5, t6, t7, t8, t9, t10, t11, t12, t13, t14, t15, t16, t17, t18, t19, t20];

pub fn install(t: &Table) {
    if crate::build::config_value("ceo_ui_refresh").as_deref() == Some("0") {
        log!("ceo context refresh off (ceo_ui_refresh=0 in script_extender.cfg)");
        return;
    }
    // SAFETY: the getter prologue is anchor-verified (stack store, push, sub rsp); the vtable is
    // verified by its slot 0 and every replaced slot is checked against its expected function.
    unsafe {
        let base = t.base();
        let vt = t.get("ceo_manager_cco_vtable");
        for (i, (slot, rva)) in SLOTS.iter().enumerate() {
            let cur = rq(vt + slot * 8);
            if cur != base + rva {
                log!("ceo context refresh: vtable slot {slot} holds {cur:#x}, expected base+{rva:#x}; not installed");
                return;
            }
            ORIG[i].store(cur, Ordering::Relaxed);
        }
        VTABLE.store(vt, Ordering::Relaxed);
        let target: Getter = core::mem::transmute(t.get("ceo_manager_context_getter"));
        let Ok(d) = GenericDetour::new(target, getter_detour) else {
            log!("ceo context refresh: could not create the getter detour");
            return;
        };
        let _ = GETTER.set(d);
        let Some(d) = GETTER.get() else { return };
        if let Err(e) = crate::freeze::enable_detour("ceo_manager_context_getter", "ceo_ui_refresh", target as usize, d) {
            log!("ceo context refresh: could not enable the getter detour: {e}");
            return;
        }
        // the thunks only read ORIG, which is set; swap the slots one by one (8-byte aligned
        // stores, atomic for a reader on another thread)
        let first = vt + SLOTS[0].0 * 8;
        let last = vt + SLOTS[SLOTS.len() - 1].0 * 8 + 8;
        let mut old = 0u32;
        if VirtualProtect(first as *mut c_void, last - first, PAGE_READWRITE, &mut old) == 0 {
            log!("ceo context refresh: VirtualProtect failed; getter recorded but methods unchanged");
            return;
        }
        for (i, (slot, _)) in SLOTS.iter().enumerate() {
            let p = (vt + slot * 8) as *const AtomicUsize;
            (*p).store(THUNKS[i] as usize, Ordering::SeqCst);
            let (mut before, mut after) = ([0u8; 16], [0u8; 16]);
            before[..8].copy_from_slice(&ORIG[i].load(Ordering::Relaxed).to_le_bytes());
            after[..8].copy_from_slice(&(THUNKS[i] as usize).to_le_bytes());
            crate::status::record_patch("ceo_context_vtable_slot", "ceo_ui_refresh", vt + slot * 8, before, after);
        }
        let mut tmp = 0u32;
        VirtualProtect(first as *mut c_void, last - first, old, &mut tmp);
    }
    log!("ceo context refresh installed ({} context methods)", SLOTS.len());
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_ceo_refresh_info", se_ceo_refresh_info);
    lua::set_global_fn(l, "se_ceo_refresh_selftest", se_ceo_refresh_selftest);
    lua::set_global_fn(l, "se_ceo_rebuilt", se_ceo_rebuilt);
}

pub static REBUILDS: AtomicU64 = AtomicU64::new(0);

/// se_ceo_rebuilt() -> n, cqis
///   Characters (whose CEO contexts the UI built) whose CEO container has been replaced since:
///   every item card of their open character panel now points at freed CEO objects. The entries
///   are updated, so each rebuild is reported once. se_api polls this and closes the panel.
unsafe extern "C" fn se_ceo_rebuilt(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let mut changed: Vec<u32> = Vec::new();
    {
        let mut g = OWNERS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(map) = g.as_mut() {
            let mut moves: Vec<(usize, usize, (usize, u32, usize))> = Vec::new();
            for (&old_l, &(x, cqi, xvt)) in map.iter() {
                if !readable(x, 0x280) || rq(x) != xvt || rd(x + 0x240) != cqi || rd(x + 0x278) > 2 { continue; }
                if let Some((l2, _)) = live_of(x) {
                    if l2 != old_l { moves.push((old_l, l2, (x, cqi, xvt))); }
                }
            }
            for (old_l, l2, v) in moves {
                map.remove(&old_l);
                map.insert(l2, v);
                if !changed.contains(&v.1) { changed.push(v.1); }
            }
        }
    }
    if !changed.is_empty() {
        REBUILDS.fetch_add(changed.len() as u64, Ordering::Relaxed);
        log!("ceo container rebuilt for cqi {changed:?}: open character panels are stale");
    }
    (api.pushnumber)(l, changed.len() as f32);
    let list: Vec<String> = changed.iter().map(|c| c.to_string()).collect();
    lua::push_str(l, &list.join(","));
    2
}

/// se_ceo_refresh_info() -> "installed=1;calls=..;refreshed=..;unverified=..;owners=.."
unsafe extern "C" fn se_ceo_refresh_info(l: *mut LuaState) -> c_int {
    let owners = OWNERS.lock().map(|g| g.as_ref().map_or(0, |m| m.len())).unwrap_or(0);
    let s = format!("installed={};calls={};refreshed={};unverified={};owners={};rebuilds={}",
        (VTABLE.load(Ordering::Relaxed) != 0) as u8, CALLS.load(Ordering::Relaxed),
        REFRESHED.load(Ordering::Relaxed), UNVERIFIED.load(Ordering::Relaxed), owners,
        REBUILDS.load(Ordering::Relaxed));
    lua::push_str(l, &s);
    1
}

/// Fake map with the header the crash dump showed (freed and reused memory).
static POISON: [u64; 8] = [0x00a09e9f00a09e9f; 8];

/// se_ceo_refresh_selftest() -> ok, message
///   Points the most recently used CEO-manager context of the character panel at a fake map with
///   the crash dump's freed-memory header, i.e. makes it exactly as stale as in the report. The
///   next context method must refresh it (log "ceo context refresh") instead of faulting.
///   Diagnostic only; refuses unless ceo_ui_refresh is installed.
unsafe extern "C" fn se_ceo_refresh_selftest(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cco = LAST_CCO.load(Ordering::Relaxed);
    let (ok, msg) = if VTABLE.load(Ordering::Relaxed) == 0 {
        (false, "not installed (ceo_ui_refresh=0 or install failed)".to_string())
    } else if cco == 0 || !readable(cco, 0x18) || rq(cco) != VTABLE.load(Ordering::Relaxed) {
        (false, "no CEO-manager context used yet: open a character's equipment first".to_string())
    } else {
        let l0 = rq(cco + 8);
        let known = OWNERS.lock().map(|g| g.as_ref().is_some_and(|m| m.contains_key(&l0))).unwrap_or(false);
        if !known {
            (false, "the last context is not a character's (no owner recorded)".to_string())
        } else {
            let old = rq(cco + 0x10);
            core::ptr::write_unaligned((cco + 0x10) as *mut usize, POISON.as_ptr() as usize);
            (true, format!("context {cco:#x}: map {old:#x} replaced by a stale-pattern fake; right-click an item card now"))
        }
    };
    log!("se_ceo_refresh_selftest: {msg}");
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &msg);
    2
}
