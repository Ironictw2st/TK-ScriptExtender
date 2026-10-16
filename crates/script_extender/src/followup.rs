//! The dead call-for-action button of the follow-up negotiation popup (MEDIATE PEACE after taking
//! the Emperor; notes/diplomacy.md). On 1.7.2.0 the popup's `button_accept` is lit (CanPropose
//! answers true) and receives the click, but its ContextCommandLeftClick `ProposeDeal` never runs,
//! so nothing is sent. Everything behind it works: the engine's ProposeDeal only builds negotiation
//! command op 5 for the deal and hands it to the command queue, and the recipient answers.
//!
//!   FUN_142f48cd0 CanPropose(cco, out)   runs when a negotiation UI is built. `cco` is a temporary
//!       (gone by the time of a click: 0.41.0-beta.9), so what is remembered is what ProposeDeal
//!       would use: deal = FUN_142f71b40(*(cco+0x218)), queue = *(*(*(*(cco+0x218)+8)+0x2188)+0x80),
//!       plus the deal's id (+8) and state record (+0x48) to recognise it later; only for a deal in
//!       a `*_proposed_by_active_faction` node (the popup), never the diplomacy screen's negotiation
//!   FUN_142f8bf40 ProposeDeal(cco)       remembers when the engine ran it by itself
//!   FUN_141b3f3b0(buf, deal, op)         builds the 0x38-byte command {vtable, deal, op, 0, 0, 0, 0}
//!   FUN_142f256e0(queue, command)        sends it
//!   FUN_142fa8000(campaign_ui, x)        per-frame campaign UI update, main thread
//!
//! se_followup_propose() -> ok, msg
//!     Only QUEUES the request: calling into the UI from a campaign Lua callback froze the game
//!     (0.41.0-beta.7). The per-frame UI update detour carries it out after the update returned:
//!     if the remembered deal still has the same id and is still in the state it was shown in, it
//!     sends command op 5 (propose) for it, exactly what ProposeDeal sends for a forcing deal.
//!     Skipped when the engine ran ProposeDeal itself within the last 0.5 s (a click that worked).
//!     The command goes through the normal command queue, so it is as multiplayer-safe as the
//!     vanilla button.
//!
//! Recipient side (0.45.1, cfg `followup_respond_hook`): the AI's follow-up offer to the human
//! (the emperor holder's "Issue Ultimatum" in `diplomacy_popup`) can be just as dead: the click
//! reaches ACCEPT / REJECT, but no negotiation command is sent although the deal waits in its
//! `followup_*_respond_*` node (notes/diplomacy.md, "Recipient side"). Same scheme:
//!   FUN_142f48b30 CanPlayerAccept(cco, out)  remembers deal, id, state, queue and the local
//!       faction = *(*(*(cco+0x210)+0x2188)+0x1a8), only for a deal in a `followup_*_respond_*` node
//!   FUN_142f43050 Accept(cco) / FUN_142f90790 Reject(cco)  remember when the engine ran them
//!   FUN_141b3f420(buf, deal, op, faction)  the command Accept (op 7) and Reject (op 6) build
//! se_followup_respond("accept"|"reject") queues it; the UI update detour sends it.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

type P = *mut c_void;
type F2 = unsafe extern "C" fn(P, P) -> u64;

struct Engine {
    negotiation_deal: unsafe extern "C" fn(P) -> P,
    command_build: unsafe extern "C" fn(*mut u8, P, u32) -> P,
    command_send: unsafe extern "C" fn(P, P) -> u64,
    command_build_respond: unsafe extern "C" fn(*mut u8, P, u32, P) -> P,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
static PROPOSE: OnceLock<GenericDetour<F2>> = OnceLock::new();
static CAN_PROPOSE: OnceLock<GenericDetour<F2>> = OnceLock::new();
static UI_UPDATE: OnceLock<GenericDetour<F2>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static DEAL: AtomicUsize = AtomicUsize::new(0);
static DEAL_ID: AtomicU64 = AtomicU64::new(0);
static DEAL_STATE: AtomicUsize = AtomicUsize::new(0);
static QUEUE: AtomicUsize = AtomicUsize::new(0);
static ENGINE_PROPOSE_MS: AtomicU64 = AtomicU64::new(0);
/// now_ms() of a queued request, 0 = none
static REQUEST_MS: AtomicU64 = AtomicU64::new(0);

const OP_PROPOSE: u32 = 5;
const OP_REJECT: u32 = 6;
const OP_ACCEPT: u32 = 7;

// recipient side
static CAN_ACCEPT: OnceLock<GenericDetour<F2>> = OnceLock::new();
static ACCEPT: OnceLock<GenericDetour<F2>> = OnceLock::new();
static REJECT: OnceLock<GenericDetour<F2>> = OnceLock::new();
static RDEAL: AtomicUsize = AtomicUsize::new(0);
static RDEAL_ID: AtomicU64 = AtomicU64::new(0);
static RDEAL_STATE: AtomicUsize = AtomicUsize::new(0);
static RQUEUE: AtomicUsize = AtomicUsize::new(0);
static RFACTION: AtomicUsize = AtomicUsize::new(0);
static ENGINE_RESPOND_MS: AtomicU64 = AtomicU64::new(0);
/// now_ms() of a queued answer, 0 = none; RESP_OP = its op (6 reject, 7 accept)
static RESP_REQUEST_MS: AtomicU64 = AtomicU64::new(0);
static RESP_OP: AtomicU32 = AtomicU32::new(0);

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

/// The popup's deal waits in a `*_proposed_by_active_faction` node (the only nodes of the vanilla
/// DB with a `ui` self-transition, both follow-up types). The diplomacy screen's own negotiation
/// runs through the same CanPropose but sits in an `approach_*` node: it must never be proposed
/// by us (0.44.0-beta.2 sent the player's half-built deal on any `button_accept` click).
unsafe fn is_followup_deal(deal: usize) -> bool {
    crate::diptrace::record_key(rq(deal + 0x48)).ends_with("_proposed_by_active_faction")
}

/// The player's answer to a follow-up waits in a `followup_*_respond_*` node
/// (`followup_forced_respond_none_in_alliance_faction_to_faction_negotiated_deal` [7 accept, 6 reject]).
unsafe fn is_followup_respond_deal(deal: usize) -> bool {
    let key = crate::diptrace::record_key(rq(deal + 0x48));
    key.starts_with("followup_") && key.contains("_respond_")
}

fn now_ms() -> u64 { START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 10_000 }

unsafe extern "C" fn can_propose_detour(cco: P, out: P) -> u64 {
    let Some(h) = CAN_PROPOSE.get() else { return 0 };
    let _g = crate::crash::enter_quiet("dipui_can_propose");
    if let Some(e) = ENGINE.get() {
        let neg = rq(cco as usize + 0x218);
        if neg != 0 {
            let deal = (e.negotiation_deal)(neg as P) as usize;
            let queue = rq(rq(rq(neg + 8) + 0x2188) + 0x80);
            if deal != 0 && queue != 0 && readable(deal, 0x60) {
                // a deal the local faction answers (CanPlayerAccept saw it) is never ours to propose
                let ours_to_answer = RDEAL.load(Ordering::Relaxed) == deal && RDEAL_ID.load(Ordering::Relaxed) == rd(deal + 8) as u64;
                if is_followup_deal(deal) && !ours_to_answer {
                    DEAL.store(deal, Ordering::Relaxed);
                    DEAL_ID.store(rd(deal + 8) as u64, Ordering::Relaxed);
                    DEAL_STATE.store(rq(deal + 0x48), Ordering::Relaxed);
                    QUEUE.store(queue, Ordering::Relaxed);
                } else {
                    // an ordinary negotiation (diplomacy screen): forget any earlier popup, so a
                    // click there can never propose the deal the player is still building
                    DEAL.store(0, Ordering::Relaxed);
                }
            }
        }
    }
    crate::dipui::around_can_propose(|| h.call(cco, out))
}

unsafe extern "C" fn propose_detour(cco: P, b: P) -> u64 {
    let Some(h) = PROPOSE.get() else { return 0 };
    let _g = crate::crash::enter("dipui_propose_deal");
    ENGINE_PROPOSE_MS.store(now_ms(), Ordering::Relaxed);
    crate::dipui::around_propose("engine click", || h.call(cco, b))
}

unsafe extern "C" fn can_accept_detour(cco: P, out: P) -> u64 {
    let Some(h) = CAN_ACCEPT.get() else { return 0 };
    let _g = crate::crash::enter_quiet("dipui_can_player_accept");
    if let Some(e) = ENGINE.get() {
        let neg = rq(cco as usize + 0x218);
        if neg != 0 {
            let deal = (e.negotiation_deal)(neg as P) as usize;
            let queue = rq(rq(rq(neg + 8) + 0x2188) + 0x80);
            let faction = rq(rq(rq(cco as usize + 0x210) + 0x2188) + 0x1a8);
            if deal != 0 && queue != 0 && faction != 0 && readable(deal, 0x60) {
                if is_followup_respond_deal(deal) {
                    let id = rd(deal + 8) as u64;
                    let new_deal = RDEAL.swap(deal, Ordering::Relaxed) != deal;
                    if RDEAL_ID.swap(id, Ordering::Relaxed) != id || new_deal {
                        log!("follow-up answer fix: offer to the player, deal {deal:#x} id {id} ({})", crate::diptrace::record_key(rq(deal + 0x48)));
                    }
                    RDEAL_STATE.store(rq(deal + 0x48), Ordering::Relaxed);
                    RQUEUE.store(queue, Ordering::Relaxed);
                    RFACTION.store(faction, Ordering::Relaxed);
                } else {
                    // any other offer the player answers: forget the follow-up
                    RDEAL.store(0, Ordering::Relaxed);
                }
            }
        }
    }
    h.call(cco, out)
}

unsafe extern "C" fn accept_detour(cco: P, b: P) -> u64 {
    let Some(h) = ACCEPT.get() else { return 0 };
    let _g = crate::crash::enter("dipui_accept");
    ENGINE_RESPOND_MS.store(now_ms(), Ordering::Relaxed);
    h.call(cco, b)
}

unsafe extern "C" fn reject_detour(cco: P, b: P) -> u64 {
    let Some(h) = REJECT.get() else { return 0 };
    let _g = crate::crash::enter("dipui_reject");
    ENGINE_RESPOND_MS.store(now_ms(), Ordering::Relaxed);
    h.call(cco, b)
}

/// The remembered offer still has the id and waits in the node it was shown in.
unsafe fn respond_deal_waiting() -> bool {
    let deal = RDEAL.load(Ordering::Relaxed);
    deal != 0 && readable(deal, 0x60) && rd(deal + 8) as u64 == RDEAL_ID.load(Ordering::Relaxed) && rq(deal + 0x48) == RDEAL_STATE.load(Ordering::Relaxed) && is_followup_respond_deal(deal)
}

/// A queued answer: op 7 / 6 for the remembered follow-up deal, exactly what Accept / Reject send.
unsafe fn send_respond(e: &Engine, asked: u64) {
    let now = now_ms();
    let op = RESP_OP.load(Ordering::Relaxed);
    let (deal, queue, faction) = (RDEAL.load(Ordering::Relaxed), RQUEUE.load(Ordering::Relaxed), RFACTION.load(Ordering::Relaxed));
    let same = respond_deal_waiting();
    if !same || !readable(queue, 8) || faction == 0 || (op != OP_ACCEPT && op != OP_REJECT) || now.saturating_sub(asked) > 1000 || now.saturating_sub(ENGINE_RESPOND_MS.load(Ordering::Relaxed)) <= 500 {
        log!("follow-up answer fix: request dropped (deal {deal:#x} unchanged {same}, op {op})");
        return;
    }
    let mut command = [0u8; 0x80];
    let c = (e.command_build_respond)(command.as_mut_ptr(), deal as P, op, faction as P);
    (e.command_send)(queue as P, c);
    ENGINE_RESPOND_MS.store(now_ms(), Ordering::Relaxed);
    log!("follow-up answer fix: sent {} for deal {deal:#x} id {}", if op == OP_ACCEPT { "accept" } else { "reject" }, rd(deal + 8));
}

/// Per-frame campaign UI update, main thread: carry out a queued request once it has returned.
unsafe extern "C" fn ui_update_detour(ui: P, x: P) -> u64 {
    let Some(h) = UI_UPDATE.get() else { return 0 };
    let _g = crate::crash::enter_quiet("campaign_ui_update");
    let r = h.call(ui, x);
    let asked_respond = RESP_REQUEST_MS.swap(0, Ordering::Relaxed);
    if asked_respond != 0 {
        if let Some(e) = ENGINE.get() { send_respond(e, asked_respond); }
    }
    let asked = REQUEST_MS.swap(0, Ordering::Relaxed);
    if asked == 0 { return r; }
    let Some(e) = ENGINE.get() else { return r };
    let now = now_ms();
    let (deal, queue) = (DEAL.load(Ordering::Relaxed), QUEUE.load(Ordering::Relaxed));
    // still the deal that was shown, still waiting in the state it was shown in
    let same = deal != 0 && readable(deal, 0x60) && rd(deal + 8) as u64 == DEAL_ID.load(Ordering::Relaxed) && rq(deal + 0x48) == DEAL_STATE.load(Ordering::Relaxed) && is_followup_deal(deal);
    if !same || !readable(queue, 8) || now.saturating_sub(asked) > 1000 || now.saturating_sub(ENGINE_PROPOSE_MS.load(Ordering::Relaxed)) <= 500 {
        log!("follow-up button fix: request dropped (deal {deal:#x} unchanged {same})");
        return r;
    }
    let mut command = [0u8; 0x80];
    crate::dipui::around_propose("script extender", || {
        let c = (e.command_build)(command.as_mut_ptr(), deal as P, OP_PROPOSE);
        (e.command_send)(queue as P, c)
    });
    ENGINE_PROPOSE_MS.store(now_ms(), Ordering::Relaxed);
    log!("follow-up button fix: sent propose for deal {deal:#x} id {}", rd(deal + 8));
    r
}

pub fn install(t: &Table) {
    // SAFETY: the three hooked prologues are anchor-verified stack stores / pushes / mov rax,rsp;
    // the other three addresses are only called.
    unsafe {
        let _ = ENGINE.set(Engine {
            negotiation_deal: core::mem::transmute(t.get("dip_negotiation_deal")),
            command_build: core::mem::transmute(t.get("dip_command_build")),
            command_send: core::mem::transmute(t.get("dipui_send")),
            command_build_respond: core::mem::transmute(t.get("dip_command_build_respond")),
        });
        if !crate::build::hook_enabled("followup_hooks") {
            log!("follow-up button fix off (followup_hooks=0 in script_extender.cfg)");
            return;
        }
        for (name, slot, detour) in [("dipui_propose_deal", &PROPOSE, propose_detour as F2), ("dipui_can_propose", &CAN_PROPOSE, can_propose_detour as F2), ("campaign_ui_update", &UI_UPDATE, ui_update_detour as F2)] {
            let target: F2 = core::mem::transmute(t.get(name));
            let Ok(d) = GenericDetour::new(target, detour) else {
                log!("follow-up button fix: could not create the detour on {name}");
                return;
            };
            let _ = slot.set(d);
            let Some(d) = slot.get() else { return };
            if let Err(e) = crate::freeze::enable_detour(name, "followup_hooks", target as usize, d) {
                log!("follow-up button fix: could not enable the detour on {name}: {e}");
                return;
            }
        }
    }
    log!("follow-up negotiation button fix installed (se_followup_propose)");
    if !crate::build::hook_enabled("followup_respond_hook") {
        log!("follow-up answer fix off (followup_respond_hook=0 in script_extender.cfg)");
        return;
    }
    // SAFETY: the three prologues are anchor-verified stack stores / pushes.
    unsafe {
        for (name, slot, detour) in [("dipui_accept", &ACCEPT, accept_detour as F2), ("dipui_reject", &REJECT, reject_detour as F2), ("dipui_can_player_accept", &CAN_ACCEPT, can_accept_detour as F2)] {
            let target: F2 = core::mem::transmute(t.get(name));
            let Ok(d) = GenericDetour::new(target, detour) else {
                log!("follow-up answer fix: could not create the detour on {name}");
                return;
            };
            let _ = slot.set(d);
            let Some(d) = slot.get() else { return };
            if let Err(e) = crate::freeze::enable_detour(name, "followup_respond_hook", target as usize, d) {
                log!("follow-up answer fix: could not enable the detour on {name}: {e}");
                return;
            }
        }
    }
    log!("follow-up answer fix installed (se_followup_respond)");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_followup_propose", se_followup_propose);
    lua::set_global_fn(l, "se_followup_respond", se_followup_respond);
}

/// se_followup_respond("accept"|"reject") -> ok, msg ; se_followup_respond("installed") -> bool, msg
unsafe extern "C" fn se_followup_respond(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let installed = CAN_ACCEPT.get().is_some() && UI_UPDATE.get().is_some();
    let arg = if (api.gettop)(l) >= 1 { lua::to_str(l, 1) } else { String::new() };
    if arg == "installed" {
        (api.pushboolean)(l, installed as c_int);
        lua::push_str(l, if installed { "installed" } else { "not installed (followup_hooks=0 or followup_respond_hook=0 in script_extender.cfg)" });
        return 2;
    }
    let now = now_ms();
    let op = match arg.as_str() { "accept" => OP_ACCEPT, "reject" => OP_REJECT, _ => 0 };
    let (ok, msg) = if !installed {
        (false, "not installed (followup_hooks=0 or followup_respond_hook=0 in script_extender.cfg)".to_string())
    } else if op == 0 {
        (false, format!("unknown answer '{arg}' (accept or reject)"))
    } else if RDEAL.load(Ordering::Relaxed) == 0 {
        (false, "no follow-up offer to the player has been shown".to_string())
    } else if !respond_deal_waiting() {
        // answered or gone: a stale offer must not swallow a click meant for another popup
        RDEAL.store(0, Ordering::Relaxed);
        (false, "the follow-up offer is no longer waiting".to_string())
    } else if now.saturating_sub(ENGINE_RESPOND_MS.load(Ordering::Relaxed)) <= 500 {
        (false, "the engine handled this click itself".to_string())
    } else {
        RESP_OP.store(op, Ordering::Relaxed);
        RESP_REQUEST_MS.store(now, Ordering::Relaxed);
        (true, "queued for the UI thread".to_string())
    };
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &msg);
    2
}

unsafe extern "C" fn se_followup_propose(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let now = now_ms();
    let installed = UI_UPDATE.get().is_some();
    // se_followup_propose("installed") only asks whether the detours are in (nothing is queued)
    if (api.gettop)(l) >= 1 && lua::to_str(l, 1) == "installed" {
        (api.pushboolean)(l, installed as c_int);
        lua::push_str(l, if installed { "installed" } else { "not installed (followup_hooks=0 in script_extender.cfg)" });
        return 2;
    }
    let (ok, msg) = if !installed {
        (false, "not installed (followup_hooks=0 in script_extender.cfg)".to_string())
    } else if DEAL.load(Ordering::Relaxed) == 0 {
        (false, "no negotiation popup has been shown yet".to_string())
    } else if now.saturating_sub(ENGINE_PROPOSE_MS.load(Ordering::Relaxed)) <= 500 {
        (false, "the engine handled this click itself".to_string())
    } else {
        REQUEST_MS.store(now, Ordering::Relaxed);
        (true, "queued for the UI thread".to_string())
    };
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &msg);
    2
}
