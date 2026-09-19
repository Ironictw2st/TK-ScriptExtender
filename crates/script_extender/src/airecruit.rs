//! The campaign AI's recruitment, made visible (notes/ai_recruitment.md). Read-only.
//!
//! `CAI_TASK_RECRUITMENT_PREFERENCE_ANALYSIS` (vtable 0x1434bddc8, task id 0xA2) runs
//! FUN_141cf8fe0(task, ctx) = its vtable slot +0x58 once per planning pass. It rebuilds the
//! task's request list and spreads two budgets over it:
//!   task +0x120 CAI faction object, +0x12c request count, +0x130 requests (0x48 bytes each):
//!        +0 target object (a CAI force / region wrapper), +0x18 i32 money, +0x1c i32 second
//!        budget, +0x20 u32 turn stamp, +0x28 u32 row count, +0x30 rows of 0x1c bytes
//!        {i32 id, i32 cost, i32 cost2, 3 x i32, u8 kind}
//!   task +0x188 pending-order countdown
//! The pricing helpers under it (FUN_141ced9a0 / FUN_141cecf30) only ever look at EMPTY retinue
//! slots, which is why the AI does not upgrade units.
//!
//! This module owns the detour on FUN_141cf8fe0 (`perf.rs` hangs its AI scope on it) and, while
//! tracing is on, copies the request list after every pass.
//!
//! se_ai_recruit_trace(on) -> was_on            switch the copying on / off (default off)
//! se_ai_recruit_passes()  -> string            drains the collected passes:
//!     "P,<seq>,<faction id>,<pending>;R,<money>,<budget2>,<turn>,<target vtable rva hex>;
//!      W,<id>,<cost>,<cost2>,<kind>;..."       (P = pass, R = request, W = row of the request)
//! se_unit_quality(q_faction, unit_key) -> string "group=quality,quality_at_max_xp;..." from the
//!     live `cdir_military_generator_unit_qualities` table (the AI's own unit ranking): accessor
//!     FUN_1408cad20(db) -> I_DATABASE_TABLE with 0x20-byte records in the vector at table+0x10
//!     {cap, count @+0x14, data @+0x18}; a record holds the group record, the land unit record
//!     and two numbers. The record layout is detected per call (pointer slots whose record key
//!     reads as text, numbers from the remaining words) and the first records are dumped to the
//!     DLL log once, so the layout can be pinned down from a live session.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

type Planner = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;

struct Engine {
    db_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    qualities_table: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    base: usize,
}

struct Row { id: i32, cost: i32, cost2: i32, kind: u8 }
struct Request { target_vt: usize, money: i32, budget2: i32, turn: u32, rows: Vec<Row> }
struct Pass { seq: u64, faction_id: u32, pending: i32, requests: Vec<Request> }

static ENGINE: OnceLock<Engine> = OnceLock::new();
static HOOK: OnceLock<GenericDetour<Planner>> = OnceLock::new();
static TRACE: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);
static PASSES: Mutex<VecDeque<Pass>> = Mutex::new(VecDeque::new());
static DUMPED: AtomicBool = AtomicBool::new(false);

const MAX_PASSES: usize = 512;

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool {
    p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0
}
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

/// The engine FACTION behind the CAI faction object: the first pointer in it whose target has
/// the progression manager pointing back at it (the check `progression.rs` uses). 0 = unknown.
unsafe fn faction_id_of(cai_faction: usize) -> u32 {
    if !readable(cai_faction, 0x200) { return 0; }
    for off in (0..0x200).step_by(8) {
        let f = core::ptr::read_unaligned((cai_faction + off) as *const usize);
        if f > 0x10000 && f % 8 == 0 && readable(f, 0x2298) && rq(f + 0x2290) == f { return rd(f + 8); }
    }
    0
}

unsafe fn snapshot(task: usize, base: usize) {
    if !readable(task, 0x190) { return; }
    let (count, data) = (rd(task + 0x12c) as usize, rq(task + 0x130));
    if count > 256 || (count > 0 && !readable(data, count * 0x48)) { return; }
    let mut requests = Vec::with_capacity(count);
    for i in 0..count {
        let e = data + i * 0x48;
        let (nrows, rows_ptr) = (rd(e + 0x28) as usize, rq(e + 0x30));
        let mut rows = Vec::new();
        if nrows <= 64 && (nrows == 0 || readable(rows_ptr, nrows * 0x1c)) {
            for k in 0..nrows {
                let r = rows_ptr + k * 0x1c;
                rows.push(Row { id: rd(r) as i32, cost: rd(r + 4) as i32, cost2: rd(r + 8) as i32, kind: *((r + 0x18) as *const u8) });
            }
        }
        let target = rq(e);
        requests.push(Request { target_vt: rq(target).wrapping_sub(base), money: rd(e + 0x18) as i32, budget2: rd(e + 0x1c) as i32, turn: rd(e + 0x20), rows });
    }
    let pass = Pass { seq: SEQ.fetch_add(1, Ordering::Relaxed), faction_id: faction_id_of(rq(task + 0x120)), pending: rd(task + 0x188) as i32, requests };
    if let Ok(mut q) = PASSES.lock() {
        if q.len() >= MAX_PASSES { q.pop_front(); }
        q.push_back(pass);
    }
}

unsafe extern "C" fn planner_detour(task: *mut c_void, ctx: *mut c_void) -> u64 {
    let Some(h) = HOOK.get() else { return 1 };
    let outer = crate::perf::ai_scope_enter();
    let r = h.call(task, ctx);
    crate::perf::ai_scope_exit(outer);
    if TRACE.load(Ordering::Relaxed) {
        if let Some(e) = ENGINE.get() { snapshot(task as usize, e.base); }
    }
    r
}

pub fn install(t: &Table) {
    let (base, _) = crate::process::main_module();
    let _ = ENGINE.set(unsafe { Engine { db_get: core::mem::transmute(t.get("db_get")), qualities_table: core::mem::transmute(t.get("unit_qualities_table")), base } });
    // SAFETY: anchor-verified prologue `mov rax,rsp` / stack stores / pushes.
    unsafe {
        let planner: Planner = core::mem::transmute(t.get("cai_recruit_budget"));
        let Ok(d) = GenericDetour::new(planner, planner_detour) else {
            log!("ai recruitment trace: could not create the planner detour");
            return;
        };
        let _ = HOOK.set(d);
        let Some(d) = HOOK.get() else { return };
        if crate::freeze::with_threads_frozen(planner as usize, 16, || d.enable()).is_err() {
            log!("ai recruitment trace: could not hook the planner");
            return;
        }
    }
    log!("ai recruitment planner hook installed (trace off until a script asks for it)");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_ai_recruit_trace", se_ai_recruit_trace);
    lua::set_global_fn(l, "se_ai_recruit_passes", se_ai_recruit_passes);
    lua::set_global_fn(l, "se_unit_quality", se_unit_quality);
}

unsafe extern "C" fn se_ai_recruit_trace(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let on = (api.gettop)(l) >= 1 && (api.toboolean)(l, 1) != 0;
    let was = TRACE.swap(on, Ordering::Relaxed);
    if !on { if let Ok(mut q) = PASSES.lock() { q.clear(); } }
    (api.pushboolean)(l, was as c_int);
    1
}

unsafe extern "C" fn se_ai_recruit_passes(l: *mut LuaState) -> c_int {
    let passes: Vec<Pass> = PASSES.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
    let mut s = String::new();
    for p in passes {
        s.push_str(&format!("P,{},{},{};", p.seq, p.faction_id, p.pending));
        for r in p.requests {
            s.push_str(&format!("R,{},{},{},{:x};", r.money, r.budget2, r.turn, r.target_vt));
            for w in r.rows { s.push_str(&format!("W,{},{},{},{};", w.id, w.cost, w.cost2, w.kind)); }
        }
    }
    lua::push_str(l, &s);
    1
}

/// A number stored either as f32 or as integer (the engine has both kinds of columns).
fn number(bits: u32) -> f32 {
    let f = f32::from_bits(bits);
    if f.is_finite() && (f == 0.0 || (f.abs() >= 1.0e-3 && f.abs() < 1.0e9)) { f } else { bits as i32 as f32 }
}

unsafe extern "C" fn se_unit_quality(l: *mut LuaState) -> c_int {
    let fail = |l: *mut LuaState, msg: &str| -> c_int { lua::push_str(l, ""); lua::push_str(l, msg); 2 };
    let Some(e) = ENGINE.get() else { return fail(l, "engine table missing") };
    let faction = match crate::progression::faction_from_arg(l, 1) { Ok(f) => f, Err(m) => return fail(l, &m) };
    let key = lua::to_str(l, 2);
    let world = rq(rq(faction + 0x288) + 0x78);
    if world == 0 || !readable(world + 0x3b38, 8) { return fail(l, "campaign model not reachable from the faction") }
    let db = (e.db_get)((world + 0x3b38) as *mut c_void);
    if db.is_null() { return fail(l, "database not available") }
    let table = (e.qualities_table)(db) as usize;
    if !readable(table, 0x30) { return fail(l, "unit qualities table not available") }
    let (count, data) = (rd(table + 0x14) as usize, rq(table + 0x18));
    if count == 0 || count > 50_000 || !readable(data, count * 0x20) {
        return fail(l, &format!("unit qualities table layout not as expected (count {count}, data {:#x})", data));
    }
    if !DUMPED.swap(true, Ordering::Relaxed) {
        log!("unit qualities table {:#x}: {count} records of 0x20 bytes at {:#x}", table, data);
        for i in 0..count.min(3) {
            let r = data + i * 0x20;
            log!("  record {i}: {:016x} {:016x} {:016x} {:016x} keys '{}' '{}' '{}'", rq(r), rq(r + 8), rq(r + 0x10), rq(r + 0x18),
                crate::recruit::record_key(rq(r)), crate::recruit::record_key(rq(r + 8)), crate::recruit::record_key(rq(r + 0x10)));
        }
    }
    let mut out = String::new();
    for i in 0..count {
        let r = data + i * 0x20;
        let mut keys: Vec<String> = Vec::new();
        let mut numbers: Vec<f32> = Vec::new();
        for slot in 0..4 {
            let v = core::ptr::read_unaligned((r + slot * 8) as *const usize);
            let k = if v > 0x10000 && v % 8 == 0 { crate::recruit::record_key(v) } else { "?".into() };
            if k != "?" { keys.push(k); } else if v < 0x10000 || v % 8 != 0 || !readable(v, 8) {
                numbers.push(number(v as u32));
                numbers.push(number((v >> 32) as u32));
            }
        }
        if let Some(pos) = keys.iter().position(|k| *k == key) {
            let group = keys.iter().enumerate().find(|(j, _)| *j != pos).map(|(_, k)| k.clone()).unwrap_or_else(|| "?".into());
            let mut it = numbers.into_iter().filter(|n| *n != 0.0);
            let (q, qmax) = (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0));
            out.push_str(&format!("{group}={q},{qmax};"));
        }
    }
    lua::push_str(l, &out);
    1
}
