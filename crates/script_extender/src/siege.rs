//! AI assault gate: the flat 50 % refusal becomes an odds-scaled one (notes/siege_assault.md,
//! notes/siege_assault_static.md).
//!
//! The campaign AI asks FUN_141e39f30 (exe+0x1e39f30) "is this assault worth it" for a besieging or
//! approaching force. After the strength-ratio test the gate draws a uniform number r in [0,100) from
//! the campaign-AI RNG and REFUSES when `r <= 50.0` (`comiss xmm0,[50.0]; ja accept`, at
//! exe+0x1e3a269), whatever the odds are. A force that is already sieging is asked again every turn, so
//! it keeps sitting outside a weak, unwalled settlement for several turns (a vanilla behaviour,
//! about 6-7 % of sieges in the soaks even at 14-40x odds).
//!
//! Here the 7-byte `comiss xmm0,[rip+..]` is replaced by a `call` into a stub that compares the
//! SAME roll against a threshold chosen from the odds (att/def read from the gate's frame:
//! floats at [rbp-0x30],[rbp-0x2c]). The roll is still drawn exactly as in vanilla, so the
//! campaign-AI RNG stream does not shift and both multiplayer peers decide identically; only the
//! number the roll is compared with changes.
//!
//! cfg: `siege_odds_gate` (default 1; 0 = vanilla) and `siege_odds_bands`
//! (default `1.5:25,3:10,6:2.5` = odds from 1.5 -> refuse when r <= 25, from 3 -> 10, from 6 -> 2.5;
//! below the first band the vanilla 50). Both change the simulation: both are in the sync tag.
//! The gate is also used for attacks on enemy FORCES (kind 2), so field-battle aggression changes
//! with it. se.diag.siege() reports the counters.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

const COMPARE_RVA: usize = 0x1e3a269;
/// exe bytes at 0x1e3a264: `call FUN_141db1310 ; comiss xmm0,[50.0] ; ja +4 ; mov al,1 ; jmp +2 ; xor al,al`
const SITE_BYTES: [u8; 20] = [0xe8, 0xa7, 0x70, 0xf7, 0xff, 0x0f, 0x2f, 0x05, 0x4c, 0x7f, 0x47, 0x01, 0x77, 0x04, 0xb0, 0x01, 0xeb, 0x02, 0x32, 0xc0];
const SITE_PRE: usize = 5; // offset of the comiss inside SITE_BYTES (site starts at 0x1e3a264)
const VANILLA: f32 = 50.0;
const MAX_BANDS: usize = 8;

static BANDS: OnceLock<Vec<(f32, f32)>> = OnceLock::new(); // (min odds, threshold), ascending
static STUB: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicU64 = AtomicU64::new(0);
static VANILLA_REFUSALS: AtomicU64 = AtomicU64::new(0);
static REFUSALS: AtomicU64 = AtomicU64::new(0);
static ACCEPTED_BY_GATE: AtomicU64 = AtomicU64::new(0);
static BAD_FRAME: AtomicU64 = AtomicU64::new(0);
static BAND_CALLS: [AtomicU64; MAX_BANDS + 1] = [const { AtomicU64::new(0) }; MAX_BANDS + 1];
static BAND_FLIPPED: [AtomicU64; MAX_BANDS + 1] = [const { AtomicU64::new(0) }; MAX_BANDS + 1];
static LAST_ODDS_X100: AtomicU32 = AtomicU32::new(0);

extern "system" {
    fn IsBadReadPtr(p: *const core::ffi::c_void, n: usize) -> i32;
}

/// "1.5:25,3:10,6:2.5" -> sorted [(1.5,25),(3,10),(6,2.5)]; None on any malformed entry.
pub fn parse_bands(s: &str) -> Option<Vec<(f32, f32)>> {
    let mut v = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once(':')?;
        let (odds, thr): (f32, f32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        if !(odds.is_finite() && thr.is_finite()) || odds <= 0.0 || !(0.0..=100.0).contains(&thr) { return None; }
        v.push((odds, thr));
    }
    if v.len() > MAX_BANDS { return None; }
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    Some(v)
}

pub const DEFAULT_BANDS: &str = "1.5:25,3:10,6:2.5";

/// The bands in force (cfg or default); the sync tag feeds on this text.
pub fn bands_text() -> String {
    crate::build::config_value("siege_odds_bands").filter(|s| parse_bands(s).is_some()).unwrap_or_else(|| DEFAULT_BANDS.to_string())
}

/// (band index, threshold) for the odds att/def. Band 0 = below the first band (vanilla 50).
pub fn threshold_for(bands: &[(f32, f32)], odds: f32) -> (usize, f32) {
    let mut out = (0usize, VANILLA);
    for (i, (min, thr)) in bands.iter().enumerate() {
        if odds >= *min { out = (i + 1, *thr); }
    }
    out
}

/// Called from the stub with the gate's frame pointer and the roll just drawn; returns the threshold
/// the roll is compared with (refuse when roll <= threshold).
unsafe extern "C" fn threshold(rbp: usize, roll: f32) -> f32 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let p = rbp.wrapping_sub(0x30);
    if rbp < 0x10000 || IsBadReadPtr(p as *const _, 8) != 0 {
        BAD_FRAME.fetch_add(1, Ordering::Relaxed);
        return VANILLA;
    }
    let att = core::ptr::read_unaligned(p as *const f32);
    let def = core::ptr::read_unaligned((p + 4) as *const f32);
    if !att.is_finite() || !def.is_finite() || att < 0.0 || def < 0.0 {
        BAD_FRAME.fetch_add(1, Ordering::Relaxed);
        return VANILLA;
    }
    let odds = if def <= 0.0 { f32::INFINITY } else { att / def };
    let Some(bands) = BANDS.get() else { return VANILLA };
    let (band, thr) = threshold_for(bands, odds);
    LAST_ODDS_X100.store(if odds.is_finite() { (odds * 100.0).min(4.0e9) as u32 } else { u32::MAX }, Ordering::Relaxed);
    BAND_CALLS[band].fetch_add(1, Ordering::Relaxed);
    let vanilla_refuse = roll <= VANILLA;
    let refuse = roll <= thr;
    if vanilla_refuse { VANILLA_REFUSALS.fetch_add(1, Ordering::Relaxed); }
    if refuse { REFUSALS.fetch_add(1, Ordering::Relaxed); }
    if vanilla_refuse && !refuse {
        ACCEPTED_BY_GATE.fetch_add(1, Ordering::Relaxed);
        BAND_FLIPPED[band].fetch_add(1, Ordering::Relaxed);
    }
    thr
}

/// Stub entered by the `call` that replaces the comiss (rsp = 8 mod 16, rbp = gate frame, xmm0 = roll).
/// Returns with the flags of `comiss roll, threshold` for the original `ja` that follows.
fn build_stub(func: usize, cell: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(160);
    s.extend_from_slice(&[0x50, 0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53]); // push rax rcx rdx r8 r9 r10 r11
    s.extend_from_slice(&[0x48, 0x83, 0xec, 0x40]); // sub rsp,0x40
    s.extend_from_slice(&[0xf3, 0x0f, 0x11, 0x44, 0x24, 0x20]); // movss [rsp+0x20],xmm0
    s.extend_from_slice(&[0xf3, 0x0f, 0x10, 0xc8]); // movss xmm1,xmm0 (2nd arg: the roll)
    s.extend_from_slice(&[0x48, 0x89, 0xe9]); // mov rcx,rbp (1st arg)
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(func as u64).to_le_bytes()); // mov rax,func
    s.extend_from_slice(&[0xff, 0xd0]); // call rax
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(cell as u64).to_le_bytes()); // mov rax,cell
    s.extend_from_slice(&[0xf3, 0x0f, 0x11, 0x00]); // movss [rax],xmm0 (threshold)
    s.extend_from_slice(&[0xf3, 0x0f, 0x10, 0x44, 0x24, 0x20]); // movss xmm0,[rsp+0x20] (roll back)
    s.extend_from_slice(&[0x48, 0x83, 0xc4, 0x40]); // add rsp,0x40
    s.extend_from_slice(&[0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5a, 0x59, 0x58]); // pop r11 r10 r9 r8 rdx rcx rax
    s
}

unsafe fn install_gate(base: usize) -> Result<(), String> {
    use crate::engineguard::{alloc_near, bytes_at, make_executable, write_code_for};
    let site = base + COMPARE_RVA - SITE_PRE;
    if bytes_at::<20>(site) != Some(SITE_BYTES) { return Err(format!("bytes at {site:#x} differ (not 1.7.2.0?)")); }
    let bands = parse_bands(&bands_text()).ok_or("siege_odds_bands is malformed")?;
    let _ = BANDS.set(bands);
    let page = alloc_near(base + COMPARE_RVA).ok_or("no free page within rel32 reach")?;
    // layout: code (fixed length), comiss (7), ret (1), then the f32 cell the comiss reads
    let base_len = build_stub(threshold as *const () as usize, 0).len();
    let cell = page + ((base_len + 8 + 3) & !3);
    let mut code = build_stub(threshold as *const () as usize, cell);
    let disp = (cell as i64 - (page as i64 + base_len as i64 + 7)) as i32;
    code.extend_from_slice(&[0x0f, 0x2f, 0x05]);
    code.extend_from_slice(&disp.to_le_bytes());
    code.push(0xc3);
    core::ptr::copy_nonoverlapping(code.as_ptr(), page as *mut u8, code.len());
    core::ptr::write_unaligned(cell as *mut f32, VANILLA);
    make_executable(page, 0x100);
    let at = base + COMPARE_RVA;
    let rel = page as i64 - (at as i64 + 5);
    if rel < i32::MIN as i64 || rel > i32::MAX as i64 { return Err("stub out of rel32 reach".into()); }
    let mut patch = [0u8; 7];
    patch[0] = 0xe8;
    patch[1..5].copy_from_slice(&(rel as i32).to_le_bytes());
    patch[5] = 0x90;
    patch[6] = 0x90;
    write_code_for("siege_odds_gate", "siege_odds_gate", at, &patch)?;
    STUB.store(page, Ordering::Relaxed);
    Ok(())
}

/// Opt-in (default off): the 0.46.0-siege.1 stub writes its threshold cell inside a page that
/// make_executable turns PAGE_EXECUTE_READ -> access violation on the first call (5 Nov 2026).
pub fn gate_enabled() -> bool {
    crate::build::config_value("siege_odds_gate").as_deref() == Some("1")
}

pub fn install(t: &Table) {
    match install_force_assault(t.base()) {
        Ok(()) => log!("siege force assault anchors ok (cfg siege_force_assault={})", crate::build::hook_enabled("siege_force_assault") as u8),
        Err(e) => log!("siege force assault unavailable: {e}"),
    }
    if !gate_enabled() {
        log!("siege odds gate off (opt-in: siege_odds_gate=1; the stub is not fixed yet): vanilla flat 50 % assault refusal");
        return;
    }
    // SAFETY: bytes verified first; written with the other threads frozen (engineguard::write_code_for).
    match unsafe { install_gate(t.base()) } {
        Ok(()) => log!("siege odds gate installed at exe+{COMPARE_RVA:#x}: bands {}", bands_text()),
        Err(e) => log!("siege odds gate not installed: {e}"),
    }
}

// ------------------------------------------------------------------------------------------------
// Forced assault for AI besiegers (cfg siege_force_assault; notes/siege_assault_static.md Steps 7-11)
// ------------------------------------------------------------------------------------------------
// Script orders (attack_settlement) are refused on AI turns: the binding's turn-state check
// FUN_1419da7a0 needs the turn manager phase [[ctx+0x3b68]+0x8c] == 1 ("orders open"); the AI turn
// keeps it 0. se_siege_assault opens that phase for exactly one attack_settlement call of the
// faction whose turn it is (AI only) and restores the saved value right after (proven in game on
// 1.7.2.0, 5 Nov 2026: Dong Zhuo force 629 assaulted Chang'an). ctx = [[char+0x250]+0x78]
// (FUN_141498690 on char+0x250, as the binding does). MP: the window touches only simulation state
// and runs inside a model callback, but it has not been verified in lockstep: se_api refuses it in MP.

const GATE_RVA: usize = 0x19da80c; // `mov rax,[rbx+0x3b68] ; cmp dword [rax+0x8c],1 ; jne +8` in FUN_1419da7a0
const GATE_BYTES: [u8; 16] = [0x48, 0x8b, 0x83, 0x68, 0x3b, 0x00, 0x00, 0x83, 0xb8, 0x8c, 0x00, 0x00, 0x00, 0x01, 0x75, 0x08];
const CTX_RVA: usize = 0x1498690; // `mov rax,[rcx] ; mov rax,[rax+0x78] ; ret`
const CTX_BYTES: [u8; 8] = [0x48, 0x8b, 0x01, 0x48, 0x8b, 0x40, 0x78, 0xc3];
/// MILITARY_FORCE vtable (slot-0 object address), cqi dword at +8, siege object chain [[force+0x5c0]]
const FORCE_VT_RVA: usize = 0x3460298;

static FA_READY: AtomicUsize = AtomicUsize::new(0); // exe base once the anchors matched
static FA_CALLS: AtomicU64 = AtomicU64::new(0);
static FA_OPENED: AtomicU64 = AtomicU64::new(0);
static FA_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static FA_REFUSED: AtomicU64 = AtomicU64::new(0);

unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const _, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd32(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }

fn install_force_assault(base: usize) -> Result<(), String> {
    use crate::engineguard::bytes_at;
    unsafe {
        if bytes_at::<16>(base + GATE_RVA) != Some(GATE_BYTES) { return Err("turn-phase gate bytes differ".into()); }
        if bytes_at::<8>(base + CTX_RVA) != Some(CTX_BYTES) { return Err("ctx accessor bytes differ".into()); }
    }
    FA_READY.store(base, Ordering::Relaxed);
    Ok(())
}

/// Engine MILITARY_FORCE behind a query/modify military force script object (validated by vtable + cqi).
unsafe fn force_from_arg(l: *mut LuaState, idx: c_int, cqi: u32) -> Result<usize, String> {
    let api = lua::api().ok_or("lua api missing")?;
    let base = FA_READY.load(Ordering::Relaxed);
    if base == 0 { return Err("force assault not available (anchors)".into()); }
    let ty = (api.type_)(l, idx);
    if ty != lua::LUA_TLIGHTUSERDATA && ty != lua::LUA_TUSERDATA { return Err(format!("arg {idx} is not a script object")); }
    let p = (api.touserdata)(l, idx) as usize;
    if p == 0 { return Err("null userdata".into()); }
    let vt = base + FORCE_VT_RVA;
    for c in [p, rq(p), rq(p + 0x18), rq(rq(p) + 0x18), rq(rq(p + 8) + 0x18), rq(rq(rq(p) + 8) + 0x18)] {
        if c != 0 && rq(c) == vt && rd32(c + 8) == cqi { return Ok(c); }
    }
    Err(format!("military force {cqi} not found behind arg {idx}"))
}

/// se_siege_equipment(query_military_force, cqi) -> built, in_progress   (or nil, message)
/// S = [[force+0x5c0]] (the force's siege object at its target): built-items vector count [S+0xd4],
/// in-progress count [S+0xc4]. No siege object (not besieging) -> 0, 0.
unsafe extern "C" fn se_siege_equipment(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    match force_from_arg(l, 1, cqi) {
        Ok(f) => {
            let s = rq(rq(f + 0x5c0));
            let (built, prog) = if s != 0 { (rd32(s + 0xd4), rd32(s + 0xc4)) } else { (0, 0) };
            (api.pushinteger)(l, built.min(64) as isize);
            (api.pushinteger)(l, prog.min(64) as isize);
            2
        }
        Err(e) => {
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe fn fa_fail(l: *mut LuaState, m: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    FA_REFUSED.fetch_add(1, Ordering::Relaxed);
    (api.pushboolean)(l, 0);
    lua::push_str(l, m);
    2
}

/// se_siege_assault(query_character, cqi, modify_character, query_settlement) -> ok, message
/// Calls modify_character:attack_settlement(settlement) with the turn phase opened for that one call.
/// Refused unless it is the general's faction's turn, that faction is AI and the phase is 0.
unsafe extern "C" fn se_siege_assault(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    FA_CALLS.fetch_add(1, Ordering::Relaxed);
    if FA_READY.load(Ordering::Relaxed) == 0 || !crate::build::hook_enabled("siege_force_assault") {
        return fa_fail(l, "siege_force_assault is off or its anchors did not match");
    }
    let cqi = (api.tointeger)(l, 2) as u32;
    let ch = match crate::pool::character_from_arg(l, 1, cqi) {
        Ok(c) => c,
        Err(e) => return fa_fail(l, &e),
    };
    let ctx = rq(rq(ch + 0x250) + 0x78);
    let tm = rq(ctx + 0x3b68);
    let rec = rq(tm + 0x48);
    let fac = rq(rq(ch + 0x270));
    if ctx == 0 || tm == 0 || rec == 0 || fac == 0 || !readable(tm + 0x8c, 4) || !readable(rec + 0xcd0, 1) {
        return fa_fail(l, "turn state not readable");
    }
    if rd32(rec + 8) != rd32(fac + 8) { return fa_fail(l, "not this general's faction's turn"); }
    if core::ptr::read_unaligned((rec + 0xcd0) as *const u8) != 0 { return fa_fail(l, "human faction"); }
    let phase_p = (tm + 0x8c) as *mut u32;
    let old = core::ptr::read_unaligned(phase_p);
    if old != 0 { return fa_fail(l, &format!("turn phase is {old}, not 0 (AI turn)")); }
    let top = (api.gettop)(l);
    (api.getfield)(l, 3, c"attack_settlement".as_ptr());
    (api.pushvalue)(l, 3);
    (api.pushvalue)(l, 4);
    core::ptr::write_unaligned(phase_p, 1);
    FA_OPENED.fetch_add(1, Ordering::Relaxed);
    let rc = (api.pcall)(l, 2, 1, 0);
    core::ptr::write_unaligned(phase_p, old);
    if rc != 0 {
        let m = lua::to_str(l, -1);
        (api.settop)(l, top);
        return fa_fail(l, &format!("attack_settlement error: {m}"));
    }
    let ok = (api.toboolean)(l, -1) != 0;
    (api.settop)(l, top);
    if ok { FA_ACCEPTED.fetch_add(1, Ordering::Relaxed); } else { FA_REFUSED.fetch_add(1, Ordering::Relaxed); }
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, if ok { "assault ordered" } else { "attack_settlement returned false" });
    2
}

fn info() -> String {
    let mut s = format!(
        "siege_gate={};calls={};vanilla_refusals={};refusals={};turned_to_accept={};bad_frame={};last_odds_x100={}",
        (STUB.load(Ordering::Relaxed) != 0) as u8, CALLS.load(Ordering::Relaxed), VANILLA_REFUSALS.load(Ordering::Relaxed),
        REFUSALS.load(Ordering::Relaxed), ACCEPTED_BY_GATE.load(Ordering::Relaxed), BAD_FRAME.load(Ordering::Relaxed),
        LAST_ODDS_X100.load(Ordering::Relaxed),
    );
    s.push_str(&format!(";force_assault={};fa_calls={};fa_opened={};fa_accepted={};fa_refused={}",
        (FA_READY.load(Ordering::Relaxed) != 0 && crate::build::hook_enabled("siege_force_assault")) as u8,
        FA_CALLS.load(Ordering::Relaxed), FA_OPENED.load(Ordering::Relaxed), FA_ACCEPTED.load(Ordering::Relaxed), FA_REFUSED.load(Ordering::Relaxed)));
    for i in 0..=MAX_BANDS {
        let c = BAND_CALLS[i].load(Ordering::Relaxed);
        if c != 0 { s.push_str(&format!(";band{i}_calls={c};band{i}_turned={}", BAND_FLIPPED[i].load(Ordering::Relaxed))); }
    }
    s
}

/// se_siege_info() -> "siege_gate=1;calls=N;..." (read-only counters)
unsafe extern "C" fn se_siege_info(l: *mut LuaState) -> c_int {
    lua::push_str(l, &info());
    1
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_siege_info", se_siege_info);
    lua::set_global_fn(l, "se_siege_assault", se_siege_assault);
    lua::set_global_fn(l, "se_siege_equipment", se_siege_equipment);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bands() {
        let b = parse_bands(DEFAULT_BANDS).unwrap();
        assert_eq!(threshold_for(&b, 1.0), (0, 50.0));
        assert_eq!(threshold_for(&b, 1.5), (1, 25.0));
        assert_eq!(threshold_for(&b, 4.0), (2, 10.0));
        assert_eq!(threshold_for(&b, 40.0), (3, 2.5));
        assert_eq!(threshold_for(&b, f32::INFINITY), (3, 2.5));
        assert!(parse_bands("1.5:x").is_none());
        assert!(parse_bands("0:10").is_none());
        assert!(parse_bands("2:101").is_none());
        assert_eq!(parse_bands("").unwrap().len(), 0);
    }
}

#[cfg(test)]
mod stub_dump {
    #[test]
    fn dump() {
        let mut c = super::build_stub(0x1122334455667788, 0x99aabbccddeeff00);
        c.extend_from_slice(&[0x0f, 0x2f, 0x05, 0x10, 0, 0, 0, 0xc3]);
        let hex: String = c.iter().map(|b| format!("{b:02x}")).collect();
        println!("STUBHEX {hex}");
    }
}
