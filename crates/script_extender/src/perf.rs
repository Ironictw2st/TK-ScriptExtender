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
//! sync tag. An entry is served while no change of the UI's command buffer has been observed
//! (player actions) and for at most `ui_recruit_cache_ms` (script_extender.cfg, default 5000,
//! 0 = off), which bounds how long the panel can lag behind anything else; commands still
//! validate on the model. (0.32.0 used a bare 250 ms time-to-live: the UI re-queries a slot when
//! a panel is (re)built, seconds apart, so only 9% of the queries hit.)
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
type Getter2 = unsafe extern "C" fn(*mut c_void, *mut c_void);
type Getter3 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;

struct Engine {
    alloc: unsafe extern "C" fn(usize, u32) -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
    base: usize,
}

struct Entry { at: Instant, generation: u32, items: Vec<usize> }

/// Player actions reach the model as commands serialised into a byte buffer owned by
/// `*(*(ui + 0x2188) + 0x80)` (ui = the global at RVA 0x43cfa50 the CCO code uses, e.g.
/// CcoCampaignBattle.AutoResolve); its senders (FUN_142e6eab0 and template siblings) advance the
/// write offset at owner+0x5008, and a flush rewinds it. The offset is therefore not monotonic:
/// every observed CHANGE bumps our own epoch, and a cached list is only served within the epoch
/// it was built in. A write and flush between two observations can be missed, which is why the
/// time-to-live stays in place as the upper bound on staleness. Returns 0 when unavailable.
const RVA_UI_ROOT: usize = 0x43cfa50;
static LAST_OFFSET: AtomicU64 = AtomicU64::new(u64::MAX);
static EPOCH: AtomicU64 = AtomicU64::new(1);
unsafe fn command_generation(base: usize) -> u32 {
    let ui = rq(base + RVA_UI_ROOT);
    if !readable(ui + 0x2188, 8) { return 0; }
    let owner = rq(ui + 0x2188);
    if !readable(owner + 0x80, 8) { return 0; }
    let queue = rq(owner + 0x80);
    if !readable(queue + 0x5008, 4) { return 0; }
    let offset = rd(queue + 0x5008) as u64;
    if LAST_OFFSET.swap(offset, Ordering::Relaxed) != offset {
        EPOCH.fetch_add(1, Ordering::Relaxed);
    }
    (EPOCH.load(Ordering::Relaxed) & 0x7fff_ffff) as u32 | 1 << 31 // never 0 when available
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
static BUILD: OnceLock<GenericDetour<BuildList>> = OnceLock::new();
static GET2: OnceLock<GenericDetour<Getter2>> = OnceLock::new();
static GET3: OnceLock<GenericDetour<Getter3>> = OnceLock::new();
static CACHE: Mutex<Option<HashMap<(usize, u8), Entry>>> = Mutex::new(None);
static TTL_MS: AtomicU64 = AtomicU64::new(5000);
static LAST_GENERATION: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static PASSED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static UI_DEPTH: Cell<u32> = const { Cell::new(0) };
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
    let ttl = TTL_MS.load(Ordering::Relaxed);
    let (Some(e), true) = (ENGINE.get(), ttl > 0 && UI_DEPTH.with(|d| d.get()) > 0 && readable(out as usize, 16)) else {
        PASSED.fetch_add(1, Ordering::Relaxed);
        h.call(iface, out, flag);
        return;
    };
    let key = (iface as usize, flag);
    let vec = out as usize;
    let now = Instant::now();
    let generation = command_generation(e.base);
    LAST_GENERATION.store(generation as u64, Ordering::Relaxed);
    // without the command counter only a short time-to-live is safe
    let ttl = if generation == 0 { ttl.min(250) } else { ttl };
    // hit: hand out clones
    if let Ok(mut guard) = CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(entry) = map.get(&key) {
            if entry.generation == generation && now.duration_since(entry.at) < Duration::from_millis(ttl) {
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
        if let Some(v) = map.insert(key, Entry { at: now, generation, items: clones }) { for c in v.items { delete_item(c); } }
    }
}

pub fn install(t: &Table) {
    let ttl = crate::build::config_value("ui_recruit_cache_ms").and_then(|v| v.parse::<u64>().ok()).unwrap_or(5000).min(30000);
    TTL_MS.store(ttl, Ordering::Relaxed);
    if ttl == 0 {
        log!("ui recruit cache off (ui_recruit_cache_ms=0)");
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
    }
    log!("ui recruit cache installed (ttl {ttl} ms)");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_perf_stats", se_perf_stats);
}

/// se_perf_stats() -> "k=v;..." counters of the UI recruit cache
unsafe extern "C" fn se_perf_stats(l: *mut LuaState) -> c_int {
    let entries = CACHE.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0);
    let s = format!("installed={};ttl_ms={};command_counter={};hits={};misses={};passed_through={};entries={entries}",
        BUILD.get().is_some() as u8, TTL_MS.load(Ordering::Relaxed), LAST_GENERATION.load(Ordering::Relaxed), HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed), PASSED.load(Ordering::Relaxed));
    lua::push_str(l, &s);
    1
}
