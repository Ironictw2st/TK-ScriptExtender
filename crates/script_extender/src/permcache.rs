//! Performance: the recruit list builder's permission tables, built once per call instead of
//! once per candidate unit (notes/performance.md, "the quadratic term").
//!
//! FUN_141931e10(ctx, &out, mask, flag) = the body of the "what can this retinue slot recruit"
//! list. It starts with FUN_1418f2080(map, desc, character_permissions): a hash map of every
//! unit permission record of the character (`details+0xc00`), each requirement evaluated against
//! the recruiter description `desc` (= ctx+0x10). Then, for EVERY candidate unit and whenever
//! `flag != 1` (the campaign AI and most UI callers pass 0), it builds a second, complete map
//! from the faction's permission list (`faction+0x2260`) with the same `desc`, looks up that one
//! unit, and destroys the map again (FUN_1418f3520). The cost is units x faction permissions
//! per call: 15% of the main thread during an end turn on a modded late campaign.
//!
//! Inside one call of FUN_141931e10 neither `desc` (a stack object of the caller) nor the lists
//! change, so every one of those maps is identical. The detours below build a map once per
//! (desc, list) per call ("master", owned here) and hand the engine views of it:
//!   map (0x50): +0 u32 count, +8 list sentinel {prev, next = +0x10 head}, +0x18 bucket vector
//!   {cap, count, data @+0x20}, +0x28 f32 load factor, +0x30 / +0x40 vectors of record pointers
//!   {cap, count, data @+0x38 / +0x48}. Node 0x68: +0 prev, +8 next, +0x10 key, value from +0x18.
//!   The caller frees the three arrays itself (inlined) and calls FUN_1418f3520 for the nodes.
//! A view is a bitwise copy of the master with its own copies of the three arrays (the caller
//! frees those) that shares the master's nodes; lookups never touch the sentinel address except
//! to compare with it, and FUN_1418f3520 is detoured to only reset a view. Masters are destroyed
//! with the engine's own routine when the outermost FUN_141931e10 on the thread returns.
//!
//! The results are the engine's own, so the simulation does not change. script_extender.cfg
//! `recruit_perm_cache`: 1 (default) on, 0 off, 2 verify = the engine still builds every map
//! and each one is compared node by node with the master (`perm_same` / `perm_diff` in
//! `se.query.perf()`); nothing is shared in that mode. In mode 1 the first 3000 repeats of a
//! session are verified the same way before sharing starts, and a single difference switches
//! the session to verify only. The key is in the sync tag all the same.
//! Only the outermost list call on a thread is served (a nested call could reuse a stack
//! address for a different description).

use crate::addrs::Table;
use crate::log;
use core::ffi::c_void;
use retour::GenericDetour;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

const MAP_SIZE: usize = 0x50;
const VERIFY_FIRST: u64 = 3000;

type ListCore = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u8);
type MapBuild = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
type MapClear = unsafe extern "C" fn(*mut c_void);

struct Engine {
    alloc: unsafe extern "C" fn(usize, u32) -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
}

struct Master { desc: usize, list: usize, map: Box<[u64; MAP_SIZE / 8]> }

static ENGINE: OnceLock<Engine> = OnceLock::new();
static CORE: OnceLock<GenericDetour<ListCore>> = OnceLock::new();
static BUILD: OnceLock<GenericDetour<MapBuild>> = OnceLock::new();
static CLEAR: OnceLock<GenericDetour<MapClear>> = OnceLock::new();
pub static BUILT: AtomicU64 = AtomicU64::new(0);
pub static SHARED: AtomicU64 = AtomicU64::new(0);
pub static UNSCOPED: AtomicU64 = AtomicU64::new(0);
pub static SAME: AtomicU64 = AtomicU64::new(0);
pub static DIFF: AtomicU64 = AtomicU64::new(0);
pub static MODE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static MASTERS: RefCell<Vec<Master>> = const { RefCell::new(Vec::new()) };
    static VIEWS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

unsafe fn rd(p: usize) -> u32 { core::ptr::read_unaligned(p as *const u32) }
unsafe fn rq(p: usize) -> usize { core::ptr::read_unaligned(p as *const usize) }
unsafe fn wd(p: usize, v: u32) { core::ptr::write_unaligned(p as *mut u32, v) }
unsafe fn wq(p: usize, v: usize) { core::ptr::write_unaligned(p as *mut usize, v) }

/// Copies the master's array {cap @off, count @off+4, data @off+8} of 8-byte elements into the
/// view. False when the engine allocator fails.
unsafe fn copy_array(e: &Engine, master: usize, view: usize, off: usize) -> bool {
    let (count, data) = (rd(master + off + 4) as usize, rq(master + off + 8));
    if count == 0 || data == 0 {
        wd(view + off, 0);
        wd(view + off + 4, 0);
        wq(view + off + 8, 0);
        return true;
    }
    let fresh = (e.alloc)(count * 8, 0) as usize;
    if fresh == 0 { return false; }
    core::ptr::copy_nonoverlapping(data as *const u8, fresh as *mut u8, count * 8);
    wd(view + off, count as u32);
    wd(view + off + 4, count as u32);
    wq(view + off + 8, fresh);
    true
}

/// Node-by-node comparison of two maps built from the same list (same insertion order).
/// Returns what differs first. The 16-byte requirement entries are {requirement*, i32 result,
/// u32 never initialised by the engine (a stack leftover)}: only their first 12 bytes count.
unsafe fn maps_differ(a: usize, b: usize) -> Option<String> {
    for off in [0usize, 0x1c, 0x34, 0x44] {
        if rd(a + off) != rd(b + off) { return Some(format!("map +{off:#x}: {} / {}", rd(a + off), rd(b + off))); }
    }
    for off in [0x30usize, 0x40] {
        let n = rd(a + off + 4) as usize * 8;
        if n != 0 && core::slice::from_raw_parts(rq(a + off + 8) as *const u8, n) != core::slice::from_raw_parts(rq(b + off + 8) as *const u8, n) {
            return Some(format!("record list +{off:#x}"));
        }
    }
    let (mut x, mut y, mut index) = (rq(a + 0x10), rq(b + 0x10), 0usize);
    while x != a + 8 && y != b + 8 && index <= rd(a) as usize {
        if rq(x + 0x10) != rq(y + 0x10) { return Some(format!("node {index}: key {:#x} / {:#x}", rq(x + 0x10), rq(y + 0x10))); }
        for off in [0x18usize, 0x40, 0x41] {
            let (p, q) = (*((x + off) as *const u8), *((y + off) as *const u8));
            if p != q { return Some(format!("node {index} +{off:#x}: {p} / {q}")); }
        }
        for (off, size, used) in [(0x20usize, 8usize, 8usize), (0x30, 8, 8), (0x48, 16, 12), (0x58, 16, 12)] {
            let n = rd(x + off + 4) as usize;
            if n != rd(y + off + 4) as usize { return Some(format!("node {index} vector +{off:#x}: {n} / {} entries", rd(y + off + 4))); }
            let (p, q) = (rq(x + off + 8), rq(y + off + 8));
            for k in 0..n {
                if core::slice::from_raw_parts((p + k * size) as *const u8, used) != core::slice::from_raw_parts((q + k * size) as *const u8, used) {
                    return Some(format!("node {index} vector +{off:#x} entry {k}: {:016x} {:08x} / {:016x} {:08x}", rq(p + k * size), rd(p + k * size + 8), rq(q + k * size), rd(q + k * size + 8)));
                }
            }
        }
        x = rq(x + 8);
        y = rq(y + 8);
        index += 1;
    }
    if x == a + 8 && y == b + 8 { None } else { Some(format!("list length (stopped at node {index})")) }
}

unsafe fn destroy_master(e: &Engine, clear: &GenericDetour<MapClear>, m: Master) {
    let a = m.map.as_ptr() as usize;
    for off in [0x18usize, 0x30, 0x40] {
        let data = rq(a + off + 8);
        if data != 0 { (e.free)(data as *mut c_void); }
    }
    clear.call(a as *mut c_void); // frees the nodes and their vectors
}

unsafe extern "C" fn core_detour(ctx: *mut c_void, out: *mut c_void, mask: u32, flag: u8) {
    let Some(h) = CORE.get() else { return };
    DEPTH.with(|d| d.set(d.get() + 1));
    h.call(ctx, out, mask, flag);
    let outermost = DEPTH.with(|d| { let v = d.get().saturating_sub(1); d.set(v); v == 0 });
    if outermost {
        VIEWS.with(|v| v.borrow_mut().clear());
        let masters = MASTERS.with(|m| core::mem::take(&mut *m.borrow_mut()));
        if let (Some(e), Some(clear)) = (ENGINE.get(), CLEAR.get()) {
            for m in masters { destroy_master(e, clear, m); }
        }
    }
}

unsafe extern "C" fn build_detour(out: *mut c_void, desc: *mut c_void, list: *mut c_void) -> *mut c_void {
    let Some(h) = BUILD.get() else { return out };
    let (Some(e), true) = (ENGINE.get(), DEPTH.with(|d| d.get()) == 1) else {
        UNSCOPED.fetch_add(1, Ordering::Relaxed);
        return h.call(out, desc, list);
    };
    let key = (desc as usize, list as usize);
    // progress in the DLL log, so a test run does not depend on a console script
    let seen = BUILT.load(Ordering::Relaxed) + SHARED.load(Ordering::Relaxed) + SAME.load(Ordering::Relaxed);
    if seen != 0 && seen % 25_000 == 0 {
        log!("recruit permission cache: built {} shared {} verified same {} diff {}", BUILT.load(Ordering::Relaxed), SHARED.load(Ordering::Relaxed), SAME.load(Ordering::Relaxed), DIFF.load(Ordering::Relaxed));
    }
    let known = MASTERS.with(|m| m.borrow().iter().find(|x| (x.desc, x.list) == key).map(|x| x.map.as_ptr() as usize));
    let master = match known {
        // verify mode, and the first VERIFY_FIRST repeats of every session in share mode: let
        // the engine build the map and compare. One difference ends sharing for the session.
        Some(a) if MODE.load(Ordering::Relaxed) == 2 || SAME.load(Ordering::Relaxed) < VERIFY_FIRST => {
            let r = h.call(out, desc, list);
            match maps_differ(a, out as usize) {
                None => { SAME.fetch_add(1, Ordering::Relaxed); }
                Some(what) => {
                    MODE.store(2, Ordering::Relaxed);
                    if DIFF.fetch_add(1, Ordering::Relaxed) < 8 { log!("recruit permission cache: a rebuilt map differs from the first one ({what}; desc {:#x}, list {:#x}); sharing is off for this session", key.0, key.1); }
                }
            }
            return r;
        }
        Some(a) => { SHARED.fetch_add(1, Ordering::Relaxed); a }
        None => {
            let mut map: Box<[u64; MAP_SIZE / 8]> = Box::new([0; MAP_SIZE / 8]);
            let a = map.as_mut_ptr() as usize;
            h.call(a as *mut c_void, desc, list);
            BUILT.fetch_add(1, Ordering::Relaxed);
            MASTERS.with(|m| m.borrow_mut().push(Master { desc: key.0, list: key.1, map }));
            if MODE.load(Ordering::Relaxed) == 2 { return h.call(out, desc, list); }
            a
        }
    };
    // the view: the master's words, own copies of the three arrays, shared nodes
    let view = out as usize;
    core::ptr::copy_nonoverlapping(master as *const u8, view as *mut u8, MAP_SIZE);
    wq(view + 0x20, 0);
    wq(view + 0x38, 0);
    wq(view + 0x48, 0);
    if !(copy_array(e, master, view, 0x18) && copy_array(e, master, view, 0x30) && copy_array(e, master, view, 0x40)) {
        // out of memory: give the caller a map of its own instead
        for off in [0x18usize, 0x30, 0x40] {
            let data = rq(view + off + 8);
            if data != 0 { (e.free)(data as *mut c_void); }
        }
        return h.call(out, desc, list);
    }
    VIEWS.with(|v| v.borrow_mut().push(view));
    out
}

unsafe extern "C" fn clear_detour(map: *mut c_void) {
    let Some(h) = CLEAR.get() else { return };
    let a = map as usize;
    let was_view = VIEWS.with(|v| {
        let mut v = v.borrow_mut();
        match v.iter().rposition(|x| *x == a) { Some(i) => { v.swap_remove(i); true } None => false }
    });
    if was_view {
        // what the engine's routine leaves behind, without touching the shared nodes
        wq(a + 0x10, a + 8);
        wq(a + 8, 0);
        wd(a, 0);
    } else {
        h.call(map);
    }
}

pub fn install(t: &Table) {
    let mode = crate::build::config_value("recruit_perm_cache").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1).min(2);
    if mode == 0 {
        log!("recruit permission cache off (recruit_perm_cache=0)");
        return;
    }
    MODE.store(mode, Ordering::Relaxed);
    let _ = ENGINE.set(unsafe { Engine { alloc: core::mem::transmute(t.get("engine_alloc")), free: core::mem::transmute(t.get("engine_free")) } });
    // SAFETY: the three prologues are anchor-verified stack stores / pushes (nothing
    // RIP-relative in the relocated bytes). Order: the two map routines first (they pass through
    // while no list call is in progress on the thread), the scope last.
    unsafe {
        let core_fn: ListCore = core::mem::transmute(t.get("recruit_list_core"));
        let build: MapBuild = core::mem::transmute(t.get("recruit_perm_map_build"));
        let clear: MapClear = core::mem::transmute(t.get("recruit_perm_map_clear"));
        let (Ok(c), Ok(b), Ok(x)) = (GenericDetour::new(core_fn, core_detour), GenericDetour::new(build, build_detour), GenericDetour::new(clear, clear_detour)) else {
            log!("recruit permission cache: could not create the detours");
            return;
        };
        if crate::freeze::with_threads_frozen(clear as usize, 16, || x.enable()).is_err() {
            log!("recruit permission cache: could not hook the map destructor; off");
            return;
        }
        let _ = CLEAR.set(x);
        if crate::freeze::with_threads_frozen(build as usize, 16, || b.enable()).is_err() {
            log!("recruit permission cache: could not hook the map builder; off");
            return;
        }
        let _ = BUILD.set(b);
        if crate::freeze::with_threads_frozen(core_fn as usize, 16, || c.enable()).is_err() {
            log!("recruit permission cache: could not hook the list routine; off");
            return;
        }
        let _ = CORE.set(c);
    }
    log!("recruit permission cache installed (mode {mode}: {})", if mode == 2 { "verify only" } else { "share" });
}
