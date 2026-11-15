//! Battle fixes ported from Alex Zhao's DLL mods (with technical help from Wyccc; ported with his
//! permission). Spec: 190Expanded Beta\Output\Batch1115\Fold\SE_PORT_SPEC.md; notes/battle_fixes.md.
//!
//! 1. **Elephant Attack Gate Fix** (cfg `battle_elephant_gate`, default 1). The gate-attack order
//!    path calls the per-entity predicate FUN_1422964c0 at exe+0x221c4a4; elephant entities (vtable
//!    exe+0x34fb728) fail it. The 5-byte call is redirected to `elephant_pred`, which returns the
//!    original answer, or true for an elephant whose +0x358 state is 0 and whose linked object at
//!    +0x348 has its +0x5f6 slot set (Alex's test, field meanings inferred).
//! 2. **Better Formation Behaviors** (cfg `battle_formation_fix`, default 1): 6 detours + 3 code
//!    patches + 2 .data floats, all or nothing (20 signatures, vtable slots, globals and the
//!    floats are checked first; one mismatch = nothing is written):
//!    - shield / spear wall max columns 22 -> 60 (imm32 at exe+0x2198af5 / exe+0x2198bb2);
//!    - the timed "cancel N s after melee" branch at exe+0x23c4f38 NOPed;
//!    - Cantabrian circle distance 5 -> 30 and radius 28 -> 32 (.data exe+0x41a6d80 / exe+0x41a6a70,
//!      set by the exe's static initialisers at exe+0x208731 / exe+0x208891 only);
//!    - D1 formation_cancel_callback: never cancels a shield-wall behaviour (both wall classes);
//!    - D2 maximum_width: drag width matching 60 columns;
//!    - D3 charge_query: "not charging" for a shield-wall unit when asked from the interrupt
//!      timer (return address exe+0x23c4ec7);
//!    - D4 active_formation: at the two missile-block angle checks (return addresses
//!      exe+0x22ef827 / exe+0x22fc21d) formations 5 / 9 / 10 count as turtle (11): 360-degree block;
//!    - D5 morale_contribution: flank / rear (0x1b / 0x1c) negative morale ignored in 5 / 9 / 10;
//!    - D6 charge_update: after the native charge state resolves, a Halt re-forms the wall.
//!
//! D3 and D4 depend on the caller's return address, so they are hand-written stubs (global_asm
//! below) entered by the detour's `jmp`: on entry [rsp] is the engine caller's return address,
//! and the stubs return straight to it. D3 jumps on to the original; D4 calls it.
//!
//! Coexistence with Alex's own DLLs (elephant_gate_fix_<sha>.dll / z_shieldwall_rebuilt.dll, loaded
//! by his Lua from the game folder): if one is loaded, or the bytes at our sites are not vanilla,
//! that fix stays inert and the log says why. If the SE patched first, his own pre-checks fail on
//! our bytes and he applies nothing ("signature conflict, no changes applied"), so nothing is ever
//! patched twice.
//!
//! MP: every fix changes the battle simulation; both keys are in build::sync_tag, read once.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use retour::RawDetour;
use std::sync::OnceLock;

type P = *mut c_void;

// --------------------------------------------------------------------------------------------
// cfg, read once (install and the sync tag see the same value for the whole session)
// --------------------------------------------------------------------------------------------

pub const KEY_ELEPHANT: &str = "battle_elephant_gate";
pub const KEY_FORMATION: &str = "battle_formation_fix";
static CFG_ELEPHANT: OnceLock<bool> = OnceLock::new();
static CFG_FORMATION: OnceLock<bool> = OnceLock::new();
pub fn elephant_enabled() -> bool { *CFG_ELEPHANT.get_or_init(|| crate::build::hook_enabled(KEY_ELEPHANT)) }
pub fn formation_enabled() -> bool { *CFG_FORMATION.get_or_init(|| crate::build::hook_enabled(KEY_FORMATION)) }

// --------------------------------------------------------------------------------------------
// RVAs (1.7.2.0 Steam + Epic share them; anchors verify every one)
// --------------------------------------------------------------------------------------------

const ELE_ANCHOR_RVA: usize = 0x221c498;
const ELE_ANCHOR: [u8; 39] = [
    0x45, 0x38, 0xae, 0x98, 0x00, 0x00, 0x00, 0x75, 0x5d, 0x49, 0x8b, 0xcf, 0xe8, 0x17, 0xa0, 0x07, 0x00, 0x3c, 0x01, 0x75,
    0x51, 0x49, 0x8b, 0x8e, 0x90, 0x00, 0x00, 0x00, 0x33, 0xd2, 0xe8, 0xd5, 0xef, 0x06, 0x00, 0x3c, 0x01, 0x75, 0x3f,
];
const ELE_SITE_RVA: usize = 0x221c4a4; // the `call FUN_1422964c0` inside the anchor (offset 12)
const ELE_PRED_RVA: usize = 0x22964c0;
const ELE_VT_RVA: usize = 0x34fb728;

const RVA_CANCEL: usize = 0x23b3930;
const RVA_MAX_WIDTH: usize = 0x23ae110;
const RVA_CHARGE_QUERY: usize = 0x21ac920;
const RVA_ACTIVE_FORMATION: usize = 0x21ad6d0;
const RVA_MORALE: usize = 0x239a400;
const RVA_CHARGE_UPDATE: usize = 0x25eb5f0;
const RVA_ISSUE_HALT: usize = 0x23abfe0;
const RET_INTERRUPT_TIMER: usize = 0x23c4ec7; // after `call charge_query` at 0x23c4ec2
const RET_MISSILE_A: usize = 0x22ef827; // after `call active_formation` at 0x22ef822
const RET_MISSILE_B: usize = 0x22fc21d; // after `call active_formation` at 0x22fc218
const VT_WALL_A: usize = 0x3550058; // the two shield-wall formation-behaviour classes
const VT_WALL_B: usize = 0x35503e8;
const VT_WIDTH_A: usize = 0x350c6c0; // formation shapes maximum_width accepts
const VT_WIDTH_B: usize = 0x350c560;
const CHARGE_STATE_RVA: usize = 0x3c1a6e0; // the state object charge_update returns after a charge
const FLOAT_CANT_DIST: usize = 0x41a6d80;
const FLOAT_CANT_RADIUS: usize = 0x41a6a70;

/// Alex's 20 pre-check signatures {name, rva, bytes} (DLL table at 0x35480; each hits exactly once
/// in .text, at this RVA, on the installed exe: tools/battlefix/dump_sigs.py).
const SIGS: [(&str, usize, &str); 20] = [
    ("issue_halt", 0x23abfe0, "48895c24084889742410574883ec30488bd98bf24881c1803b0000e8902dfeff"),
    ("charge_update", 0x25eb5f0, "40534883ec2033c0488d5ad04885d2480f44d8488b4308488b8b80000000488b90100a0000e8b6"),
    ("resolve_stance", 0x22bab60, "48895c241848896c2420574883ec20488b018bda488bf9ff90c0030000bdffff"),
    ("entity_attack_order", 0x22ae8a0, "48895c2408488974241048897c241841564883ec3080b94806000001488bfa488bd9751a4881c130050000740a488b01ba01000000ff10c68348060000004c8db330050000c68349060000004d85f60f841a"),
    ("stance_order", 0x22afae0, "48895c2408488974241048897c241841564883ec3080b94806000001488bfa488bd9751a4881c130050000740a488b01ba01000000ff10c68348060000004c8db330050000c68349060000004d85f60f84dd"),
    ("formation_melee_policy", 0x2611460, "48895c24084889742410574883ec20488b4108488bf1488b98100a0000488bcb"),
    ("entity_mobility_policy", 0x23b8a80, "4883ec28488b89703c0000ba02000000e83b4cdfff84c00f94c04883c428c3"),
    ("charge_state_selection", 0x2603e70, "488d0561686101c3cccccccccccccccc"),
    ("cantabrian_distance_load", 0x23901ce, "f30f1035aa6be101"),
    ("cantabrian_radius_load", 0x23900d5, "f30f100d9369e101"),
    ("formation_cancel_callback", 0x23b3930, "48895c2408574883ec40488b7918488bd9488b91c0000000488b8f703c0000e80c9ddfff"),
    ("maximum_width", 0x23ae110, "40574883ec20488bf9e84294ffff84c0488b8708330000488b88300b0000488b"),
    ("charge_query", 0x21ac920, "4883ec2883b9c808000000750732c04883c428c34883c130e813624600488bc8488b10ff52604885"),
    ("shield_shape_columns", 0x2198af2, "c7433016000000488d05c03b3701488903"),
    ("spear_shape_columns", 0x2198ba8, "488d05b1393701c7473016000000488907"),
    ("active_formation", 0x21ad6d0, "48895c24084889742410574883ec20488b5920488d79188bf2483bdf74216690488b4320488d4b10"),
    ("missile_a", 0x22ef816, "ba0b000000488b88703c0000e8a9deebff84c07412"),
    ("missile_b", 0x22fc20c, "ba0b000000488b88703c0000e8b314ebff84c07412"),
    ("morale_contribution", 0x239a400, "4889742418574883ec408379583c488d7958418bf04c8bc9448bc20f83040100"),
    ("interrupt_timer", 0x23c4f2d, "0f2fc1f30f1187e40000000f835201000048895c2450488d4f0848"),
];

/// Code patches: (name, rva, vanilla bytes, new bytes). Each lies inside one of the SIGS above.
const CODE_PATCHES: [(&str, usize, &[u8], &[u8]); 3] = [
    ("shield_shape_columns", 0x2198af5, &[0x16, 0, 0, 0], &[0x3c, 0, 0, 0]),
    ("spear_shape_columns", 0x2198bb2, &[0x16, 0, 0, 0], &[0x3c, 0, 0, 0]),
    ("interrupt_timer", 0x23c4f38, &[0x0f, 0x83, 0x52, 0x01, 0x00, 0x00], &[0x90; 6]),
];
/// .data floats: (name, rva, vanilla, new)
const FLOAT_PATCHES: [(&str, usize, f32, f32); 2] = [
    ("cantabrian_distance", FLOAT_CANT_DIST, 5.0, 30.0),
    ("cantabrian_radius", FLOAT_CANT_RADIUS, 28.0, 32.0),
];
const WALL_VT_SLOTS: [usize; 5] = [0x70, 0xd8, 0xe0, 0xf0, 0x100];

const FORMATION_CIRCLE_ETC: [u32; 3] = [5, 9, 10];
const FORMATION_TURTLE: u32 = 11;
const MORALE_FLANK: u32 = 0x1b;
const MORALE_REAR: u32 = 0x1c;
const WALL_MAX_COLUMNS: u32 = 60;

// --------------------------------------------------------------------------------------------
// state shared with the stubs (the asm reads these as plain usize)
// --------------------------------------------------------------------------------------------

static BASE: AtomicUsize = AtomicUsize::new(0);
static SE_BF_CQ_RET: AtomicUsize = AtomicUsize::new(usize::MAX); // live exe+RET_INTERRUPT_TIMER
static SE_BF_CQ_ORIG: AtomicUsize = AtomicUsize::new(0); // charge_query trampoline
static SE_BF_AF_ORIG: AtomicUsize = AtomicUsize::new(0); // active_formation trampoline
static ORIG_ELE: AtomicUsize = AtomicUsize::new(0);
static ORIG_CANCEL: AtomicUsize = AtomicUsize::new(0);
static ORIG_WIDTH: AtomicUsize = AtomicUsize::new(0);
static ORIG_MORALE: AtomicUsize = AtomicUsize::new(0);
static ORIG_CHARGE_UPDATE: AtomicUsize = AtomicUsize::new(0);
static DETOURS: OnceLock<Vec<(&'static str, RawDetour)>> = OnceLock::new();

/// Install outcome per fix, for the log and se.diag.battle_fixes(): "" = not tried yet.
static ELE_STATE: OnceLock<String> = OnceLock::new();
static FORM_STATE: OnceLock<String> = OnceLock::new();

macro_rules! counters {
    ($($name:ident),*) => { $(static $name: AtomicU64 = AtomicU64::new(0);)* };
}
counters!(
    ELE_CALLS, ELE_ORIG_TRUE, ELE_PASSED, CANCEL_CALLS, CANCEL_SUPPRESSED, WIDTH_CALLS, WIDTH_SET,
    CQ_GATED, CQ_NOT_CHARGING, AF_TURTLE_MISS, AF_GATED, AF_BLOCKED, MORALE_FLANK_NEG, MORALE_IGNORED,
    CU_CHARGE_STATE, CU_HALTS
);
#[inline]
fn bump(c: &AtomicU64) { c.fetch_add(1, Ordering::Relaxed); }

// --------------------------------------------------------------------------------------------
// small readers (engine pointers are trusted once non-null, exactly as Alex's code does)
// --------------------------------------------------------------------------------------------

#[inline]
unsafe fn rq(p: usize) -> usize { core::ptr::read_unaligned(p as *const usize) }
#[inline]
unsafe fn rd(p: usize) -> u32 { core::ptr::read_unaligned(p as *const u32) }
#[inline]
unsafe fn rf(p: usize) -> f32 { core::ptr::read_unaligned(p as *const f32) }
#[inline]
fn base() -> usize { BASE.load(Ordering::Relaxed) }

/// Alex's IsShieldForm(unit): formation state +0x3320 in {1,2} and the behaviour object at +0x3310
/// (checked in both states) or, in state 2, at +0x3318 is one of the two shield-wall classes.
unsafe fn is_shield_form(unit: usize) -> bool {
    if unit == 0 { return false; }
    let st = rd(unit + 0x3320);
    if st.wrapping_sub(1) > 1 { return false; }
    let (a, b) = (base() + VT_WALL_A, base() + VT_WALL_B);
    let o = rq(unit + 0x3310);
    if o != 0 { let vt = rq(o); if vt == a || vt == b { return true; } }
    if st != 2 { return false; }
    let o = rq(unit + 0x3318);
    if o == 0 { return false; }
    let vt = rq(o);
    vt == a || vt == b
}

// --------------------------------------------------------------------------------------------
// 1. elephant gate
// --------------------------------------------------------------------------------------------

unsafe extern "C" fn elephant_pred(e: P) -> u8 {
    bump(&ELE_CALLS);
    let orig: unsafe extern "C" fn(P) -> u8 = core::mem::transmute(ORIG_ELE.load(Ordering::Relaxed));
    if orig(e) != 0 { bump(&ELE_ORIG_TRUE); return 1; }
    let e = e as usize;
    if e == 0 || rq(e) != base() + ELE_VT_RVA || rd(e + 0x358) != 0 { return 0; }
    let linked = rq(e + 0x348);
    if linked == 0 { return 0; }
    let ok = core::ptr::read_unaligned((linked + 0x5f6) as *const u16) != 0xffff;
    if ok { bump(&ELE_PASSED); }
    ok as u8
}

unsafe fn install_elephant(b: usize) -> Result<(), String> {
    use crate::engineguard::{alloc_near, bytes_at, make_executable, write_code_for};
    let anchor = b + ELE_ANCHOR_RVA;
    match bytes_at::<39>(anchor) {
        Some(x) if x == ELE_ANCHOR => {}
        Some(x) => {
            let site = &x[12..17];
            return Err(if site[0] == 0xe8 && site != &ELE_ANCHOR[12..17] {
                format!("the predicate call at exe+{ELE_SITE_RVA:#x} is already redirected ({}): another native mod (Alex Zhao's elephant_gate_fix DLL?) patched it", hex(site))
            } else {
                format!("anchor bytes at exe+{ELE_ANCHOR_RVA:#x} differ ({})", hex(&x))
            });
        }
        None => return Err(format!("exe+{ELE_ANCHOR_RVA:#x} not readable")),
    }
    let site = b + ELE_SITE_RVA;
    ORIG_ELE.store(b + ELE_PRED_RVA, Ordering::Relaxed);
    let page = alloc_near(site).ok_or("no free page within rel32 reach")?;
    // mov rax, imm64 ; jmp rax   (rax is free at a call target)
    let mut code = vec![0x48, 0xb8];
    code.extend_from_slice(&(elephant_pred as *const () as u64).to_le_bytes());
    code.extend_from_slice(&[0xff, 0xe0]);
    core::ptr::copy_nonoverlapping(code.as_ptr(), page as *mut u8, code.len());
    make_executable(page, code.len());
    let rel = page as i64 - (site as i64 + 5);
    if rel < i32::MIN as i64 || rel > i32::MAX as i64 { return Err("stub out of rel32 reach".into()); }
    let mut patch = [0xe8u8, 0, 0, 0, 0];
    patch[1..].copy_from_slice(&(rel as i32).to_le_bytes());
    write_code_for("battle_elephant_gate", KEY_ELEPHANT, site, &patch)
}

// --------------------------------------------------------------------------------------------
// 2. formation detours
// --------------------------------------------------------------------------------------------

/// D1: a shield-wall behaviour never cancels itself after melee.
unsafe extern "C" fn cancel_detour(this: P) -> u8 {
    bump(&CANCEL_CALLS);
    if !this.is_null() {
        let vt = rq(this as usize);
        if vt == base() + VT_WALL_A || vt == base() + VT_WALL_B { bump(&CANCEL_SUPPRESSED); return 0; }
    }
    let orig: unsafe extern "C" fn(P) -> u8 = core::mem::transmute(ORIG_CANCEL.load(Ordering::Relaxed));
    orig(this)
}

/// D2: a wall may be dragged as wide as 60 columns. Same arithmetic as Alex's binary:
/// w = (min(men,60)-1) * spacing + base, used when w is finite, spacing > 0 and base > 0.
unsafe extern "C" fn width_detour(unit: P) -> f32 {
    let orig: unsafe extern "C" fn(P) -> f32 = core::mem::transmute(ORIG_WIDTH.load(Ordering::Relaxed));
    let w0 = orig(unit);
    bump(&WIDTH_CALLS);
    let u = unit as usize;
    if u == 0 || !is_shield_form(u) { return w0; }
    let a = rq(u + 0x3308);
    if a == 0 { return w0; }
    let shape = rq(a + 0xb30);
    if shape == 0 { return w0; }
    let vt = rq(shape);
    if vt != base() + VT_WIDTH_A && vt != base() + VT_WIDTH_B { return w0; }
    let men = rd(u + 0x194);
    if men == 0 { return w0; }
    let (spacing, base_w) = (rf(shape + 0x18), rf(shape + 0x10));
    let w = (men.min(WALL_MAX_COLUMNS) - 1) as i32 as f32 * spacing + base_w;
    if !(w.abs() <= f32::MAX) || !(spacing > 0.0) { return w0; }
    if 0.0 < base_w { bump(&WIDTH_SET); w } else { w0 }
}

/// D3 decision, called by the stub only when the caller is the interrupt timer: true = report
/// "not charging" for this shield-wall unit.
unsafe extern "C" fn se_bf_cq_decide(q: P) -> u8 {
    bump(&CQ_GATED);
    let unit = rq(q as usize + 0xa10);
    let r = is_shield_form(unit);
    if r { bump(&CQ_NOT_CHARGING); }
    r as u8
}

/// D4 extra, called by the stub after the original said "not turtle" for id 11: true when the
/// caller is one of the two missile-block angle checks and the unit is in formation 5, 9 or 10.
unsafe extern "C" fn se_bf_af_extra(formations: P, ret: usize) -> u8 {
    bump(&AF_TURTLE_MISS);
    let b = base();
    if ret != b + RET_MISSILE_A && ret != b + RET_MISSILE_B { return 0; }
    bump(&AF_GATED);
    let orig: unsafe extern "C" fn(P, u32) -> u8 = core::mem::transmute(SE_BF_AF_ORIG.load(Ordering::Relaxed));
    let hit = FORMATION_CIRCLE_ETC.iter().any(|&id| orig(formations, id) != 0);
    if hit { bump(&AF_BLOCKED); }
    hit as u8
}

/// D5: no negative flank / rear morale while in formation 5, 9 or 10.
unsafe extern "C" fn morale_detour(this: P, kind: u32, value: i32) -> usize {
    let orig: unsafe extern "C" fn(P, u32, i32) -> usize = core::mem::transmute(ORIG_MORALE.load(Ordering::Relaxed));
    if (kind == MORALE_FLANK || kind == MORALE_REAR) && value < 0 && !this.is_null() {
        bump(&MORALE_FLANK_NEG);
        let owner = rq(this as usize + 0x10);
        if owner != 0 {
            let formations = rq(owner + 0x3c70);
            if formations != 0 {
                let af: unsafe extern "C" fn(P, u32) -> u8 = core::mem::transmute(SE_BF_AF_ORIG.load(Ordering::Relaxed));
                if FORMATION_CIRCLE_ETC.iter().any(|&id| af(formations as P, id) != 0) {
                    bump(&MORALE_IGNORED);
                    return 1;
                }
            }
        }
    }
    orig(this, kind, value)
}

/// D6: once the native charge has resolved into the charge state, a Halt re-forms the wall.
unsafe extern "C" fn charge_update_detour(a: P, b: P) -> P {
    let orig: unsafe extern "C" fn(P, P) -> P = core::mem::transmute(ORIG_CHARGE_UPDATE.load(Ordering::Relaxed));
    let r = orig(a, b);
    if r as usize != base() + CHARGE_STATE_RVA || b.is_null() { return r; }
    bump(&CU_CHARGE_STATE);
    let e = rq(b as usize - 0x28);
    if e == 0 { return r; }
    let unit = rq(e + 0xa10);
    if unit != 0 && is_shield_form(unit) {
        bump(&CU_HALTS);
        let halt: unsafe extern "C" fn(P, u32) = core::mem::transmute(base() + RVA_ISSUE_HALT);
        halt(unit as P, 0);
    }
    r
}

extern "C" {
    fn se_bf_cq_entry();
    fn se_bf_af_entry();
}

// D3 stub (charge_query). Entered by the detour jmp, so [rsp] on entry is the engine caller's
// return address. Saves the argument registers (rcx rdx r8 r9 xmm0-3) around the decision call,
// then either returns 0 to the caller or jumps on to the original with everything unchanged.
// Frame: 4 pushes + 0x68 -> ret addr at [rsp+0x88]; rsp is 16-aligned at the call.
core::arch::global_asm!(
    ".globl se_bf_cq_entry",
    ".seh_proc se_bf_cq_entry",
    "se_bf_cq_entry:",
    "push rcx",
    ".seh_pushreg rcx",
    "push rdx",
    ".seh_pushreg rdx",
    "push r8",
    ".seh_pushreg r8",
    "push r9",
    ".seh_pushreg r9",
    "sub rsp, 0x68",
    ".seh_stackalloc 0x68",
    ".seh_endprologue",
    "mov rax, qword ptr [rip + {cq_ret}]",
    "cmp qword ptr [rsp + 0x88], rax",
    "jne 3f",
    "test rcx, rcx",
    "jz 3f",
    "movdqu xmmword ptr [rsp + 0x20], xmm0",
    "movdqu xmmword ptr [rsp + 0x30], xmm1",
    "movdqu xmmword ptr [rsp + 0x40], xmm2",
    "movdqu xmmword ptr [rsp + 0x50], xmm3",
    "call {cq_decide}",
    "movdqu xmm0, xmmword ptr [rsp + 0x20]",
    "movdqu xmm1, xmmword ptr [rsp + 0x30]",
    "movdqu xmm2, xmmword ptr [rsp + 0x40]",
    "movdqu xmm3, xmmword ptr [rsp + 0x50]",
    "test al, al",
    "jz 3f",
    "xor eax, eax",
    "add rsp, 0x68",
    "pop r9",
    "pop r8",
    "pop rdx",
    "pop rcx",
    "ret",
    "3:",
    "add rsp, 0x68",
    "pop r9",
    "pop r8",
    "pop rdx",
    "pop rcx",
    "jmp qword ptr [rip + {cq_orig}]",
    ".seh_endproc",
    cq_ret = sym SE_BF_CQ_RET,
    cq_orig = sym SE_BF_CQ_ORIG,
    cq_decide = sym se_bf_cq_decide,
);

// D4 stub (active_formation(formations, id) -> bool). Captures the caller's return address
// ([rsp] on entry = [rsp+0x38] after the frame), calls the original with every argument register
// untouched, and only for a false answer to id 11 asks se_bf_af_extra. Returns to the caller.
core::arch::global_asm!(
    ".globl se_bf_af_entry",
    ".seh_proc se_bf_af_entry",
    "se_bf_af_entry:",
    "push rdi",
    ".seh_pushreg rdi",
    "push rsi",
    ".seh_pushreg rsi",
    "push rbx",
    ".seh_pushreg rbx",
    "sub rsp, 0x20",
    ".seh_stackalloc 0x20",
    ".seh_endprologue",
    "mov rdi, qword ptr [rsp + 0x38]",
    "mov rsi, rcx",
    "mov ebx, edx",
    "call qword ptr [rip + {af_orig}]",
    "test al, al",
    "jnz 4f",
    "cmp ebx, {turtle}",
    "jne 4f",
    "mov rcx, rsi",
    "mov rdx, rdi",
    "call {af_extra}",
    "4:",
    "add rsp, 0x20",
    "pop rbx",
    "pop rsi",
    "pop rdi",
    "ret",
    ".seh_endproc",
    af_orig = sym SE_BF_AF_ORIG,
    af_extra = sym se_bf_af_extra,
    turtle = const FORMATION_TURTLE,
);

// --------------------------------------------------------------------------------------------
// formation install: all or nothing
// --------------------------------------------------------------------------------------------

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn unhex(s: &str) -> Vec<u8> { (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect() }

unsafe fn bytes(addr: usize, n: usize) -> Option<Vec<u8>> {
    extern "system" { fn IsBadReadPtr(p: *const c_void, n: usize) -> i32; }
    if IsBadReadPtr(addr as *const c_void, n) != 0 { return None; }
    Some(core::slice::from_raw_parts(addr as *const u8, n).to_vec())
}

/// Every pre-check of Alex's DLL; Err lists every failure (nothing has been written yet).
unsafe fn formation_prechecks(b: usize) -> Result<(), String> {
    let mut bad = Vec::new();
    let mut hooked = Vec::new();
    for (name, rva, pat) in SIGS {
        let want = unhex(pat);
        match bytes(b + rva, want.len()) {
            Some(got) if got == want => {}
            Some(got) => {
                if matches!(got[0], 0xe9 | 0xe8 | 0xff) && rva_is_hook_target(rva) { hooked.push(name); }
                bad.push(format!("{name}@{rva:#x}"));
            }
            None => bad.push(format!("{name}@{rva:#x} unreadable")),
        }
    }
    for vt in [VT_WALL_A, VT_WALL_B] {
        for s in WALL_VT_SLOTS {
            if bytes(b + vt + s, 8).map(|_| rq(b + vt + s)) != Some(b + RVA_CANCEL) { bad.push(format!("vtable {vt:#x}+{s:#x}")); }
        }
    }
    for (at, want) in [(0x3585700usize, 0x2603e70usize), (0x3585708, 0x2603dd0), (CHARGE_STATE_RVA, 0x3581640)] {
        if bytes(b + at, 8).map(|_| rq(b + at)) != Some(b + want) { bad.push(format!("[exe+{at:#x}] != exe+{want:#x}")); }
    }
    for (name, rva, vanilla, _) in FLOAT_PATCHES {
        let v = rf(b + rva);
        if v != vanilla { bad.push(format!("{name} float = {v} (want {vanilla})")); }
    }
    if bad.is_empty() { return Ok(()); }
    let why = if !hooked.is_empty() {
        format!("{} already hooked by another native mod (Alex Zhao's z_shieldwall_rebuilt DLL?); ", hooked.join(", "))
    } else { String::new() };
    Err(format!("{why}{} pre-check(s) failed: {}", bad.len(), bad.join(", ")))
}

fn rva_is_hook_target(rva: usize) -> bool {
    [RVA_CANCEL, RVA_MAX_WIDTH, RVA_CHARGE_QUERY, RVA_ACTIVE_FORMATION, RVA_MORALE, RVA_CHARGE_UPDATE].contains(&rva)
}

unsafe fn enable_raw(name: &'static str, target: usize, d: &RawDetour) -> Result<(), String> {
    let before: [u8; 16] = crate::engineguard::bytes_at(target).unwrap_or([0; 16]);
    crate::freeze::with_threads_frozen(target, 16, || d.enable())?.map_err(|e| e.to_string())?;
    crate::status::record_patch(name, KEY_FORMATION, target, before, crate::engineguard::bytes_at(target).unwrap_or([0; 16]));
    crate::crash::hook_installed(name, KEY_FORMATION, target);
    Ok(())
}

unsafe fn install_formation(b: usize) -> Result<(), String> {
    formation_prechecks(b)?;
    // create every detour first: nothing is written until all of them exist
    let specs: [(&'static str, usize, usize, &AtomicUsize); 6] = [
        ("formation_cancel_callback", RVA_CANCEL, cancel_detour as *const () as usize, &ORIG_CANCEL),
        ("formation_maximum_width", RVA_MAX_WIDTH, width_detour as *const () as usize, &ORIG_WIDTH),
        ("formation_charge_query", RVA_CHARGE_QUERY, se_bf_cq_entry as *const () as usize, &SE_BF_CQ_ORIG),
        ("formation_active_formation", RVA_ACTIVE_FORMATION, se_bf_af_entry as *const () as usize, &SE_BF_AF_ORIG),
        ("formation_morale_contribution", RVA_MORALE, morale_detour as *const () as usize, &ORIG_MORALE),
        ("formation_charge_update", RVA_CHARGE_UPDATE, charge_update_detour as *const () as usize, &ORIG_CHARGE_UPDATE),
    ];
    let mut made = Vec::new();
    for (name, rva, det, orig) in specs {
        let d = RawDetour::new((b + rva) as *const (), det as *const ()).map_err(|e| format!("{name}: {e}"))?;
        // publish the trampoline before the jump lands
        orig.store(d.trampoline() as *const () as usize, Ordering::SeqCst);
        made.push((name, d));
    }
    SE_BF_CQ_RET.store(b + RET_INTERRUPT_TIMER, Ordering::SeqCst);
    let detours = DETOURS.get_or_init(|| made);
    // apply; on any failure undo what was applied
    let mut enabled: Vec<(usize, &RawDetour)> = Vec::new();
    let mut written: Vec<(&'static str, usize, &'static [u8])> = Vec::new();
    let rollback = |enabled: &[(usize, &RawDetour)], written: &[(&'static str, usize, &'static [u8])]| {
        for (t, d) in enabled {
            let _ = crate::freeze::with_threads_frozen(*t, 16, || d.disable());
        }
        for (name, at, old) in written {
            let _ = crate::engineguard::write_code_for(name, KEY_FORMATION, *at, old);
        }
        for (_, rva, vanilla, _) in FLOAT_PATCHES {
            core::ptr::write_unaligned((b + rva) as *mut f32, vanilla);
        }
    };
    for (i, (name, d)) in detours.iter().enumerate() {
        let t = b + specs[i].1;
        if let Err(e) = enable_raw(name, t, d) {
            rollback(&enabled, &written);
            return Err(format!("enabling {name}: {e}; rolled back"));
        }
        enabled.push((t, d));
    }
    for (name, rva, old, new) in CODE_PATCHES {
        if let Err(e) = crate::engineguard::write_code_for(name, KEY_FORMATION, b + rva, new) {
            rollback(&enabled, &written);
            return Err(format!("patching {name}: {e}; rolled back"));
        }
        written.push((name, b + rva, old));
    }
    for (name, rva, _, new) in FLOAT_PATCHES {
        let at = b + rva;
        let before: [u8; 16] = crate::engineguard::bytes_at(at).unwrap_or([0; 16]);
        core::ptr::write_unaligned(at as *mut f32, new);
        crate::status::record_patch(name, KEY_FORMATION, at, before, crate::engineguard::bytes_at(at).unwrap_or([0; 16]));
    }
    Ok(())
}

// --------------------------------------------------------------------------------------------
// Alex's own DLLs: present?
// --------------------------------------------------------------------------------------------

#[repr(C)]
struct ModuleEntry32W {
    dw_size: u32,
    th32_module_id: u32,
    th32_process_id: u32,
    glblcnt_usage: u32,
    proccnt_usage: u32,
    mod_base_addr: *mut u8,
    mod_base_size: u32,
    h_module: P,
    sz_module: [u16; 256],
    sz_exe_path: [u16; 260],
}

extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> P;
    fn Module32FirstW(snap: P, e: *mut ModuleEntry32W) -> i32;
    fn Module32NextW(snap: P, e: *mut ModuleEntry32W) -> i32;
    fn CloseHandle(h: P) -> i32;
}

/// Lower-case names of loaded modules that look like Alex Zhao's battle-fix DLLs.
fn alex_modules() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x8 | 0x10, 0); // TH32CS_SNAPMODULE | SNAPMODULE32
        if snap.is_null() || snap as usize == usize::MAX { return out; }
        let mut e: ModuleEntry32W = core::mem::zeroed();
        e.dw_size = core::mem::size_of::<ModuleEntry32W>() as u32;
        let mut ok = Module32FirstW(snap, &mut e) != 0;
        while ok {
            let n = e.sz_module.iter().position(|&c| c == 0).unwrap_or(256);
            let name = String::from_utf16_lossy(&e.sz_module[..n]).to_ascii_lowercase();
            if name.starts_with("elephant_gate_fix") || name.starts_with("z_shieldwall_rebuilt") { out.push(name); }
            ok = Module32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

pub fn install(t: &Table) {
    let b = t.base();
    BASE.store(b, Ordering::SeqCst);
    let alex = alex_modules();
    if !alex.is_empty() { log!("battle fixes: Alex Zhao's DLL(s) already loaded: {}", alex.join(", ")); }

    let ele = if !elephant_enabled() {
        format!("off ({KEY_ELEPHANT}=0)")
    } else if let Some(m) = alex.iter().find(|m| m.starts_with("elephant_gate_fix")) {
        format!("inert: {m} is loaded and provides the same fix")
    } else {
        // SAFETY: anchor verified first; written with the other threads frozen.
        match unsafe { install_elephant(b) } {
            Ok(()) => format!("installed at exe+{ELE_SITE_RVA:#x}"),
            Err(e) => format!("inert: {e}"),
        }
    };
    log!("battle fix elephant gate: {ele}");
    let _ = ELE_STATE.set(ele);

    let form = if !formation_enabled() {
        format!("off ({KEY_FORMATION}=0)")
    } else if let Some(m) = alex.iter().find(|m| m.starts_with("z_shieldwall_rebuilt")) {
        format!("inert: {m} is loaded and provides the same fix")
    } else {
        // SAFETY: every pre-check passes before anything is written; partial failures roll back.
        match unsafe { install_formation(b) } {
            Ok(()) => "installed: 6 detours + wall columns 60 + interrupt timer + cantabrian 30/32".to_string(),
            Err(e) => format!("inert: {e}"),
        }
    };
    log!("battle fix formations: {form}");
    let _ = FORM_STATE.set(form);
}

fn info() -> String {
    let st = |s: &OnceLock<String>| s.get().map(|x| x.starts_with("installed") as u8).unwrap_or(0);
    let c = |a: &AtomicU64| a.load(Ordering::Relaxed);
    format!(
        "elephant_gate={};formation_fix={};ele_calls={};ele_orig_true={};ele_passed={};cancel_calls={};cancel_suppressed={};\
         width_calls={};width_set={};cq_gated={};cq_not_charging={};af_turtle_miss={};af_gated={};af_blocked={};\
         morale_flank_neg={};morale_ignored={};cu_charge_state={};cu_halts={}|elephant_state={}|formation_state={}",
        st(&ELE_STATE), st(&FORM_STATE), c(&ELE_CALLS), c(&ELE_ORIG_TRUE), c(&ELE_PASSED), c(&CANCEL_CALLS), c(&CANCEL_SUPPRESSED),
        c(&WIDTH_CALLS), c(&WIDTH_SET), c(&CQ_GATED), c(&CQ_NOT_CHARGING), c(&AF_TURTLE_MISS), c(&AF_GATED), c(&AF_BLOCKED),
        c(&MORALE_FLANK_NEG), c(&MORALE_IGNORED), c(&CU_CHARGE_STATE), c(&CU_HALTS),
        ELE_STATE.get().map(String::as_str).unwrap_or(""), FORM_STATE.get().map(String::as_str).unwrap_or(""),
    )
}

/// se_battlefix_info() -> "elephant_gate=1;formation_fix=1;...|elephant_state=...|formation_state=..."
unsafe extern "C" fn se_battlefix_info(l: *mut LuaState) -> c_int {
    lua::push_str(l, &info());
    1
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_battlefix_info", se_battlefix_info);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sig_table_covers_patches() {
        for (name, rva, old, _) in CODE_PATCHES {
            let (_, srva, pat) = SIGS.iter().find(|s| s.0 == name).unwrap();
            let p = unhex(pat);
            let off = rva - srva;
            assert_eq!(&p[off..off + old.len()], old, "{name}");
        }
        assert_eq!(&ELE_ANCHOR[12..17], &[0xe8, 0x17, 0xa0, 0x07, 0x00]);
        // the call at the elephant site targets the predicate
        let rel = i32::from_le_bytes(ELE_ANCHOR[13..17].try_into().unwrap()) as i64;
        assert_eq!((ELE_SITE_RVA as i64 + 5 + rel) as usize, ELE_PRED_RVA);
        // the gated return addresses follow the calls inside the missile signatures
        for (sig, ret) in [("missile_a", RET_MISSILE_A), ("missile_b", RET_MISSILE_B)] {
            let (_, srva, pat) = SIGS.iter().find(|s| s.0 == sig).unwrap();
            let p = unhex(pat);
            assert_eq!(p[12], 0xe8);
            let rel = i32::from_le_bytes(p[13..17].try_into().unwrap()) as i64;
            assert_eq!(srva + 17, ret);
            assert_eq!((ret as i64 + rel) as usize, RVA_ACTIVE_FORMATION);
        }
    }
}
