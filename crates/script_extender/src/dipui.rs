//! UI side of a diplomacy negotiation popup, made visible (notes/diplomacy.md). Read-only; part of
//! the `diag_diplomacy` trace (`diptrace.rs` installs it and owns the timeline).
//!
//! `diplomacy_followup_negotiation_popup_panel.twui.xml`: the call-for-action button is
//! ContextCommandLeftClick `ProposeDeal`, made inactive by `CanPropose` (CcoDiplomacyNegotiation,
//! handlers registered in FUN_14028cc90).
//!   FUN_142f8bf40 ProposeDeal(cco): FUN_142f70e60 (state offers action 10 = counter offer) -> sends
//!       op 10 + op 7; else when cco+0x210 -> +0x2a78, FUN_141acda60(deal), FUN_141ac79f0(deal),
//!       FUN_142fabe80(neg) or byte 0x1443d1a50 is set -> sends op 5 (propose) at once; else
//!       FUN_142db89b0(cco, FUN_142f52e40) true -> op 5; else FUN_142db89b0(cco, FUN_142f48950)
//!       FALSE -> RETURNS WITHOUT DOING ANYTHING; true -> FUN_142f11010(cco, FUN_142f6f230) (a
//!       confirmation) and returns. Sending = FUN_142f256e0(queue, command built by FUN_141b3f3b0).
//!   FUN_142f48cd0 CanPropose(cco, out): FUN_141ac54c0(deal) && state offers action 5 / 10 / 4 &&
//!       FUN_142f6ba90(neg, local faction) && (the predicates above).
//! Every probe below only speaks while one of the two handlers is on the stack; CanPropose runs
//! every frame, so it is written only when its answers change.

use crate::addrs::Table;
use crate::log;
use core::ffi::c_void;
use retour::GenericDetour;
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::sync::OnceLock;

type P = *mut c_void;
type F2 = unsafe extern "C" fn(P, P) -> u64;

thread_local! {
    /// 0 = outside, 1 = inside CanPropose, 2 = inside ProposeDeal
    static CTX: Cell<u8> = const { Cell::new(0) };
    static LINE: RefCell<String> = const { RefCell::new(String::new()) };
    static LAST_CAN: RefCell<String> = const { RefCell::new(String::new()) };
}

fn rva(p: usize) -> usize {
    let (base, size) = crate::process::main_module();
    if p >= base && p < base + size { p - base + 0x1_4000_0000 } else { p }
}

macro_rules! probe {
    ($static:ident, $detour:ident, $label:expr, $pred:expr) => {
        static $static: OnceLock<GenericDetour<F2>> = OnceLock::new();
        unsafe extern "C" fn $detour(a: P, b: P) -> u64 {
            let Some(h) = $static.get() else { return 0 };
            let _g = crate::crash::enter($label);
            if CTX.with(|c| c.get()) == 0 { return h.call(a, b); }
            // the predicate evaluator is handed {fn, 0}: name the predicate it is asked about
            let what = if $pred && !b.is_null() { format!("({:x})", rva(*(b as *const usize))) } else { String::new() };
            let r = h.call(a, b);
            LINE.with(|l| { let _ = write!(l.borrow_mut(), " {}{}={}", $label, what, r as u8); });
            r
        }
    };
}

probe!(IS_COUNTER, is_counter_detour, "counter_offer_state", false);
probe!(HAS_FORCED, has_forced_detour, "deal_has_forcing_component", false);
probe!(FABE80, fabe80_detour, "fabe80", false);
probe!(PRED, pred_detour, "predicate", true);
probe!(DEAL_VALID, deal_valid_detour, "deal_components_valid", false);
probe!(IS_PROPOSER, is_proposer_detour, "local_faction_is_proposer", false);
probe!(F710C0, f710c0_detour, "f710c0", false);
probe!(CONFIRM, confirm_detour, "CONFIRMATION_DIALOG", true);
probe!(SEND, send_detour, "COMMAND_SENT", false);

/// Called by `followup.rs`, which owns the detours on the two handlers: run `f` (the engine's
/// ProposeDeal) and write what it asked and did into the trace.
pub unsafe fn around_propose(who: &str, f: impl FnOnce() -> u64) -> u64 {
    if !crate::diptrace::tracing() { return f(); }
    let outer = CTX.with(|c| c.replace(2));
    LINE.with(|l| l.borrow_mut().clear());
    let r = f();
    CTX.with(|c| c.set(outer));
    let line = LINE.with(|l| core::mem::take(&mut *l.borrow_mut()));
    let verdict = if line.contains("COMMAND_SENT") { "sent a command" } else if line.contains("CONFIRMATION_DIALOG") { "asked for a confirmation" } else { "DID NOTHING" };
    crate::diptrace::note(format!("UI ProposeDeal ({who}) {verdict}:{line}"));
    r
}

/// The same for CanPropose, which runs every frame: written only when its answers change.
pub unsafe fn around_can_propose(f: impl FnOnce() -> u64) -> u64 {
    if !crate::diptrace::tracing() || CTX.with(|c| c.get()) != 0 { return f(); }
    CTX.with(|c| c.set(1));
    LINE.with(|l| l.borrow_mut().clear());
    let r = f();
    CTX.with(|c| c.set(0));
    let line = LINE.with(|l| core::mem::take(&mut *l.borrow_mut()));
    let changed = LAST_CAN.with(|last| {
        let mut last = last.borrow_mut();
        if *last == line { false } else { *last = line.clone(); true }
    });
    if changed { crate::diptrace::note(format!("UI CanPropose evaluated:{line}")); }
    r
}

pub fn install(t: &Table) {
    // SAFETY: every prologue is anchor-verified pushes / stack stores / sub rsp.
    unsafe {
        let hooks: [(&str, &OnceLock<GenericDetour<F2>>, F2); 9] = [
            ("dipui_is_counter", &IS_COUNTER, is_counter_detour),
            ("dipui_has_forced", &HAS_FORCED, has_forced_detour),
            ("dipui_fabe80", &FABE80, fabe80_detour),
            ("dipui_predicate", &PRED, pred_detour),
            ("dipui_deal_valid", &DEAL_VALID, deal_valid_detour),
            ("dipui_is_proposer", &IS_PROPOSER, is_proposer_detour),
            ("dipui_f710c0", &F710C0, f710c0_detour),
            ("dipui_confirm", &CONFIRM, confirm_detour),
            ("dipui_send", &SEND, send_detour),
        ];
        for (name, slot, detour) in hooks {
            let target: F2 = core::mem::transmute(t.get(name));
            let Ok(d) = GenericDetour::new(target, detour) else {
                log!("diplomacy ui trace: could not create the detour on {name}");
                return;
            };
            let _ = slot.set(d);
            let Some(d) = slot.get() else { return };
            if let Err(e) = crate::freeze::enable_detour(name, "diag_diplomacy", target as usize, d) {
                log!("diplomacy ui trace: could not enable the detour on {name}: {e}");
                return;
            }
        }
    }
    log!("diplomacy ui trace installed (what ProposeDeal / CanPropose ask)");
}
