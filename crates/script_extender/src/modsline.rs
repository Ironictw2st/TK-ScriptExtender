//! Rescue after the vanilla crash-report mods-line overflow (190E Batch1117 CrashReports README, headline).
//!
//! `UTILITYDLL::CRASH_REPORT::set_mods` (exe+0x3a7320) strcats "mods: a.pack, b.pack, ..." into the
//! 0x400-byte static `s_mods` (exe+0x3c5e4a0) with no bound. Past it, in the same .bss run:
//! - exe+0x3c5ebc0..0x3c5ebdf: live engine flags and ints;
//! - exe+0x3c5ebe0: the root lua_State pointer. FUN_140362490 creates it only while it is 0, so a mods line of
//!   1856 chars or more leaves text there. The first script environment (~16 s after launch) then calls
//!   lua_newthread on it and faults at exe+0x75d0d6 (proven in two player dumps: L = 1858 and 1920).
//!
//! The SE is injected after set_mods has normally run, so this runs first thing in bootstrap:
//! 1. When s_mods holds an overflowed line (starts "mods: ", no NUL in its 0x400 bytes) and the root slot holds a
//!    value that cannot be a lua_State (text, a misaligned or non-canonical value, unmapped memory), the slot is
//!    set back to 0 (compare-exchange). The engine then creates the root state itself, exactly as on a short list.
//! 2. s_mods is cut at 0x3ff, so the crash reporter's own copy of the line stays inside its buffer.
//! 3. set_mods is detoured (original first, then the same repair), in case the game calls it again later
//!    or the SE was injected before it ran.
//!
//! Not repairable: the flags at 0x3c5ebc0..0x3c5ebdf and past 0x3c5ebe8 that the text also covered (their values
//! are lost). TK Mod Manager warns from 1824 chars for that reason; this only stops the certain crash.
//!
//! Both data addresses are read from the instructions that use them (set_mods+0x19 and FUN_140362490+0xf) and must
//! equal the known RVAs, so another build or layout is refused, never written. cfg `guard_mods_line` (default 1),
//! also off with engine_crash_guards=0. No simulation change: not in the sync tag.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use retour::GenericDetour;
use std::sync::{Mutex, OnceLock};

type P = *mut c_void;
type SetMods = unsafe extern "C" fn(P);

const SET_MODS_RVA: usize = 0x3a7320;
/// push rdi; sub rsp,0x20; cmp dword [rcx+4],0; mov rdi,rcx; je +0xb8; mov eax,[rip+..]; mov [rip+disp32],eax
const SET_MODS_ANCHOR: [u8; 27] = [
    0x40, 0x57, 0x48, 0x83, 0xec, 0x20, 0x83, 0x79, 0x04, 0x00, 0x48, 0x8b, 0xf9, 0x0f, 0x84, 0xb8, 0x00, 0x00, 0x00,
    0x8b, 0x05, 0x3f, 0x6f, 0xec, 0x02, 0x89, 0x05,
];
/// `mov [rip+disp32], eax` writing "mods" to s_mods: opcode at +0x19, disp32 at +0x1b, next instruction at +0x1f
const SET_MODS_STORE_DISP: usize = 0x1b;
const SET_MODS_STORE_NEXT: usize = 0x1f;

const ROOT_GETTER_RVA: usize = 0x362490;
/// mov [rsp+8],rbx; mov [rsp+0x10],rsi; push rdi; sub rsp,0x40; mov rdi,[rip+disp32]
const ROOT_GETTER_ANCHOR: [u8; 18] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x57, 0x48, 0x83, 0xec, 0x40, 0x48, 0x8b, 0x3d,
];
const ROOT_LOAD_DISP: usize = 0x12;
const ROOT_LOAD_NEXT: usize = 0x16;

const S_MODS_RVA: usize = 0x3c5e4a0;
const ROOT_SLOT_RVA: usize = 0x3c5ebe0;
const S_MODS_SIZE: usize = 0x400;
/// how far a line is measured for the log (the text may be cut short by engine writes into it)
const MEASURE_MAX: usize = 0x1000;

static SET_MODS: OnceLock<GenericDetour<SetMods>> = OnceLock::new();
static S_MODS: AtomicUsize = AtomicUsize::new(0);
static ROOT_SLOT: AtomicUsize = AtomicUsize::new(0);
pub static REPAIRS: AtomicU64 = AtomicU64::new(0);
static LAST: Mutex<String> = Mutex::new(String::new());

extern "system" {
    fn VirtualQuery(addr: *const c_void, info: *mut Mbi, len: usize) -> usize;
}

#[repr(C)]
struct Mbi {
    base_address: usize,
    allocation_base: usize,
    allocation_protect: u32,
    partition_id: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

const MEM_COMMIT: u32 = 0x1000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_GUARD: u32 = 0x100;

/// The shape a heap lua_State pointer always has: 8-aligned, above the null page, canonical user space.
pub(crate) fn pointer_shaped(v: usize) -> bool {
    v % 8 == 0 && v >= 0x10000 && v < 0x0000_7fff_ffff_0000
}

/// The value reads as the tail of an ASCII text run: printable low bytes, then only zero bytes (strcat's NUL and
/// the untouched rest of the slot). A real heap pointer's top bytes are 0 too, but its low bytes almost never are
/// all printable; this is only used together with `prev_printable` and a proven overflow.
pub(crate) fn text_shaped(v: usize) -> bool {
    let b = v.to_le_bytes();
    let n = b.iter().position(|&x| x == 0).unwrap_or(8);
    n > 0 && b[..n].iter().all(|&x| (0x20..0x7f).contains(&x)) && b[n..].iter().all(|&x| x == 0)
}

unsafe fn committed_readable(v: usize) -> bool {
    let mut m: Mbi = core::mem::zeroed();
    if VirtualQuery(v as *const c_void, &mut m, core::mem::size_of::<Mbi>()) == 0 { return false; }
    m.state == MEM_COMMIT && m.protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 && m.protect != 0
}

/// rip-relative target of the disp32 at `at`, the instruction ending at `next`
unsafe fn rip_target(at: usize, next: usize) -> usize {
    let d = core::ptr::read_unaligned(at as *const i32);
    (next as isize + d as isize) as usize
}

/// Both data addresses, re-derived from the code that uses them; None (logged) on any mismatch.
unsafe fn locate(base: usize) -> Result<(usize, usize), String> {
    let sm = base + SET_MODS_RVA;
    if crate::engineguard::bytes_at::<27>(sm) != Some(SET_MODS_ANCHOR) { return Err(format!("set_mods bytes at {sm:#x} differ")); }
    let rg = base + ROOT_GETTER_RVA;
    if crate::engineguard::bytes_at::<18>(rg) != Some(ROOT_GETTER_ANCHOR) { return Err(format!("root getter bytes at {rg:#x} differ")); }
    let s_mods = rip_target(sm + SET_MODS_STORE_DISP, sm + SET_MODS_STORE_NEXT);
    let slot = rip_target(rg + ROOT_LOAD_DISP, rg + ROOT_LOAD_NEXT);
    if s_mods != base + S_MODS_RVA || slot != base + ROOT_SLOT_RVA {
        return Err(format!("data layout differs (s_mods {:#x}, root slot {:#x})", s_mods.wrapping_sub(base), slot.wrapping_sub(base)));
    }
    Ok((s_mods, slot))
}

/// The repair itself. Reads only the exe's own .bss; writes at most the root slot (0) and s_mods[0x3ff] (NUL).
unsafe fn repair(why: &str) {
    let (s, slot) = (S_MODS.load(Ordering::Relaxed), ROOT_SLOT.load(Ordering::Relaxed));
    if s == 0 || slot == 0 { return; }
    // a copy: the engine thread may be writing the buffer right now
    let buf: [u8; S_MODS_SIZE] = core::ptr::read_volatile(s as *const [u8; S_MODS_SIZE]);
    if &buf[..6] != b"mods: " || buf.contains(&0) {
        // not set yet, or the line fits its buffer: nothing overflowed
        return;
    }
    let len = (0..MEASURE_MAX).find(|&i| core::ptr::read_volatile((s + i) as *const u8) == 0).unwrap_or(MEASURE_MAX);
    let cell = &*(slot as *const AtomicUsize);
    let v = cell.load(Ordering::Acquire);
    let prev_printable = (0x20..0x7f).contains(&core::ptr::read_volatile((slot - 1) as *const u8));
    let garbage = v != 0 && (!pointer_shaped(v) || !committed_readable(v) || (text_shaped(v) && prev_printable));
    let mut msg = format!("mods line overflow ({why}): {len}+ chars, buffer {S_MODS_SIZE}; root lua slot {v:#x}");
    if garbage {
        if cell.compare_exchange(v, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            REPAIRS.fetch_add(1, Ordering::Relaxed);
            msg.push_str(" was text, reset to 0 (the game creates the root state itself)");
        } else {
            msg.push_str(" changed while checked; left alone");
        }
    } else if v == 0 {
        msg.push_str(" is 0 (line ended before it, or the NUL landed on it); nothing to repair");
    } else {
        msg.push_str(" looks like a real state; left alone");
    }
    // the crash reporter's copy stays inside its own buffer from here on
    core::ptr::write_volatile((s + S_MODS_SIZE - 1) as *mut u8, 0);
    msg.push_str(". Engine flags after the buffer may still be wrong: fewer mod packs is the real fix.");
    log!("{msg}");
    if let Ok(mut l) = LAST.lock() { *l = msg; }
}

unsafe extern "C" fn set_mods_detour(mods: P) {
    let Some(h) = SET_MODS.get() else { return };
    h.call(mods);
    repair("set_mods called again");
}

/// First thing in bootstrap, before the slow installs: the root state is created ~16 s after launch.
pub fn install(t: &Table) {
    if !crate::build::hook_enabled("engine_crash_guards") || !crate::build::hook_enabled("guard_mods_line") {
        log!("mods-line guard off (guard_mods_line=0 or engine_crash_guards=0)");
        return;
    }
    let base = t.base();
    // SAFETY: locate() verifies the code bytes and both derived data addresses before anything is read or written.
    unsafe {
        match locate(base) {
            Ok((s, slot)) => {
                S_MODS.store(s, Ordering::Relaxed);
                ROOT_SLOT.store(slot, Ordering::Relaxed);
            }
            Err(e) => {
                log!("mods-line guard not installed: {e}");
                return;
            }
        }
        repair("at injection");
        let target = base + SET_MODS_RVA;
        let f: SetMods = core::mem::transmute(target);
        match GenericDetour::new(f, set_mods_detour) {
            Ok(d) => {
                let _ = SET_MODS.set(d);
                if let Some(d) = SET_MODS.get() {
                    if let Err(e) = crate::freeze::enable_detour("guard_mods_line", "guard_mods_line", target, d) {
                        log!("mods-line guard: set_mods detour not enabled: {e} (the injection-time repair still ran)");
                    }
                }
            }
            Err(e) => log!("mods-line guard: set_mods detour not built: {e} (the injection-time repair still ran)"),
        }
    }
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_mods_line_info", se_mods_line_info);
}

/// se_mods_line_info() -> "installed=1;repairs=0;last=<log line or empty>"
unsafe extern "C" fn se_mods_line_info(l: *mut LuaState) -> c_int {
    let last = LAST.lock().map(|s| s.clone()).unwrap_or_default();
    let s = format!(
        "installed={};repairs={};last={}",
        (S_MODS.load(Ordering::Relaxed) != 0) as u8,
        REPAIRS.load(Ordering::Relaxed),
        last
    );
    lua::push_str(l, &s);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_from_the_two_player_dumps_are_not_pointers() {
        // L = 1858: "ck" + NUL; L = 1920: "leration"
        assert!(!pointer_shaped(0x6b63));
        assert!(!pointer_shaped(0x6e6f_6974_6172_656c));
        assert!(text_shaped(0x6b63));
        assert!(text_shaped(0x6e6f_6974_6172_656c));
    }

    #[test]
    fn heap_pointers_pass() {
        for v in [0x0000_01d2_3a4b_5c60usize, 0x0000_0230_0000_1000, 0x0000_7ff6_1234_0008] {
            assert!(pointer_shaped(v), "{v:#x}");
            assert!(!text_shaped(v), "{v:#x}");
        }
    }

    #[test]
    fn text_shape_needs_text_then_zeros() {
        assert!(!text_shaped(0));
        assert!(!text_shaped(0x0041_0042)); // a zero byte between text bytes
        assert!(text_shaped(0x2e6b_6361)); // "ack."
    }

    #[test]
    fn data_layout_matches_the_binary() {
        // the root slot sits 0x740 past s_mods: the 1856-char bound in the README and in TK Mod Manager
        assert_eq!(ROOT_SLOT_RVA - S_MODS_RVA, 0x740);
        // set_mods+0x1f + 0x038b7161 and FUN_140362490+0x16 + 0x038fc73a (disp32 read from 1.7.2.0)
        assert_eq!(SET_MODS_RVA + SET_MODS_STORE_NEXT + 0x038b_7161, S_MODS_RVA);
        assert_eq!(ROOT_GETTER_RVA + ROOT_LOAD_NEXT + 0x038f_c73a, ROOT_SLOT_RVA);
    }
}
