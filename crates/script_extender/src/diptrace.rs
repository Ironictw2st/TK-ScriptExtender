//! Diplomacy validation, made visible (notes/diplomacy.md, "Validation trace"). Read-only.
//! Installed with `diag_diplomacy=1` (off until a script asks) or `=2` (recording from the start).
//!
//! FUN_141aaae10(out, component, negotiation, faction_a, faction_b, params, flags) answers "is
//! this treaty component valid between A and B". Its result object (FUN_141ab3800) is
//!   +0 u8 verdict, +1 u8 inverted, +8 reason record* (the winning `reason` of the failed
//!   campaign_diplomacy_groups row, priority i32 at +0x20), +0x10 context, +0x18 per-faction map
//! and results are merged, not nested (FUN_141abf030 = AND, FUN_141abf3d0 = OR), so the tree is
//! only visible while it is walked:
//!   FUN_1413c86b0(out, group, .., faction_a @7, faction_b @8, .., ctx @10, invert @11, @12)
//!       group +0x18 type (0 AND, 1 OR, 2 NOT), +0x20 reason record*, +0x2c/+0x30 requirement
//!       leaves, +0x3c/+0x40 child groups
//!   FUN_1413d0cc0(out, requirement record, .., faction_a @7, faction_b @8, ..)   one leaf
//! `diag.rs` owns the detour on FUN_141aaae10 and calls eval_enter / eval_exit here.
//!
//! The deal builder above it (a negotiation = +0x3c component count, +0x40 entries of 0x28 bytes):
//!   FUN_141ad4c00(negotiation, component, params, A, B)   add one component: FUN_141ada920
//!       (= FUN_141aaae10 with flags 7) must pass, then FUN_141ac10c0 / FUN_141ac1670 append it
//!   FUN_141ad0870(component_set, negotiation, mask, x, y) -> u8   expand the required treaties of
//!       what was added (recursive; validates every candidate with flags 7, adds it through
//!       FUN_141ad4c00); callers FUN_141a98c60 and FUN_141ad10fc
//!   FUN_141ac7ee0(queue entry, world) builds the deal of a pending follow-up negotiation (this is
//!       what runs when the ultimatum popup is raised, not when its button is clicked)
//!   FUN_141ac3440(deal, action, swap_sides)   the negotiation state machine: deal +0x48 = state
//!       record with transitions +0x2c/+0x30 {+0x10 next state, +0x18 action id}; actions seen in
//!       FUN_141ac7ee0: 3 (nothing to propose), 5 (a component is not unilateral), 4 (applied),
//!       0xe (wait for a response). deal +0x18 = negotiation (components +0x3c/+0x40)
//!   FUN_141a94940(ctx, deal)   apply the deal's components
//!   FUN_141b40890(stream, world)   the negotiation COMMAND handler (what a click in a diplomacy
//!       panel ends up in): deal = FUN_141b3ef10(stream, &out, 0xa0), then op 0 reset, 1 add /
//!       2 remove a component, 3 cancel, 5 propose (FUN_141ad4b30), 6 / 7 vote, 8, 9, 10, 0xc
//!   FUN_141ad4b30(deal) returns without a word when an entry of the list at deal +0x90 has
//!       (byte +0x14 & 0x34) == 4
//!   FUN_141b89ce0(dip_mgr, deal, action)   notification; action 0xe = waiting for the player
//! All are recorded in the timeline with the exe frames that called them, next to every
//! evaluation and every left mouse click, so "the click did X and stopped at Y" can be read off.
//!
//! Identical evaluations (component, A, B, flags, verdict, reason) are stored once with a count;
//! the walk of the first one is kept. FACTION +0xbf0 = its factions record (verified live).
//!
//! `diag_diplomacy=2` records from the start without any script (a session with no Lua console):
//! dip_trace.txt is then rewritten every two seconds while new events come in.
//!
//! se_dip_trace(on [, include_ai]) -> was_on    switch on / off; switching ON clears the store
//! se_dip_trace_mark(label)                     marker in the timeline
//! se_dip_trace_report() -> path, evals, blocked, summary
//!     writes dip_trace.txt next to the DLL; summary = the blocked evaluations, one per line

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

type P = *mut c_void;
type Walk = unsafe extern "C" fn(P, P, P, P, P, P, P, P, P, P, u64, P) -> P;
type AddComponent = unsafe extern "C" fn(P, P, P, P, P);
type Expand = unsafe extern "C" fn(P, P, P, P, P) -> u64;
type Action = unsafe extern "C" fn(P, u32, u8);
type Apply = unsafe extern "C" fn(P, P);
type Command = unsafe extern "C" fn(P, P) -> u64;
type Lookup = unsafe extern "C" fn(P, *mut usize, u32) -> P;
type Notify = unsafe extern "C" fn(P, P, u32) -> u64;

static GROUP: OnceLock<GenericDetour<Walk>> = OnceLock::new();
static LEAF: OnceLock<GenericDetour<Walk>> = OnceLock::new();
static ADD: OnceLock<GenericDetour<AddComponent>> = OnceLock::new();
static EXPAND: OnceLock<GenericDetour<Expand>> = OnceLock::new();
static ACTION: OnceLock<GenericDetour<Action>> = OnceLock::new();
static APPLY: OnceLock<GenericDetour<Apply>> = OnceLock::new();
static COMMAND: OnceLock<GenericDetour<Command>> = OnceLock::new();
static LOOKUP: OnceLock<GenericDetour<Lookup>> = OnceLock::new();
static NOTIFY: OnceLock<GenericDetour<Notify>> = OnceLock::new();
static TRACE: AtomicBool = AtomicBool::new(false);
static WITH_AI: AtomicBool = AtomicBool::new(false);
static STORE: Mutex<Option<Store>> = Mutex::new(None);
static DIRTY: AtomicBool = AtomicBool::new(false);
static START: OnceLock<std::time::Instant> = OnceLock::new();

const MAX_EVALS: usize = 4000;
const MAX_STEPS: usize = 400;
const MAX_TIMELINE: usize = 6000;
const FRAMES: usize = 14;
const OFF_FACTION_RECORD: usize = 0xbf0;

struct Step { depth: u32, leaf: bool, kind: u32, key: String, reason: String, ok: bool }
struct Eval { seq: u64, at_ms: u64, count: u64, component: String, a: String, b: String, flags: u32, ai: bool, ok: bool, reason: String, stack: String, steps: Vec<Step> }
struct Event { at_ms: u64, wall: String, click: bool, text: String }
#[derive(Default)]
struct Store { seq: u64, evals: Vec<Eval>, index: HashMap<(usize, usize, usize, u32, bool, usize), usize>, timeline: VecDeque<Event>, names: HashMap<usize, String> }

thread_local! {
    /// nesting depth of the walk on this thread and the steps of the evaluation in progress
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static STEPS: RefCell<Vec<Step>> = const { RefCell::new(Vec::new()) };
    static IN_EVAL: Cell<bool> = const { Cell::new(false) };
    static EXPAND_DEPTH: Cell<u32> = const { Cell::new(0) };
    static IN_COMMAND: Cell<bool> = const { Cell::new(false) };
}

#[repr(C)]
struct SystemTime { year: u16, month: u16, dow: u16, day: u16, hour: u16, minute: u16, second: u16, ms: u16 }

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
    fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16;
    fn GetLocalTime(t: *mut SystemTime);
}
#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn GetForegroundWindow() -> *mut c_void;
    fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
}
unsafe fn readable(p: usize, n: usize) -> bool {
    p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0
}
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

/// CA::String {u32 len, u32 cap, char* @+8}, short strings inline (top nibble of +8 == 8).
unsafe fn ca_string(sp: usize) -> String {
    if !readable(sp, 16) { return String::new(); }
    if rq(sp + 8) >> 60 == 8 {
        let raw = core::slice::from_raw_parts(sp as *const u8, 15);
        let n = raw.iter().position(|&c| c == 0).unwrap_or(15);
        let ok = n > 0 && raw[..n].iter().all(|c| c.is_ascii_graphic());
        return if ok { String::from_utf8_lossy(&raw[..n]).into_owned() } else { String::new() };
    }
    let (len, ptr) = (rd(sp) as usize, rq(sp + 8));
    if len == 0 || len > 160 || !readable(ptr, len) { return String::new(); }
    let raw = core::slice::from_raw_parts(ptr as *const u8, len);
    if raw.iter().all(|c| c.is_ascii_graphic()) { String::from_utf8_lossy(raw).into_owned() } else { String::new() }
}

unsafe fn record_key(rec: usize) -> String {
    if rec == 0 || rec % 8 != 0 || !readable(rec, 0x20) { return String::new(); }
    let s = ca_string(rec + 8);
    if !s.is_empty() { return s; }
    ca_string(rq(rec + 8))
}

/// Name of an engine object that is not a DB record itself: its own key if it has one, else the
/// key of the first record it points to whose key satisfies `want`. Looked up once per object.
unsafe fn name_of(store: &mut Store, obj: usize, span: usize, want: fn(&str) -> bool) -> String {
    if obj == 0 { return "-".into(); }
    if let Some(s) = store.names.get(&obj) { return s.clone(); }
    let mut name = record_key(obj);
    if !want(&name) {
        name.clear();
        if readable(obj, span) {
            for off in (0..span).step_by(8) {
                let k = record_key(core::ptr::read_unaligned((obj + off) as *const usize));
                if want(&k) { name = k; break; }
            }
        }
    }
    if name.is_empty() { name = format!("?{obj:#x}"); }
    store.names.insert(obj, name.clone());
    name
}
unsafe fn faction_name(store: &mut Store, faction: usize) -> String {
    let k = record_key(rq(faction + OFF_FACTION_RECORD));
    if is_faction(&k) { return k; }
    name_of(store, faction, 0x1000, is_faction)
}
fn now_ms() -> u64 { START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 }
fn wall() -> String {
    let mut t = SystemTime { year: 0, month: 0, dow: 0, day: 0, hour: 0, minute: 0, second: 0, ms: 0 };
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}.{:03}", t.hour, t.minute, t.second, t.ms)
}
fn is_faction(k: &str) -> bool { k.contains("_faction_") }
fn is_component(k: &str) -> bool { k.contains("components_") }
fn any_key(k: &str) -> bool { k.len() > 3 }

/// Return addresses inside the exe above the caller, as Ghidra addresses (image base 0x140000000).
/// A detour is entered by a jump, so the first exe frame is the engine function that called the
/// hooked one.
fn exe_stack() -> String {
    let (base, size) = crate::process::main_module();
    let mut frames = [core::ptr::null_mut::<c_void>(); 48];
    let n = unsafe { RtlCaptureStackBackTrace(1, frames.len() as u32, frames.as_mut_ptr(), core::ptr::null_mut()) } as usize;
    let mut s = String::new();
    let mut kept = 0;
    for f in &frames[..n] {
        let a = *f as usize;
        if a < base || a >= base + size { continue; }
        let _ = write!(s, "{}{:x}", if kept == 0 { "" } else { " < " }, a - base + 0x1_4000_0000);
        kept += 1;
        if kept == FRAMES { break; }
    }
    s
}

fn push_event(store: &mut Store, click: bool, text: String) {
    if store.timeline.len() >= MAX_TIMELINE { store.timeline.pop_front(); }
    store.timeline.push_back(Event { at_ms: now_ms(), wall: wall(), click, text });
    DIRTY.store(true, Ordering::Relaxed);
}

/// For `dipui.rs`: is the trace recording, and one more line for the timeline.
pub fn tracing() -> bool { TRACE.load(Ordering::Relaxed) }
pub fn note(text: String) {
    if let Ok(mut guard) = STORE.lock() { push_event(guard.get_or_insert_with(Store::default), false, text); }
}

fn traced(in_ai_scan: bool) -> bool {
    TRACE.load(Ordering::Relaxed) && (!in_ai_scan || WITH_AI.load(Ordering::Relaxed))
}

/// Called by the FUN_141aaae10 detour before the engine runs. Returns whether this call is traced.
pub unsafe fn eval_enter(in_ai_scan: bool) -> bool {
    if !traced(in_ai_scan) || IN_EVAL.with(|c| c.get()) { return false; }
    IN_EVAL.with(|c| c.set(true));
    DEPTH.with(|d| d.set(0));
    STEPS.with(|s| s.borrow_mut().clear());
    true
}

/// Called after the engine returned, with the filled result object.
pub unsafe fn eval_exit(out: usize, comp: usize, fa: usize, fb: usize, flags: u32, in_ai_scan: bool) {
    IN_EVAL.with(|c| c.set(false));
    let steps: Vec<Step> = STEPS.with(|s| core::mem::take(&mut *s.borrow_mut()));
    let (ok, reason) = if readable(out, 0x10) { (*(out as *const u8) != 0, rq(out + 8)) } else { (false, 0) };
    let stack = exe_stack();
    let Ok(mut guard) = STORE.lock() else { return };
    let store = guard.get_or_insert_with(Store::default);
    store.seq += 1;
    let (component, a, b) = (name_of(store, comp, 0x100, is_component), faction_name(store, fa), faction_name(store, fb));
    let reason_key = if reason == 0 { String::new() } else { name_of(store, reason, 0x40, any_key) };
    let line = format!("EVAL {} {component} | {a} -> {b} | flags {flags:#x} | {reason_key} | {stack}", if ok { "OK" } else { "BLOCKED" });
    push_event(store, false, line);
    let key = (comp, fa, fb, flags, ok, reason);
    if let Some(&i) = store.index.get(&key) { store.evals[i].count += 1; return; }
    if store.evals.len() >= MAX_EVALS { return; }
    let e = Eval { seq: store.seq, at_ms: now_ms(), count: 1, flags, ai: in_ai_scan, ok, component, a, b, reason: reason_key, stack, steps };
    store.index.insert(key, store.evals.len());
    store.evals.push(e);
}

unsafe fn walk(hook: &GenericDetour<Walk>, leaf: bool, a: [P; 10], inv: u64, last: P) -> P {
    if !IN_EVAL.with(|c| c.get()) {
        return hook.call(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], inv, last);
    }
    let depth = DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v });
    let slot = STEPS.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() >= MAX_STEPS { return None; }
        s.push(Step { depth, leaf, kind: 0, key: String::new(), reason: String::new(), ok: false });
        Some(s.len() - 1)
    });
    let r = hook.call(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], inv, last);
    DEPTH.with(|d| d.set(depth));
    if let Some(i) = slot {
        let (out, node) = (a[0] as usize, a[1] as usize);
        let ok = readable(out, 1) && *(out as *const u8) != 0;
        let (kind, key, reason) = if leaf {
            let mut k = record_key(node);
            if k.is_empty() {
                for off in (0x10..0x60).step_by(8) {
                    k = record_key(rq(node + off));
                    if !k.is_empty() { break; }
                }
            }
            (0, k, String::new())
        } else {
            (rd(node + 0x18), record_key(node), record_key(rq(node + 0x20)))
        };
        STEPS.with(|s| if let Some(st) = s.borrow_mut().get_mut(i) { st.kind = kind; st.key = key; st.reason = reason; st.ok = ok; });
    }
    r
}

unsafe extern "C" fn group_detour(a0: P, a1: P, a2: P, a3: P, a4: P, a5: P, a6: P, a7: P, a8: P, a9: P, inv: u64, last: P) -> P {
    let Some(h) = GROUP.get() else { return a0 };
    let _g = crate::crash::enter("dip_group_eval");
    walk(h, false, [a0, a1, a2, a3, a4, a5, a6, a7, a8, a9], inv, last)
}
unsafe extern "C" fn leaf_detour(a0: P, a1: P, a2: P, a3: P, a4: P, a5: P, a6: P, a7: P, a8: P, a9: P, inv: u64, last: P) -> P {
    let Some(h) = LEAF.get() else { return a0 };
    let _g = crate::crash::enter("dip_requirement_eval");
    walk(h, true, [a0, a1, a2, a3, a4, a5, a6, a7, a8, a9], inv, last)
}

unsafe fn component_count(negotiation: usize) -> u32 { rd(negotiation + 0x3c) }

unsafe extern "C" fn add_detour(neg: P, comp: P, params: P, fa: P, fb: P) {
    let Some(h) = ADD.get() else { return };
    let _g = crate::crash::enter("dip_deal_add");
    if !traced(crate::diag::in_scan()) { return h.call(neg, comp, params, fa, fb); }
    let before = component_count(neg as usize);
    let stack = exe_stack();
    h.call(neg, comp, params, fa, fb);
    let after = component_count(neg as usize);
    if let Ok(mut guard) = STORE.lock() {
        let store = guard.get_or_insert_with(Store::default);
        let (component, a, b) = (name_of(store, comp as usize, 0x100, is_component), faction_name(store, fa as usize), faction_name(store, fb as usize));
        let line = format!("ADD {} {component} | {a} -> {b} | deal {:#x} components {before} -> {after} | {stack}", if after > before { "done" } else { "REFUSED" }, neg as usize);
        push_event(store, false, line);
    }
}

unsafe extern "C" fn expand_detour(set: P, neg: P, mask: P, x: P, y: P) -> u64 {
    let Some(h) = EXPAND.get() else { return 0 };
    let _g = crate::crash::enter("dip_deal_expand");
    if !traced(crate::diag::in_scan()) { return h.call(set, neg, mask, x, y); }
    let depth = EXPAND_DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v });
    let before = component_count(neg as usize);
    let stack = if depth == 0 { exe_stack() } else { String::new() };
    let r = h.call(set, neg, mask, x, y);
    EXPAND_DEPTH.with(|d| d.set(depth));
    let after = component_count(neg as usize);
    if let Ok(mut guard) = STORE.lock() {
        let store = guard.get_or_insert_with(Store::default);
        let line = format!("EXPAND depth {depth} -> {} | deal {:#x} components {before} -> {after} | {stack}", if r as u8 != 0 { "ok" } else { "FAILED" }, neg as usize);
        push_event(store, false, line);
    }
    r
}

/// State of a deal: key of its state record and the actions that lead out of it.
unsafe fn deal_state(deal: usize) -> String {
    let state = rq(deal + 0x48);
    if state == 0 { return "(no state)".into(); }
    let mut s = record_key(state);
    if s.is_empty() { s = format!("?{state:#x}"); }
    let (n, arr) = (rd(state + 0x2c) as usize, rq(state + 0x30));
    if n > 0 && n <= 32 && readable(arr, n * 8) {
        s.push_str(" [");
        for i in 0..n {
            let t = rq(arr + i * 8);
            let _ = write!(s, "{}{}->{}", if i == 0 { "" } else { ", " }, rd(t + 0x18), record_key(rq(t + 0x10)));
        }
        s.push(']');
    }
    s
}

/// id, state, component count and the flag bytes of the list at deal +0x90 (FUN_141ad4b30 refuses
/// to propose while one of them has (flags & 0x34) == 4).
unsafe fn deal_info(deal: usize) -> String {
    if !readable(deal, 0xa0) { return format!("deal {deal:#x} (unreadable)"); }
    let mut s = format!("deal {deal:#x} id {} components {} | {} | flags", rd(deal + 8), rd(deal + 0x54), deal_state(deal));
    let (head, mut node, mut n) = (deal + 0x90, rq(deal + 0x98), 0);
    while node != 0 && node != head && n < 16 && readable(node, 0x18) {
        let f = *((node + 0x14) as *const u8);
        let _ = write!(s, " {f:#04x}{}", if f & 0x34 == 4 { "(BLOCKS PROPOSE)" } else { "" });
        node = rq(node + 8);
        n += 1;
    }
    s
}

unsafe extern "C" fn command_detour(stream: P, world: P) -> u64 {
    let Some(h) = COMMAND.get() else { return 0 };
    let _g = crate::crash::enter("dip_command");
    if !TRACE.load(Ordering::Relaxed) { return h.call(stream, world); }
    let stack = exe_stack();
    if let Ok(mut guard) = STORE.lock() { push_event(guard.get_or_insert_with(Store::default), false, format!("COMMAND negotiation begins | {stack}")); }
    IN_COMMAND.with(|c| c.set(true));
    let r = h.call(stream, world);
    IN_COMMAND.with(|c| c.set(false));
    let failed = readable(stream as usize + 8, 1) && *((stream as usize + 8) as *const u8) != 0;
    if let Ok(mut guard) = STORE.lock() { push_event(guard.get_or_insert_with(Store::default), false, format!("COMMAND negotiation ends | stream error flag {failed}")); }
    r
}

unsafe extern "C" fn lookup_detour(stream: P, out: *mut usize, ty: u32) -> P {
    let Some(h) = LOOKUP.get() else { return stream };
    let _g = crate::crash::enter("dip_command_deal");
    let r = h.call(stream, out, ty);
    if ty == 0xa0 && IN_COMMAND.with(|c| c.get()) && TRACE.load(Ordering::Relaxed) {
        let deal = if readable(out as usize, 8) { *out } else { 0 };
        let text = if deal == 0 { "COMMAND deal lookup -> NOT FOUND".to_string() } else { format!("COMMAND deal lookup -> {}", deal_info(deal)) };
        if let Ok(mut guard) = STORE.lock() { push_event(guard.get_or_insert_with(Store::default), false, text); }
    }
    r
}

unsafe extern "C" fn notify_detour(mgr: P, deal: P, action: u32) -> u64 {
    let Some(h) = NOTIFY.get() else { return 0 };
    let _g = crate::crash::enter("dip_notify");
    if action == 0xe && traced(crate::diag::in_scan()) {
        let text = format!("WAIT FOR PLAYER (0xe) {} | {}", deal_info(deal as usize), exe_stack());
        if let Ok(mut guard) = STORE.lock() { push_event(guard.get_or_insert_with(Store::default), false, text); }
    }
    h.call(mgr, deal, action)
}

unsafe extern "C" fn action_detour(deal: P, action: u32, swap: u8) {
    let Some(h) = ACTION.get() else { return };
    let _g = crate::crash::enter("dip_deal_action");
    if !traced(crate::diag::in_scan()) { return h.call(deal, action, swap); }
    let d = deal as usize;
    let (before, stack, id, comps) = (deal_state(d), exe_stack(), rd(d + 8), rd(d + 0x54));
    h.call(deal, action, swap);
    let after = deal_state(d);
    if let Ok(mut guard) = STORE.lock() {
        let store = guard.get_or_insert_with(Store::default);
        push_event(store, false, format!("ACTION {action} swap {swap} | deal {d:#x} id {id} components {comps} | {before} => {after} | {stack}"));
    }
}

unsafe extern "C" fn apply_detour(ctx: P, deal: P) {
    let Some(h) = APPLY.get() else { return };
    let _g = crate::crash::enter("dip_deal_apply");
    if !traced(crate::diag::in_scan()) { return h.call(ctx, deal); }
    let stack = exe_stack();
    h.call(ctx, deal);
    if let Ok(mut guard) = STORE.lock() {
        let store = guard.get_or_insert_with(Store::default);
        push_event(store, false, format!("APPLY deal {:#x} | {stack}", deal as usize));
    }
}

/// Left mouse button presses while the game window is in front, so the timeline shows which
/// events a click caused. Polled; a click is 60+ ms long.
fn click_watch() {
    let me = std::process::id();
    let mut down = false;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(8));
        if !TRACE.load(Ordering::Relaxed) { down = false; continue; }
        let pressed = unsafe { GetAsyncKeyState(1) } < 0;
        if pressed && !down {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
            if pid == me {
                if let Ok(mut guard) = STORE.lock() {
                    push_event(guard.get_or_insert_with(Store::default), true, "CLICK".into());
                }
            }
        }
        down = pressed;
    }
}

unsafe fn enable<T: retour::Function>(name: &'static str, target: usize, d: &GenericDetour<T>) -> bool {
    match crate::freeze::enable_detour(name, "diag_diplomacy", target, d) {
        Ok(()) => true,
        Err(e) => { log!("diplomacy trace: {name}: {e}"); false }
    }
}

/// Called by `diag::install` once its own detours are in (same cfg switch).
pub fn install(t: &Table) {
    // SAFETY: all six prologues are anchor-verified pushes / stack stores (no rip-relative operand).
    unsafe {
        let group: Walk = core::mem::transmute(t.get("dip_group_eval"));
        let leaf: Walk = core::mem::transmute(t.get("dip_requirement_eval"));
        let add: AddComponent = core::mem::transmute(t.get("dip_deal_add"));
        let expand: Expand = core::mem::transmute(t.get("dip_deal_expand"));
        let (Ok(g), Ok(f), Ok(a), Ok(x)) = (GenericDetour::new(group, group_detour), GenericDetour::new(leaf, leaf_detour), GenericDetour::new(add, add_detour), GenericDetour::new(expand, expand_detour)) else {
            log!("diplomacy trace: could not create the detours");
            return;
        };
        let (_, _, _, _) = (GROUP.set(g), LEAF.set(f), ADD.set(a), EXPAND.set(x));
        let (Some(g), Some(f), Some(a), Some(x)) = (GROUP.get(), LEAF.get(), ADD.get(), EXPAND.get()) else { return };
        let action: Action = core::mem::transmute(t.get("dip_deal_action"));
        let apply: Apply = core::mem::transmute(t.get("dip_deal_apply"));
        let (Ok(ac), Ok(ap)) = (GenericDetour::new(action, action_detour), GenericDetour::new(apply, apply_detour)) else {
            log!("diplomacy trace: could not create the deal detours");
            return;
        };
        let (_, _) = (ACTION.set(ac), APPLY.set(ap));
        let (Some(ac), Some(ap)) = (ACTION.get(), APPLY.get()) else { return };
        let command: Command = core::mem::transmute(t.get("dip_command"));
        let lookup: Lookup = core::mem::transmute(t.get("dip_command_deal"));
        let notify: Notify = core::mem::transmute(t.get("dip_notify"));
        let (Ok(co), Ok(lo), Ok(no)) = (GenericDetour::new(command, command_detour), GenericDetour::new(lookup, lookup_detour), GenericDetour::new(notify, notify_detour)) else {
            log!("diplomacy trace: could not create the command detours");
            return;
        };
        let (_, _, _) = (COMMAND.set(co), LOOKUP.set(lo), NOTIFY.set(no));
        let (Some(co), Some(lo), Some(no)) = (COMMAND.get(), LOOKUP.get(), NOTIFY.get()) else { return };
        if !enable("dip_command", command as usize, co) || !enable("dip_command_deal", lookup as usize, lo) || !enable("dip_notify", notify as usize, no) {
            log!("diplomacy trace: could not enable the command detours");
            return;
        }
        if !enable("dip_group_eval", group as usize, g) || !enable("dip_requirement_eval", leaf as usize, f) || !enable("dip_deal_add", add as usize, a)
            || !enable("dip_deal_expand", expand as usize, x) || !enable("dip_deal_action", action as usize, ac) || !enable("dip_deal_apply", apply as usize, ap) {
            log!("diplomacy trace: could not enable the detours");
            return;
        }
    }
    crate::dipui::install(t);
    std::thread::spawn(click_watch);
    if crate::build::config_value("diag_diplomacy").as_deref() == Some("2") {
        let _ = now_ms();
        TRACE.store(true, Ordering::Relaxed);
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if DIRTY.swap(false, Ordering::Relaxed) { let _ = write_report(); }
        });
        log!("diplomacy trace installed and recording (diag_diplomacy=2): dip_trace.txt next to the DLL");
        return;
    }
    log!("diplomacy trace installed (off until a script asks for it)");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_dip_trace", se_dip_trace);
    lua::set_global_fn(l, "se_dip_trace_mark", se_dip_trace_mark);
    lua::set_global_fn(l, "se_dip_trace_report", se_dip_trace_report);
}

unsafe extern "C" fn se_dip_trace(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let top = (api.gettop)(l);
    let on = top >= 1 && (api.toboolean)(l, 1) != 0;
    WITH_AI.store(top >= 2 && (api.toboolean)(l, 2) != 0, Ordering::Relaxed);
    if on && GROUP.get().is_none() { log!("se_dip_trace: detours not installed (script_extender.cfg: diag_diplomacy=1)"); }
    let was = TRACE.swap(on, Ordering::Relaxed);
    if on && !was { if let Ok(mut g) = STORE.lock() { *g = Some(Store::default()); } }
    (api.pushboolean)(l, was as c_int);
    1
}

unsafe extern "C" fn se_dip_trace_mark(l: *mut LuaState) -> c_int {
    if !TRACE.load(Ordering::Relaxed) { return 0; }
    let label = lua::to_str(l, 1);
    if let Ok(mut g) = STORE.lock() {
        push_event(g.get_or_insert_with(Store::default), false, format!("MARK {label}"));
    }
    0
}

fn render(store: &Store) -> (String, String, usize) {
    let (mut text, mut summary, mut blocked) = (String::new(), String::new(), 0usize);
    let _ = writeln!(text, "# script_extender {} diplomacy validation trace: {} distinct evaluation(s), {} in total", env!("CARGO_PKG_VERSION"), store.evals.len(), store.seq);
    let _ = writeln!(text, "#");
    let _ = writeln!(text, "# TIMELINE (newest {MAX_TIMELINE} events; addresses = Ghidra, innermost caller first). A CLICK is");
    let _ = writeln!(text, "# listed when something followed it within 3 s, plus the last five.");
    let events: Vec<&Event> = store.timeline.iter().collect();
    let last_clicks: Vec<usize> = events.iter().enumerate().filter(|(_, e)| e.click).map(|(i, _)| i).rev().take(5).collect();
    for (i, e) in events.iter().enumerate() {
        if e.click {
            let followed = events[i + 1..].iter().find(|n| !n.click).is_some_and(|n| n.at_ms - e.at_ms <= 3000);
            if !followed && !last_clicks.contains(&i) { continue; }
        }
        let _ = writeln!(text, "{} @{:.2} {}", e.wall, e.at_ms as f64 / 1000.0, e.text);
    }
    let _ = writeln!(text, "#");
    let _ = writeln!(text, "# DISTINCT EVALUATIONS: E <first seen> @<seconds> x<times> OK|BLOCKED <component> | <a> -> <b> | flags | reason");
    let _ = writeln!(text, "#   walk: AND/OR/NOT = campaign_diplomacy_groups node (key, reason), REQ = requirement leaf; '+' passed, '-' failed");
    for e in &store.evals {
        let head = format!("{} @{:.1} x{} {} {} | {} -> {} | flags {:#x}{} | {}", e.seq, e.at_ms as f64 / 1000.0, e.count, if e.ok { "OK" } else { "BLOCKED" }, e.component, e.a, e.b, e.flags, if e.ai { " ai" } else { "" }, e.reason);
        let _ = writeln!(text, "E {head}");
        let _ = writeln!(text, "  stack {}", e.stack);
        if !e.ok {
            blocked += 1;
            if summary.len() < 6000 { let _ = writeln!(summary, "{head}"); }
        }
        for s in &e.steps {
            let kind = if s.leaf { "REQ" } else { match s.kind { 0 => "AND", 1 => "OR", 2 => "NOT", _ => "?" } };
            let _ = writeln!(text, "  {}{} {} {} {}", "  ".repeat(s.depth as usize), if s.ok { '+' } else { '-' }, kind, s.key, s.reason);
        }
    }
    (text, summary, blocked)
}

fn write_report() -> (std::path::PathBuf, String, usize, usize) {
    let (text, summary, blocked, evals) = match STORE.lock() {
        Ok(g) => match g.as_ref() {
            Some(store) => { let (t, s, b) = render(store); (t, s, b, store.evals.len()) }
            None => (String::new(), String::new(), 0, 0),
        },
        Err(_) => (String::new(), String::new(), 0, 0),
    };
    let path = crate::process::self_dir().unwrap_or_default().join("dip_trace.txt");
    if let Err(e) = std::fs::write(&path, text) { log!("diplomacy trace: {}: {e}", path.display()); }
    (path, summary, blocked, evals)
}

unsafe extern "C" fn se_dip_trace_report(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let (path, summary, blocked, evals) = write_report();
    lua::push_str(l, &path.display().to_string());
    (api.pushnumber)(l, evals as f32);
    (api.pushnumber)(l, blocked as f32);
    lua::push_str(l, &summary);
    4
}
