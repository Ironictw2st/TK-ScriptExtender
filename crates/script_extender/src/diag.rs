//! Diagnostics: a high-resolution stopwatch for the Lua listener profiler in `se_api.lua`
//! (`se.diag.listeners_start / listeners_report`). lua_Number is a 32-bit float in this build,
//! so absolute clock values are useless in Lua; the stopwatch lives here and only elapsed
//! microseconds cross the boundary.
//!
//! se_timer_push([level]) -> depth     start a stopwatch (after dropping those at `level` and
//!                                     above); returns its 1-based depth
//! se_timer_pop(depth) -> microseconds elapsed time of that stopwatch; everything started
//!                                     after it is dropped too (a callback that raised an error
//!                                     never reaches its own pop)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use retour::GenericDetour;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use core::ffi::{c_int, c_void};
use std::cell::RefCell;
use std::time::Instant;

thread_local! {
    static STACK: RefCell<Vec<Instant>> = const { RefCell::new(Vec::new()) };
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_timer_push", se_timer_push);
    lua::set_global_fn(l, "se_timer_pop", se_timer_pop);
}

unsafe extern "C" fn se_timer_push(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    // optional level (1-based): stopwatches at that level and above were left behind by
    // callbacks that raised an error; drop them first
    let level = if (api.gettop)(l) >= 1 { (api.tonumber)(l, 1) as usize } else { 0 };
    let depth = STACK.with(|s| {
        let mut s = s.borrow_mut();
        if level >= 1 && level <= s.len() { s.truncate(level - 1); }
        if s.len() >= 256 { s.clear(); }
        s.push(Instant::now());
        s.len()
    });
    (api.pushnumber)(l, depth as f32);
    1
}

unsafe extern "C" fn se_timer_pop(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let depth = (api.tonumber)(l, 1) as usize;
    let micros = STACK.with(|s| {
        let mut s = s.borrow_mut();
        if depth == 0 || depth > s.len() { return 0.0; }
        let started = s[depth - 1];
        s.truncate(depth - 1);
        started.elapsed().as_secs_f64() * 1.0e6
    });
    (api.pushnumber)(l, micros as f32);
    1
}

// ---------------------------------------------------------------------------------------------
// Measurement only (`diag_diplomacy=1` in script_extender.cfg, default off): does the campaign
// AI evaluate the same diplomatic component for the same pair of factions repeatedly?
//
// End-turn profile on 0.34.1: FUN_141bbadf0 (AI target scoring) spends ~9% of the main thread in
// FUN_141aaae10(out, component, negotiation, faction_a, faction_b, params, flags) = "is this
// treaty component available / valid between A and B" (a data-driven condition tree, recursive
// FUN_1413c86b0), called from FUN_141d8bf60(scanner, x) through FUN_141acf410 / FUN_141d8fb40.
// The counters compare, per call of FUN_141d8bf60: evaluations, distinct (component, A, B,
// flags) and distinct (component, A, B, flags, first 64 bytes of params). Nothing is changed.
// ---------------------------------------------------------------------------------------------

type DipEval = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, u32) -> u64;
type DipScan = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;

static DIP_EVAL: OnceLock<GenericDetour<DipEval>> = OnceLock::new();
static DIP_SCAN: OnceLock<GenericDetour<DipScan>> = OnceLock::new();
pub static DIP_SCANS: AtomicU64 = AtomicU64::new(0);
pub static DIP_CALLS: AtomicU64 = AtomicU64::new(0);
pub static DIP_UNSCOPED: AtomicU64 = AtomicU64::new(0);
pub static DIP_DISTINCT: AtomicU64 = AtomicU64::new(0);
pub static DIP_DISTINCT_PARAMS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static DIP_DEPTH: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    static DIP_KEYS: RefCell<(HashSet<(usize, usize, usize, u32)>, HashSet<(usize, usize, usize, u32, u64)>)> = RefCell::new((HashSet::new(), HashSet::new()));
}

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}

/// Is this thread inside a campaign AI diplomacy scan (FUN_141d8bf60)?
pub fn in_scan() -> bool { DIP_DEPTH.with(|d| d.get()) != 0 }

unsafe extern "C" fn dip_scan_detour(a: *mut c_void, b: *mut c_void) -> u64 {
    let Some(h) = DIP_SCAN.get() else { return 0 };
    let _g = crate::crash::enter("cai_dip_scan");
    let outer = DIP_DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v == 0 });
    if outer { DIP_SCANS.fetch_add(1, Ordering::Relaxed); }
    let r = h.call(a, b);
    DIP_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    if outer {
        DIP_KEYS.with(|k| {
            let mut k = k.borrow_mut();
            DIP_DISTINCT.fetch_add(k.0.len() as u64, Ordering::Relaxed);
            DIP_DISTINCT_PARAMS.fetch_add(k.1.len() as u64, Ordering::Relaxed);
            k.0.clear();
            k.1.clear();
        });
    }
    r
}

unsafe extern "C" fn dip_eval_detour(out: *mut c_void, comp: *mut c_void, neg: *mut c_void, fa: *mut c_void, fb: *mut c_void, params: *mut c_void, flags: u32) -> u64 {
    let Some(h) = DIP_EVAL.get() else { return 0 };
    let _g = crate::crash::enter("dip_component_eval");
    let in_scan = DIP_DEPTH.with(|d| d.get()) != 0;
    if !in_scan {
        DIP_UNSCOPED.fetch_add(1, Ordering::Relaxed);
    } else {
        DIP_CALLS.fetch_add(1, Ordering::Relaxed);
        let mut hash: u64 = 0xcbf29ce484222325;
        if !params.is_null() && IsBadReadPtr(params, 64) == 0 {
            for b in core::slice::from_raw_parts(params as *const u8, 64) { hash = (hash ^ *b as u64).wrapping_mul(0x100000001b3); }
        }
        DIP_KEYS.with(|k| {
            let mut k = k.borrow_mut();
            k.0.insert((comp as usize, fa as usize, fb as usize, flags));
            k.1.insert((comp as usize, fa as usize, fb as usize, flags, hash));
        });
    }
    let traced = crate::diptrace::eval_enter(in_scan);
    let r = h.call(out, comp, neg, fa, fb, params, flags);
    if traced { crate::diptrace::eval_exit(out as usize, comp as usize, fa as usize, fb as usize, flags, in_scan); }
    r
}

pub fn install(t: &Table) {
    if !matches!(crate::build::config_value("diag_diplomacy").as_deref(), Some("1") | Some("2")) { return; }
    // SAFETY: both prologues are anchor-verified stack stores / pushes.
    unsafe {
        let eval: DipEval = core::mem::transmute(t.get("dip_component_eval"));
        let scan: DipScan = core::mem::transmute(t.get("cai_dip_scan"));
        let (Ok(e), Ok(s)) = (GenericDetour::new(eval, dip_eval_detour), GenericDetour::new(scan, dip_scan_detour)) else {
            log!("diplomacy diagnostic: could not create the detours");
            return;
        };
        let (_, _) = (DIP_EVAL.set(e), DIP_SCAN.set(s));
        let (Some(e), Some(s)) = (DIP_EVAL.get(), DIP_SCAN.get()) else { return };
        if let Err(err) = crate::freeze::enable_detour("dip_component_eval", "diag_diplomacy", eval as usize, e)
            .and_then(|_| crate::freeze::enable_detour("cai_dip_scan", "diag_diplomacy", scan as usize, s)) {
            log!("diplomacy diagnostic: could not enable the detours: {err}");
            return;
        }
    }
    log!("diplomacy diagnostic installed (counters in se.query.perf(): dip_*)");
    crate::diptrace::install(t);
}
