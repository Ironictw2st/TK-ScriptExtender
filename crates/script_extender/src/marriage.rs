//! Marriage between relatives by marriage (notes/family.md).
//!
//! The engine's marriage verdict FUN_14141e7e0(family_mgr, famA, famB, mode) refuses a pair with
//! code 1 when FUN_1413d4c20 finds any path between them over father, mother, SPOUSE, the +0x50
//! list and children, with no depth limit. One marriage between two houses therefore makes every
//! member of one family "related" to every member of the other, and no second marriage is
//! possible ("There exist no two characters in these factions who can be married").
//!
//! Here the relatedness search answers differently only while the marriage verdict is running on
//! the thread (a thread-local flag set by a detour on the verdict): "related" only when the two
//! share a blood ancestor within `marriage_blood_generations` generations (father / mother links;
//! 0 = never). The verdict then continues with its own later tests, including the close-kin test
//! FUN_141454d70 (parents, children, siblings, grandparents), so those marriages stay impossible.
//! Every other use of the search, notably the faction-leader family membership behind the
//! "distant relative" status and icon, gets the engine's own answer.
//!
//! script_extender.cfg: `marriage_inlaws` (default 1; 0 = no hook at all, vanilla rule) and
//! `marriage_blood_generations` (default 0). Both change the simulation: they are in the sync tag.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::cell::Cell;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use retour::GenericDetour;
use std::sync::OnceLock;

type P = *mut c_void;
type Verdict = unsafe extern "C" fn(P, P, P, i32) -> u64;
type Related = unsafe extern "C" fn(P, P, P) -> P;

static VERDICT: OnceLock<GenericDetour<Verdict>> = OnceLock::new();
static RELATED: OnceLock<GenericDetour<Related>> = OnceLock::new();
static GENERATIONS: AtomicU32 = AtomicU32::new(0);
static VERDICTS: AtomicU64 = AtomicU64::new(0);
static SEARCHES: AtomicU64 = AtomicU64::new(0);
static BLOOD_BLOCKED: AtomicU64 = AtomicU64::new(0);
static MAY_MARRY: AtomicU64 = AtomicU64::new(0);

const MAX_GENERATIONS: u32 = 6;
const FATHER: usize = 0x20;
const MOTHER: usize = 0x28;
const CODE_MAY_MARRY: u64 = 0x22;

thread_local! {
    static IN_VERDICT: Cell<bool> = const { Cell::new(false) };
}

extern "system" {
    fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }

/// The blood ancestors of `fm` up to `n` generations: (ancestor, generations above fm). Bounded:
/// at most 2^n entries, n <= MAX_GENERATIONS.
unsafe fn ancestors(fm: usize, n: u32) -> Vec<(usize, u32)> {
    let mut out: Vec<(usize, u32)> = Vec::new();
    let mut frontier = vec![fm];
    for depth in 1..=n {
        let mut next = Vec::new();
        for f in frontier {
            for off in [FATHER, MOTHER] {
                let p = rq(f + off);
                if p != 0 && !out.iter().any(|(a, _)| *a == p) {
                    out.push((p, depth));
                    next.push(p);
                }
            }
        }
        if next.is_empty() { break; }
        frontier = next;
    }
    out
}

/// Blood relation within `n` generations: one is the other's ancestor, or they share an ancestor
/// at most `n` generations above both. Returns a non-null family member (what the engine returns
/// for "related"), or 0.
unsafe fn blood_related(a: usize, b: usize, n: u32) -> usize {
    if n == 0 || a == 0 || b == 0 { return 0; }
    let (up_a, up_b) = (ancestors(a, n), ancestors(b, n));
    if up_a.iter().any(|(x, _)| *x == b) { return b; }
    if up_b.iter().any(|(x, _)| *x == a) { return a; }
    up_a.iter().find(|(x, _)| up_b.iter().any(|(y, _)| y == x)).map(|(x, _)| *x).unwrap_or(0)
}

unsafe extern "C" fn verdict_detour(mgr: P, a: P, b: P, mode: i32) -> u64 {
    let Some(h) = VERDICT.get() else { return 0 };
    let _g = crate::crash::enter_quiet("marriage_verdict");
    VERDICTS.fetch_add(1, Ordering::Relaxed);
    let outer = IN_VERDICT.with(|f| f.replace(true));
    let r = h.call(mgr, a, b, mode);
    IN_VERDICT.with(|f| f.set(outer));
    if r & 0xffff_ffff == CODE_MAY_MARRY { MAY_MARRY.fetch_add(1, Ordering::Relaxed); }
    r
}

unsafe extern "C" fn related_detour(mgr: P, a: P, b: P) -> P {
    let Some(h) = RELATED.get() else { return core::ptr::null_mut() };
    if !IN_VERDICT.with(|f| f.get()) { return h.call(mgr, a, b); }
    let _g = crate::crash::enter_quiet("family_related");
    SEARCHES.fetch_add(1, Ordering::Relaxed);
    let hit = blood_related(a as usize, b as usize, GENERATIONS.load(Ordering::Relaxed));
    if hit != 0 { BLOOD_BLOCKED.fetch_add(1, Ordering::Relaxed); }
    hit as P
}

/// cfg `marriage_blood_generations`, clamped to MAX_GENERATIONS (also what the sync tag hashes).
pub fn generations() -> u32 {
    crate::build::config_value("marriage_blood_generations").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0).min(MAX_GENERATIONS)
}

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("marriage_inlaws") {
        log!("marriage hook off (marriage_inlaws=0 in script_extender.cfg): vanilla relatedness rule");
        return;
    }
    let n = generations();
    GENERATIONS.store(n, Ordering::Relaxed);
    // SAFETY: both prologues are anchor-verified stack stores / pushes / mov rax,rsp (no
    // RIP-relative operand in the relocated bytes).
    unsafe {
        let verdict: Verdict = core::mem::transmute(t.get("marriage_verdict"));
        let related: Related = core::mem::transmute(t.get("family_related"));
        let (Ok(v), Ok(r)) = (GenericDetour::new(verdict, verdict_detour), GenericDetour::new(related, related_detour)) else {
            log!("marriage hook: could not create the detours");
            return;
        };
        // stored before they are enabled; the search first, so the verdict never sets a flag
        // that nothing reads
        let (_, _) = (RELATED.set(r), VERDICT.set(v));
        let (Some(r), Some(v)) = (RELATED.get(), VERDICT.get()) else { return };
        if let Err(e) = crate::freeze::enable_detour("family_related", "marriage_inlaws", related as usize, r) {
            log!("marriage hook: could not enable the relatedness detour ({e}); off");
            return;
        }
        if let Err(e) = crate::freeze::enable_detour("marriage_verdict", "marriage_inlaws", verdict as usize, v) {
            log!("marriage hook: could not enable the verdict detour ({e}); off");
            return;
        }
    }
    log!("marriage hook installed: relatives by marriage may marry; blood relatives within {n} generation(s) may not (plus the engine's close-kin rule)");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_marriage_stats", se_marriage_stats);
}

/// se_marriage_stats() -> "installed=;generations=;verdicts=;searches=;blood_blocked=;may_marry="
unsafe extern "C" fn se_marriage_stats(l: *mut LuaState) -> c_int {
    let s = format!("installed={};generations={};verdicts={};searches={};blood_blocked={};may_marry={}",
        VERDICT.get().is_some() as u8, GENERATIONS.load(Ordering::Relaxed), VERDICTS.load(Ordering::Relaxed),
        SEARCHES.load(Ordering::Relaxed), BLOOD_BLOCKED.load(Ordering::Relaxed), MAY_MARRY.load(Ordering::Relaxed));
    lua::push_str(l, &s);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake FAMILY_MEMBER nodes in a Vec: father at +0x20, mother at +0x28.
    struct Tree { nodes: Vec<Box<[usize; 8]>> }
    impl Tree {
        fn new() -> Self { Tree { nodes: Vec::new() } }
        fn add(&mut self, father: usize, mother: usize) -> usize {
            let mut n = Box::new([0usize; 8]);
            n[FATHER / 8] = father;
            n[MOTHER / 8] = mother;
            let p = n.as_ptr() as usize;
            self.nodes.push(n);
            p
        }
    }

    #[test]
    fn blood_rules() {
        let mut t = Tree::new();
        let (gf, gm) = (t.add(0, 0), t.add(0, 0));
        let (p1, p2) = (t.add(gf, gm), t.add(gf, gm)); // siblings
        let (c1, c2) = (t.add(p1, 0), t.add(p2, 0)); // first cousins
        let stranger = t.add(0, 0);
        unsafe {
            assert_eq!(blood_related(c1, c2, 0), 0, "n = 0: nothing counts");
            assert_eq!(blood_related(c1, c2, 1), 0, "cousins share an ancestor 2 generations up");
            assert_ne!(blood_related(c1, c2, 2), 0);
            assert_ne!(blood_related(p1, p2, 1), 0, "siblings share parents");
            assert_ne!(blood_related(c1, gf, 2), 0, "grandparent is an ancestor");
            assert_eq!(blood_related(c1, gf, 1), 0);
            assert_eq!(blood_related(c1, stranger, 6), 0);
        }
    }
}
