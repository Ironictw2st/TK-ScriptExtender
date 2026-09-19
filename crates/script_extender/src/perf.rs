//! Performance: a short-lived cache for the UI's "what can this retinue slot recruit" queries.
//!
//! Profiles (notes/performance.md) show FUN_141934b40 -> FUN_141931e10 -> FUN_1418f2080 as the
//! hottest routine of an open character panel (25% of the main thread) and 11% of an end turn.
//! FUN_141934b40(iface, &out_vec, flag) is vfunc +0x88 of the retinue slot recruitment interface:
//! it rebuilds the full list of recruitable units (one heap item per unit: cost, turns, lock
//! reasons ...) from scratch on every call. The UI asks for it through two CCO getters on every
//! refresh:
//!   FUN_143002050(cco, out)       "can this force recruit anything": walks every slot of every
//!                                 character of the force and builds each slot's full list
//!   FUN_142ed73b0(cco, out, arg)  the recruitable-unit list of the panel
//!
//! Only those UI callers are served from the cache (a thread-local depth counter is raised while
//! one of the two getters runs); the campaign AI and every other caller always get the engine's
//! own computation, so the simulation is untouched and nothing here belongs in the multiplayer
//! sync tag. An entry is served while the owning faction's treasury and the turn number are
//! unchanged (`state_stamp`) and for at most `ui_recruit_cache_ms` (script_extender.cfg, default
//! 5000, 0 = off), which bounds how long the panel can lag behind anything else (a disband, a
//! free recruitment, a script change); commands still validate on the model.
//!
//! The campaign AI (0.33, `ai_recruit_cache`, default 0 = off): the recruitment budget planner
//! FUN_141cf8fe0(planner, ctx) runs five evaluators in a row (FUN_141ce96b0, ..8800, ..6070,
//! ..77a0, ..6700); each walks the faction's forces and asks FUN_141ced9a0 / FUN_141cecf30 "what
//! could this character's empty slots recruit for the remaining budget", which builds the full
//! list per slot again (20% of the main thread during an end turn). The planner only allocates
//! budgets, it does not recruit, so inside ONE planner call a slot's list cannot change. The AI
//! cache therefore lives exactly as long as one planner call (cleared on entry and exit) and is
//! additionally guarded by the state stamp and the slot identity.
//!   ai_recruit_cache=1  verify: the engine still builds every list; a repeated query is
//!                       compared with the remembered list (counters `ai_same` / `ai_diff`).
//!                       Changes nothing, proves (or disproves) exactness on a real campaign.
//!   ai_recruit_cache=2  serve repeated queries from the cache.
//! The key is part of `build::sync_tag` (multiplayer: both machines must agree).
//!
//! Item = 0x98 bytes, vtable RVA 0x349c220 (the same class `recruit.rs` reads): +8 record,
//! +0x10 cost, +0x14 turns, +0x18, +0x1c, +0x20 vector of 16-byte PODs, +0x30 u32, +0x38 u8,
//! +0x40 / +0x50 vectors of 8-byte PODs, +0x60 / +0x61 u8, +0x68 / +0x78 vectors of 16-byte PODs,
//! +0x88 u64, +0x90 / +0x94 u32. Vectors are {u32 cap, u32 count, data*} from the engine
//! allocator; the item is freed through its vtable slot 0 (item, 1). A result is cached only if
//! every item has that vtable; clones are bitwise copies with freshly allocated vector data.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ITEM_SIZE: usize = 0x98;
const RVA_ITEM_VTABLE: usize = 0x349c220;
/// (offset of the vector inside the item, element size)
const ITEM_VECTORS: [(usize, usize); 5] = [(0x20, 16), (0x40, 8), (0x50, 8), (0x68, 16), (0x78, 16)];
const MAX_ITEMS: usize = 600;
const MAX_ENTRIES: usize = 512;

type BuildList = unsafe extern "C" fn(*mut c_void, *mut c_void, u8);
type Planner = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;
type Getter2 = unsafe extern "C" fn(*mut c_void, *mut c_void);
type Getter3 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;

struct Engine {
    alloc: unsafe extern "C" fn(usize, u32) -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
    base: usize,
}

struct Entry { at: Instant, generation: u64, ident: usize, items: Vec<usize> }

/// State stamp of a query: the owning faction's treasury and the turn number. Those are what a
/// recruitment changes (cost) and what a new turn changes; selection / camera commands, which
/// made a command-buffer based invalidation useless (0.32.1: every panel open invalidated
/// everything), do not touch them. Chain, from FUN_141934b40: character =
/// `*(*(*(iface+8)+0x48)+0x48)`, details = `**(character+0x260)`, owner = `**(details+0x68)`,
/// faction = `**(owner+0x270)` (checked by its progression back-pointer at +0x2290), treasury =
/// i32 at faction+0x2a8+0x880 (FUN_141461c80), turn = `*(*(world+0x3b78)+0x5c)`.
/// Returns 0 when any link is missing (then only a 250 ms time-to-live is used).
unsafe fn state_stamp(iface: usize) -> u64 {
    let ptr = |p: usize, off: usize| -> usize { if p != 0 && readable(p + off, 8) { rq(p + off) } else { 0 } };
    let character = ptr(ptr(ptr(iface, 8), 0x48), 0x48);
    let details = ptr(ptr(character, 0x260), 0);
    let owner = ptr(ptr(details, 0x68), 0);
    let faction = ptr(ptr(owner, 0x270), 0);
    if faction == 0 || !readable(faction, 0x2298) || rq(faction + 0x2290) != faction { return 0; }
    let treasury = rd(faction + 0x2a8 + 0x880) as u64;
    let world = ptr(ptr(faction, 0x288), 0x78);
    let turn_obj = ptr(world, 0x3b78);
    let turn = if turn_obj != 0 && readable(turn_obj + 0x5c, 4) { rd(turn_obj + 0x5c) as u64 } else { 0 };
    (turn << 32 | treasury) | 1 << 63 // never 0 when available
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
static BUILD: OnceLock<GenericDetour<BuildList>> = OnceLock::new();
static GET2: OnceLock<GenericDetour<Getter2>> = OnceLock::new();
static GET3: OnceLock<GenericDetour<Getter3>> = OnceLock::new();
struct AiEntry { stamp: u64, ident: usize, items: Vec<usize> }
static AI_CACHE: Mutex<Option<HashMap<(usize, u8), AiEntry>>> = Mutex::new(None);
static AI_MODE: AtomicU64 = AtomicU64::new(0);
static AI_SCOPES: AtomicU64 = AtomicU64::new(0);
static AI_CALLS: AtomicU64 = AtomicU64::new(0);
static AI_SAME: AtomicU64 = AtomicU64::new(0);
static AI_DIFF: AtomicU64 = AtomicU64::new(0);
static AI_SERVED: AtomicU64 = AtomicU64::new(0);
static AI_DIFF_LOGGED: AtomicU64 = AtomicU64::new(0);
static PLANNER: OnceLock<GenericDetour<Planner>> = OnceLock::new();
static CACHE: Mutex<Option<HashMap<(usize, u8), Entry>>> = Mutex::new(None);
static TTL_MS: AtomicU64 = AtomicU64::new(5000);
static LAST_GENERATION: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static PASSED: AtomicU64 = AtomicU64::new(0);
// why a query missed: never seen, time-to-live over, state stamp / slot identity changed, and
// how often the stamp chain could not be followed
static MISS_NEW: AtomicU64 = AtomicU64::new(0);
static MISS_EXPIRED: AtomicU64 = AtomicU64::new(0);
static MISS_STATE: AtomicU64 = AtomicU64::new(0);
static STAMP_FAILED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static UI_DEPTH: Cell<u32> = const { Cell::new(0) };
    static AI_DEPTH: Cell<u32> = const { Cell::new(0) };
}

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool {
    p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0
}
unsafe fn rq(p: usize) -> usize { core::ptr::read_unaligned(p as *const usize) }
unsafe fn rd(p: usize) -> u32 { core::ptr::read_unaligned(p as *const u32) }

unsafe fn is_plain_item(e: &Engine, item: usize) -> bool {
    if !readable(item, ITEM_SIZE) || rq(item).wrapping_sub(e.base) != RVA_ITEM_VTABLE { return false; }
    ITEM_VECTORS.iter().all(|(off, size)| {
        let (count, data) = (rd(item + off + 4) as usize, rq(item + off + 8));
        count <= 4096 && (count == 0 || readable(data, count * size))
    })
}

/// Bitwise copy with freshly allocated vector data (capacity = count, like the engine's copies).
unsafe fn clone_item(e: &Engine, src: usize) -> Option<usize> {
    let dst = (e.alloc)(ITEM_SIZE, 0) as usize;
    if dst == 0 { return None; }
    core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, ITEM_SIZE);
    for (off, size) in ITEM_VECTORS {
        let (count, data) = (rd(src + off + 4) as usize, rq(src + off + 8));
        let fresh = if count == 0 { 0 } else { (e.alloc)(count * size, 0) as usize };
        if count != 0 {
            if fresh == 0 { return None; }
            core::ptr::copy_nonoverlapping(data as *const u8, fresh as *mut u8, count * size);
        }
        core::ptr::write_unaligned((dst + off) as *mut u32, count as u32);
        core::ptr::write_unaligned((dst + off + 8) as *mut usize, fresh);
    }
    Some(dst)
}

unsafe fn delete_item(item: usize) {
    let dtor: unsafe extern "C" fn(*mut c_void, u32) = core::mem::transmute(rq(rq(item)));
    dtor(item as *mut c_void, 1);
}

/// Appends to the caller's vector {u32 cap, u32 count, data*} of item pointers the way the
/// engine does (doubling growth through the engine allocator).
unsafe fn push_item(e: &Engine, vec: usize, item: usize) -> bool {
    let (cap, count, data) = (rd(vec) as usize, rd(vec + 4) as usize, rq(vec + 8));
    let mut data = data;
    if count >= cap {
        let new_cap = (cap * 2).max(1);
        let fresh = (e.alloc)(new_cap * 8, 0) as usize;
        if fresh == 0 { return false; }
        if count > 0 { core::ptr::copy_nonoverlapping(data as *const u8, fresh as *mut u8, count * 8); }
        if data != 0 { (e.free)(data as *mut c_void); }
        core::ptr::write_unaligned(vec as *mut u32, new_cap as u32);
        core::ptr::write_unaligned((vec + 8) as *mut usize, fresh);
        data = fresh;
    }
    core::ptr::write_unaligned((data + count * 8) as *mut usize, item);
    core::ptr::write_unaligned((vec + 4) as *mut u32, (count + 1) as u32);
    true
}

/// Field-wise comparison of two items: everything except the vectors' capacity and data pointer,
/// plus the vectors' contents. Returns the first differing offset.
unsafe fn item_diff(a: usize, b: usize) -> Option<usize> {
    // the known fields only (offset, size): padding after the u8 fields is never initialised
    const FIELDS: [(usize, usize); 16] = [(0, 8), (8, 8), (0x10, 4), (0x14, 4), (0x18, 4), (0x1c, 4), (0x24, 4), (0x30, 4),
        (0x38, 1), (0x44, 4), (0x54, 4), (0x60, 2), (0x6c, 4), (0x7c, 4), (0x88, 8), (0x90, 8)];
    for (o, n) in FIELDS {
        if core::slice::from_raw_parts((a + o) as *const u8, n) != core::slice::from_raw_parts((b + o) as *const u8, n) { return Some(o); }
    }
    for (off, size) in ITEM_VECTORS {
        let n = rd(a + off + 4) as usize * size; // counts are equal here
        if n != 0 && core::slice::from_raw_parts(rq(a + off + 8) as *const u8, n) != core::slice::from_raw_parts(rq(b + off + 8) as *const u8, n) {
            return Some(off + 8);
        }
    }
    None
}

unsafe fn ai_clear() {
    if let Ok(mut guard) = AI_CACHE.lock() {
        if let Some(map) = guard.as_mut() {
            for (_, v) in map.drain() { for c in v.items { delete_item(c); } }
        }
    }
}

unsafe extern "C" fn planner_detour(planner: *mut c_void, ctx: *mut c_void) -> u64 {
    let Some(h) = PLANNER.get() else { return 1 };
    let outer = AI_DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v == 0 });
    if outer { ai_clear(); AI_SCOPES.fetch_add(1, Ordering::Relaxed); }
    let r = h.call(planner, ctx);
    AI_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    if outer {
        ai_clear();
        // the counters also go to the DLL log (at most every 15 s), so a run needs no script
        static LAST_REPORT: Mutex<Option<Instant>> = Mutex::new(None);
        if let Ok(mut last) = LAST_REPORT.lock() {
            if last.map_or(true, |t| t.elapsed() > Duration::from_secs(15)) {
                *last = Some(Instant::now());
                log!("ai recruit cache: scopes {} calls {} same {} diff {} served {}", AI_SCOPES.load(Ordering::Relaxed), AI_CALLS.load(Ordering::Relaxed),
                    AI_SAME.load(Ordering::Relaxed), AI_DIFF.load(Ordering::Relaxed), AI_SERVED.load(Ordering::Relaxed));
            }
        }
    }
    r
}

/// Clones of what the engine appended to `vec` after index `before`; None when the result is
/// not something this module understands.
unsafe fn clone_appended(e: &Engine, vec: usize, before: usize) -> Option<Vec<usize>> {
    let (after, data) = (rd(vec + 4) as usize, rq(vec + 8));
    if after < before || after - before > MAX_ITEMS || (after > 0 && !readable(data, after * 8)) { return None; }
    let mut clones = Vec::with_capacity(after - before);
    for i in before..after {
        let item = rq(data + i * 8);
        let cloned = if is_plain_item(e, item) { clone_item(e, item) } else { None };
        match cloned {
            Some(c) => clones.push(c),
            None => { for c in clones { delete_item(c); } return None; }
        }
    }
    Some(clones)
}

/// A query made by the AI's recruitment budget planner (see the module comment).
unsafe fn ai_query(e: &Engine, h: &GenericDetour<BuildList>, iface: *mut c_void, out: *mut c_void, flag: u8, mode: u64) {
    AI_CALLS.fetch_add(1, Ordering::Relaxed);
    let (key, vec) = ((iface as usize, flag), out as usize);
    let stamp = state_stamp(iface as usize);
    let ident = if readable(iface as usize + 8, 8) { rq(iface as usize + 8) } else { 0 };
    let before = rd(vec + 4) as usize;
    if stamp == 0 || ident == 0 { h.call(iface, out, flag); return; }
    if mode >= 2 {
        if let Ok(guard) = AI_CACHE.lock() {
            if let Some(entry) = guard.as_ref().and_then(|m| m.get(&key)).filter(|en| en.stamp == stamp && en.ident == ident) {
                let mut ok = true;
                for item in &entry.items {
                    match clone_item(e, *item) {
                        Some(c) => if !push_item(e, vec, c) { delete_item(c); ok = false; break; },
                        None => { ok = false; break; }
                    }
                }
                if ok { AI_SERVED.fetch_add(1, Ordering::Relaxed); return; }
                // allocation failed mid-way: the engine appends after what was pushed; the
                // planner only takes minima and counts over affordable items
            }
        }
    }
    h.call(iface, out, flag);
    let Some(clones) = clone_appended(e, vec, before) else { return };
    let Ok(mut guard) = AI_CACHE.lock() else { for c in clones { delete_item(c); } return };
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(entry) = map.get(&key).filter(|en| en.stamp == stamp && en.ident == ident) {
        // verify mode: the same query again inside one planner call
        let mut diff = if entry.items.len() != clones.len() { Some((usize::MAX, 0)) } else { None };
        if diff.is_none() {
            for (i, (a, b)) in entry.items.iter().zip(clones.iter()).enumerate() {
                if let Some(o) = item_diff(*a, *b) { diff = Some((i, o)); break; }
            }
        }
        match diff {
            None => { AI_SAME.fetch_add(1, Ordering::Relaxed); }
            Some((i, o)) => {
                AI_DIFF.fetch_add(1, Ordering::Relaxed);
                if AI_DIFF_LOGGED.fetch_add(1, Ordering::Relaxed) < 12 {
                    log!("ai recruit cache: repeated query differs: iface {:#x} flag {flag} items {} -> {} first difference item {i} offset {o:#x}", iface as usize, entry.items.len(), clones.len());
                }
            }
        }
    }
    if map.len() >= 4096 { for c in clones { delete_item(c); } return; }
    if let Some(v) = map.insert(key, AiEntry { stamp, ident, items: clones }) { for c in v.items { delete_item(c); } }
}

unsafe extern "C" fn get2_detour(cco: *mut c_void, out: *mut c_void) {
    let Some(h) = GET2.get() else { return };
    UI_DEPTH.with(|d| d.set(d.get() + 1));
    h.call(cco, out);
    UI_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
}

unsafe extern "C" fn get3_detour(cco: *mut c_void, out: *mut c_void, arg: *mut c_void) -> *mut c_void {
    let Some(h) = GET3.get() else { return out };
    UI_DEPTH.with(|d| d.set(d.get() + 1));
    let r = h.call(cco, out, arg);
    UI_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    r
}

unsafe extern "C" fn build_detour(iface: *mut c_void, out: *mut c_void, flag: u8) {
    let Some(h) = BUILD.get() else { return };
    let ai_mode = AI_MODE.load(Ordering::Relaxed);
    if ai_mode > 0 && AI_DEPTH.with(|d| d.get()) > 0 && UI_DEPTH.with(|d| d.get()) == 0 && readable(out as usize, 16) {
        if let Some(e) = ENGINE.get() { ai_query(e, h, iface, out, flag, ai_mode); return; }
    }
    let ttl = TTL_MS.load(Ordering::Relaxed);
    let (Some(e), true) = (ENGINE.get(), ttl > 0 && UI_DEPTH.with(|d| d.get()) > 0 && readable(out as usize, 16)) else {
        PASSED.fetch_add(1, Ordering::Relaxed);
        h.call(iface, out, flag);
        return;
    };
    let key = (iface as usize, flag);
    let vec = out as usize;
    let now = Instant::now();
    let generation = state_stamp(iface as usize);
    LAST_GENERATION.store(generation & 0xffff_ffff, Ordering::Relaxed);
    // without the state stamp only a short time-to-live is safe
    let ttl = if generation == 0 { STAMP_FAILED.fetch_add(1, Ordering::Relaxed); ttl.min(250) } else { ttl };
    // the interface address is the key; what it points at guards against an address reused by
    // another slot's interface
    let ident = if readable(iface as usize + 8, 8) { rq(iface as usize + 8) } else { 0 };
    // hit: hand out clones
    if let Ok(mut guard) = CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        match map.get(&key) {
            None => { MISS_NEW.fetch_add(1, Ordering::Relaxed); }
            Some(entry) if entry.generation != generation || entry.ident != ident => { MISS_STATE.fetch_add(1, Ordering::Relaxed); }
            Some(entry) if now.duration_since(entry.at) >= Duration::from_millis(ttl) => { MISS_EXPIRED.fetch_add(1, Ordering::Relaxed); }
            Some(entry) => {
                let mut ok = true;
                for item in &entry.items {
                    match clone_item(e, *item) {
                        Some(c) => if !push_item(e, vec, c) { delete_item(c); ok = false; break; },
                        None => { ok = false; break; }
                    }
                }
                if ok { HITS.fetch_add(1, Ordering::Relaxed); return; }
                // allocation failed mid-way: fall through to the engine (it appends after what
                // was pushed; the caller only tests the items, duplicates are harmless)
            }
        }
    }
    // miss: let the engine build the list, then keep clones of what it appended
    let before = rd(vec + 4) as usize;
    h.call(iface, out, flag);
    MISSES.fetch_add(1, Ordering::Relaxed);
    let (after, data) = (rd(vec + 4) as usize, rq(vec + 8));
    if after < before || after - before > MAX_ITEMS || (after > 0 && !readable(data, after * 8)) { return; }
    let mut clones = Vec::with_capacity(after - before);
    for i in before..after {
        let item = rq(data + i * 8);
        let cloned = if is_plain_item(e, item) { clone_item(e, item) } else { None };
        match cloned {
            Some(c) => clones.push(c),
            None => { for c in clones { delete_item(c); } return; }
        }
    }
    if let Ok(mut guard) = CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if map.len() >= MAX_ENTRIES {
            let old: Vec<(usize, u8)> = map.iter().filter(|(_, v)| now.duration_since(v.at) > Duration::from_secs(5)).map(|(k, _)| *k).collect();
            for k in old { if let Some(v) = map.remove(&k) { for c in v.items { delete_item(c); } } }
            if map.len() >= MAX_ENTRIES { for c in clones { delete_item(c); } return; }
        }
        if let Some(v) = map.insert(key, Entry { at: now, generation, ident, items: clones }) { for c in v.items { delete_item(c); } }
    }
}

pub fn install(t: &Table) {
    let ttl = crate::build::config_value("ui_recruit_cache_ms").and_then(|v| v.parse::<u64>().ok()).unwrap_or(5000).min(30000);
    TTL_MS.store(ttl, Ordering::Relaxed);
    let ai_mode = crate::build::config_value("ai_recruit_cache").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0).min(2);
    if ttl == 0 && ai_mode == 0 {
        log!("recruit caches off (ui_recruit_cache_ms=0, ai_recruit_cache=0)");
        return;
    }
    let (base, _) = crate::process::main_module();
    let _ = ENGINE.set(unsafe { Engine { alloc: core::mem::transmute(t.get("engine_alloc")), free: core::mem::transmute(t.get("engine_free")), base } });
    // SAFETY: all three prologues are anchor-verified and start with pushes / stack stores and a
    // sub rsp (no RIP-relative instruction in the relocated bytes).
    unsafe {
        let build: BuildList = core::mem::transmute(t.get("recruit_list_build"));
        let get2: Getter2 = core::mem::transmute(t.get("cco_can_recruit_any"));
        let get3: Getter3 = core::mem::transmute(t.get("cco_recruit_list"));
        let (Ok(b), Ok(g2), Ok(g3)) = (GenericDetour::new(build, build_detour), GenericDetour::new(get2, get2_detour), GenericDetour::new(get3, get3_detour)) else {
            log!("ui recruit cache: could not create the detours");
            return;
        };
        // the list builder last, so it never sees a UI depth the getters are not maintaining yet
        let r2 = crate::freeze::with_threads_frozen(get2 as usize, 16, || g2.enable());
        let r3 = crate::freeze::with_threads_frozen(get3 as usize, 16, || g3.enable());
        if r2.is_err() || r3.is_err() {
            log!("ui recruit cache: could not enable the getter detours; cache stays off");
            return;
        }
        let _ = GET2.set(g2);
        let _ = GET3.set(g3);
        if crate::freeze::with_threads_frozen(build as usize, 16, || b.enable()).is_err() {
            log!("ui recruit cache: could not enable the list detour; cache stays off");
            return;
        }
        let _ = BUILD.set(b);
        if ai_mode > 0 {
            // prologue: mov rax,rsp / two stack stores / pushes (nothing RIP-relative)
            let planner: Planner = core::mem::transmute(t.get("cai_recruit_budget"));
            match GenericDetour::new(planner, planner_detour) {
                Ok(p) if crate::freeze::with_threads_frozen(planner as usize, 16, || p.enable()).is_ok() => {
                    let _ = PLANNER.set(p);
                    AI_MODE.store(ai_mode, Ordering::Relaxed);
                    log!("ai recruit cache installed (mode {ai_mode}: {})", if ai_mode == 1 { "verify only" } else { "serve" });
                }
                _ => log!("ai recruit cache: could not hook the planner; off"),
            }
        }
    }
    log!("ui recruit cache installed (ttl {ttl} ms)");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_perf_stats", se_perf_stats);
}

/// se_perf_stats() -> "k=v;..." counters of the UI recruit cache
unsafe extern "C" fn se_perf_stats(l: *mut LuaState) -> c_int {
    let entries = CACHE.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0);
    let s = format!("installed={};ttl_ms={};last_treasury_seen={};hits={};misses={};passed_through={};entries={entries};miss_new={};miss_expired={};miss_state={};stamp_failed={};ai_mode={};ai_scopes={};ai_calls={};ai_same={};ai_diff={};ai_served={};perm_mode={};perm_built={};perm_shared={};perm_same={};perm_diff={};perm_unscoped={}",
        BUILD.get().is_some() as u8, TTL_MS.load(Ordering::Relaxed), LAST_GENERATION.load(Ordering::Relaxed), HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed), PASSED.load(Ordering::Relaxed),
        MISS_NEW.load(Ordering::Relaxed), MISS_EXPIRED.load(Ordering::Relaxed), MISS_STATE.load(Ordering::Relaxed), STAMP_FAILED.load(Ordering::Relaxed),
        AI_MODE.load(Ordering::Relaxed), AI_SCOPES.load(Ordering::Relaxed), AI_CALLS.load(Ordering::Relaxed), AI_SAME.load(Ordering::Relaxed), AI_DIFF.load(Ordering::Relaxed), AI_SERVED.load(Ordering::Relaxed),
        crate::permcache::MODE.load(Ordering::Relaxed), crate::permcache::BUILT.load(Ordering::Relaxed), crate::permcache::SHARED.load(Ordering::Relaxed),
        crate::permcache::SAME.load(Ordering::Relaxed), crate::permcache::DIFF.load(Ordering::Relaxed), crate::permcache::UNSCOPED.load(Ordering::Relaxed));
    lua::push_str(l, &s);
    1
}
