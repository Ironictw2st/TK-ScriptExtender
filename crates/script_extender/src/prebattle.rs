//! Multiplayer pre-battle: one Delegate click is enough (notes/prebattle_delegate.md).
//!
//! The pending battle keeps a PRE_BATTLE_VOTING_SYSTEM (pending-battle manager +0x188): a vector
//! {cap +8, count +0xc, data +0x10} of 0x28-byte FACTION_VOTE entries {CA::String faction key +0,
//! i32 vote +0x10 (0 = not voted yet, 2 = Delegate / autoresolve), u8 flag +0x14, u8 +0x15, timer
//! +0x18}, one per human faction in the battle. FUN_14191aed0(sys, faction, vote, flag) records a
//! vote (called by the CCQ_SET_PENDING_BATTLE_READY_TO_START executor on every machine, and by the
//! vote timer). The vote update FUN_141945a58 waits until no entry is 0 and then starts the battle,
//! autoresolved if ANY entry voted 2. So in vanilla one Delegate already decides the outcome; the
//! other human only has to click too before anything happens.
//!
//! Here, after the engine has recorded a Delegate vote, the same vote is recorded through the same
//! routine for every other faction that has not voted yet. It runs inside the synced command on
//! every machine with the same data, so it stays in lockstep.
//!
//! script_extender.cfg: `prebattle_single_delegate` (default 1; 0 = no hook, vanilla voting). It
//! changes the simulation, so it is in the sync tag.

use crate::addrs::Table;
use crate::log;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use retour::GenericDetour;
use std::sync::OnceLock;

type P = *mut c_void;
type SetVote = unsafe extern "C" fn(P, P, i32, u8);
type FactionByKey = unsafe extern "C" fn(P, P) -> P;

static SET_VOTE: OnceLock<GenericDetour<SetVote>> = OnceLock::new();
static FACTION_BY_KEY: AtomicUsize = AtomicUsize::new(0);
static VOTES: AtomicU64 = AtomicU64::new(0);
static FILLED: AtomicU64 = AtomicU64::new(0);

const VOTE_DELEGATE: i32 = 2;
const ENTRY_SIZE: usize = 0x28;
const MAX_VOTERS: u32 = 16;

extern "system" {
    fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

/// Factions of the entries that have not voted yet, other than `voter`. Collected before any
/// further vote is recorded, because recording one can erase entries from the vector.
unsafe fn not_voted(sys: usize, voter: usize) -> Vec<usize> {
    let lookup = FACTION_BY_KEY.load(Ordering::Relaxed);
    // sys = manager +0x188; *(manager) +0x60 -> getter object, *(+0x78) = world; world +0x3b68 = faction manager
    let world = rq(rq(rq(sys) + 0x60) + 0x78);
    let factions = rq(world + 0x3b68);
    let (count, data) = (rd(sys + 0xc), rq(sys + 0x10));
    if lookup == 0 || factions == 0 || data == 0 || count > MAX_VOTERS { return Vec::new(); }
    let lookup: FactionByKey = core::mem::transmute(lookup);
    let mut out = Vec::new();
    for i in 0..count as usize {
        let e = data + i * ENTRY_SIZE;
        if !readable(e, ENTRY_SIZE) || rd(e + 0x10) != 0 { continue; }
        // the engine's own key -> faction lookup (the vote update does the same; "rebels" gives 0)
        let f = lookup(factions as P, e as P) as usize;
        if f != 0 && f != voter && !out.contains(&f) { out.push(f); }
    }
    out
}

unsafe extern "C" fn set_vote_detour(sys: P, faction: P, vote: i32, flag: u8) {
    let Some(h) = SET_VOTE.get() else { return };
    h.call(sys, faction, vote, flag);
    let _g = crate::crash::enter_quiet("prebattle_vote");
    VOTES.fetch_add(1, Ordering::Relaxed);
    if vote != VOTE_DELEGATE {
        log!("pre-battle vote: faction 0x{:x} vote {vote} flag {flag}", faction as usize);
        return;
    }
    let others = not_voted(sys as usize, faction as usize);
    for f in &others {
        // the trampoline: the original routine, not this detour
        h.call(sys, *f as P, VOTE_DELEGATE, flag);
    }
    FILLED.fetch_add(others.len() as u64, Ordering::Relaxed);
    log!("pre-battle vote: faction 0x{:x} delegated (flag {flag}); recorded the same vote for {} other faction(s) that had not voted", faction as usize, others.len());
}

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("prebattle_single_delegate") {
        log!("pre-battle delegate hook off (prebattle_single_delegate=0 in script_extender.cfg): every human votes");
        return;
    }
    FACTION_BY_KEY.store(t.get("faction_by_key"), Ordering::Relaxed);
    // SAFETY: the prologue is anchor-verified (mov [rsp+18],rbx; push rbp/rsi/rdi/r12/r14; sub
    // rsp,30: no RIP-relative operand in the relocated bytes).
    unsafe {
        let target: SetVote = core::mem::transmute(t.get("prebattle_set_vote"));
        let Ok(d) = GenericDetour::new(target, set_vote_detour) else {
            log!("pre-battle delegate hook: could not create the detour");
            return;
        };
        let _ = SET_VOTE.set(d);
        let Some(d) = SET_VOTE.get() else { return };
        if let Err(e) = crate::freeze::enable_detour("prebattle_set_vote", "prebattle_single_delegate", target as usize, d) {
            log!("pre-battle delegate hook: could not enable the detour ({e}); off");
            return;
        }
    }
    log!("pre-battle delegate hook installed: one human's Delegate vote counts for every human in the battle");
}
