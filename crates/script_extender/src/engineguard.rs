//! Null guards for engine crashes seen in #mod-manager-reports (notes/ENGINE_CRASH_GUARDS.md).
//!
//! Each guard turns one unchecked NULL read into the path the engine itself takes when the same
//! value is missing elsewhere in that function. Each guard checks its own bytes before writing
//! anything: a mismatch skips only that guard (logged), never the whole address table.
//!
//! 1. `CcoDiplomacyEvent.InactiveString` FUN_142f67d30(cco, setter): ev = *(cco+0x18); when
//!    *(ev+0x2c) == 0 it reads the inactive factor-group record *(ev+0x20) with no null check
//!    (crash exe+0x2f67df5 reading 0x18). The other two record picks in the same function return
//!    when the record is NULL; the detour does the same (no string set).
//! 2. FUN_1418b4050(owner+0x10, ceo_key) "does this CEO owner hold the CEO": called through a CCO
//!    member pointer on a NULL owner, `this` arrives as 0x10 (crash exe+0x18b4060 reading 0x18,
//!    "Art of War" hover). 18 callers, UI and model. The detour answers false ("not held") for a
//!    `this` below 0x10000 or a NULL container at this+8; vanilla faults there.
//! 3. Text layout FUN_140641580: looks up the text's font (FUN_14064eb90) and runs a per-glyph loop
//!    on the font object with no null check (crash exe+0x641657, `mov r10,[rbx]`, rbx = 0); the
//!    rest of the function null-checks the same font and skips. In-place patch of the 11 bytes at
//!    exe+0x641645: `mov rbx,[rax+8]; test rbx,rbx; jz +0x670; jmp +0x66b` (the loop's own bottom
//!    test), so with a font the code runs exactly as before.
//! 4. `CcoCampaignBuildingSlot.ShowInBuildingBrowser` (0x143050230): appends the result of
//!    resolve FUN_143147350(region, level) to a list with no null check (the second resolve call
//!    in the same function checks), and the sort comparator FUN_140ce4a90 then reads NULL+0x80
//!    (crash exe+0xce4a90). The call at exe+0x30505ca is redirected to a stub near the exe that
//!    calls resolve and, on NULL, continues at exe+0x3050666 (the "already in the list" path).
//!
//! 5-8: weak_lock, clear_selection, phase_record, payload_walk (sections below).
//! 9. move_faction (0.48.1): move_to_faction_and_make_recruited with an unknown faction key on a
//!    recruitment-pool character (crash exe+0x17c60d0) becomes a no-op. Own switch guard_move_faction.
//! 10. cco_list_walk (0.48.1): the UI Cco list getter FUN_142f99bf0 stops at a NULL / implausible
//!    list node (crash exe+0x1c254d4). Own switch guard_cco_list_walk.
//!
//! cfg `engine_crash_guards` (default 1; 0 = none installed). Guard 2 is reachable from model code;
//! it only changes what happens where vanilla would crash, but the key is in the sync tag anyway
//! so two machines in one lobby always run the same guards.

use crate::addrs::Table;
use crate::lua::{self, LuaState};
use crate::log;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use retour::GenericDetour;
use std::sync::OnceLock;

type P = *mut c_void;
type InactiveString = unsafe extern "C" fn(P, P);
type CeoOwnerHas = unsafe extern "C" fn(P, P) -> usize;

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualQuery(addr: *const c_void, info: *mut MemoryBasicInformation, len: usize) -> usize;
    fn FlushInstructionCache(process: *mut c_void, addr: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}

#[repr(C)]
struct MemoryBasicInformation {
    base_address: usize,
    allocation_base: usize,
    allocation_protect: u32,
    partition_id: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

const PAGE_EXECUTE_READ: u32 = 0x20;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_FREE: u32 = 0x10000;

const INACTIVE_STRING_RVA: usize = 0x2f67d30;
const INACTIVE_STRING_ANCHOR: [u8; 16] = [0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xda, 0x48, 0x8b, 0xf9];
const CEO_OWNER_HAS_RVA: usize = 0x18b4050;
const CEO_OWNER_HAS_ANCHOR: [u8; 16] = [0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xd9, 0x48, 0x8b, 0xfa];

const FONT_SITE_RVA: usize = 0x641645;
/// mov rbx,[rax+8]; cmp edi,[rbp+4]; jnc +0x22 (-> 0x670); nop
const FONT_SITE_BEFORE: [u8; 11] = [0x48, 0x8b, 0x58, 0x08, 0x3b, 0x7d, 0x04, 0x73, 0x22, 0x66, 0x90];
/// mov rbx,[rax+8]; test rbx,rbx; jz +0x22 (-> 0x670); jmp +0x1b (-> 0x66b)
const FONT_SITE_AFTER: [u8; 11] = [0x48, 0x8b, 0x58, 0x08, 0x48, 0x85, 0xdb, 0x74, 0x22, 0xeb, 0x1b];
/// the loop's bottom test at 0x66b (cmp edi,[rbp+4]; jc 0x650) and the code after it at 0x670
const FONT_LOOP_BOTTOM_RVA: usize = 0x64166b;
const FONT_LOOP_BOTTOM: [u8; 9] = [0x3b, 0x7d, 0x04, 0x72, 0xe0, 0x49, 0x8b, 0x4d, 0x10];

const BROWSER_CALL_RVA: usize = 0x30505ca;
/// call resolve; mov r15d,esi; mov r14,rax
const BROWSER_CALL_BYTES: [u8; 11] = [0xe8, 0x81, 0x6d, 0x0f, 0x00, 0x44, 0x8b, 0xfe, 0x4c, 0x8b, 0xf0];
const BROWSER_RESOLVE_RVA: usize = 0x3147350;
const BROWSER_CONTINUE_RVA: usize = 0x3050666;
/// mov r14,[rbp-0x39]; mov r15,[rbp-0x41]
const BROWSER_CONTINUE_BYTES: [u8; 8] = [0x4c, 0x8b, 0x75, 0xc7, 0x4c, 0x8b, 0x7d, 0xbf];

static INACTIVE: OnceLock<GenericDetour<InactiveString>> = OnceLock::new();
static CEO_OWNER: OnceLock<GenericDetour<CeoOwnerHas>> = OnceLock::new();
static BASE: AtomicUsize = AtomicUsize::new(0);
static FONT_PATCHED: AtomicBool = AtomicBool::new(false);
static BROWSER_STUB: AtomicUsize = AtomicUsize::new(0);
pub static INACTIVE_HITS: AtomicU64 = AtomicU64::new(0);
pub static CEO_OWNER_HITS: AtomicU64 = AtomicU64::new(0);
/// incremented by the building-browser stub itself (`lock inc qword [rcx]`)
pub static BROWSER_HITS: AtomicU64 = AtomicU64::new(0);
static LOGGED_INACTIVE: AtomicU32 = AtomicU32::new(0);
static LOGGED_CEO: AtomicU32 = AtomicU32::new(0);

pub(crate) unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
pub(crate) unsafe fn bytes_at<const N: usize>(p: usize) -> Option<[u8; N]> {
    if readable(p, N) { Some(core::ptr::read_unaligned(p as *const [u8; N])) } else { None }
}

/// The InactiveString read that has no null check: the "no value-dependent strings" branch
/// (*(ev+0x2c) == 0) with no inactive record at *(ev+0x20). Mirrors the function's own entry
/// checks (ev and *(ev+0x18) non-null) so every other case reaches the original unchanged.
unsafe fn inactive_would_fault(cco: usize) -> bool {
    let ev = core::ptr::read_unaligned((cco + 0x18) as *const usize);
    if ev == 0 || core::ptr::read_unaligned((ev + 0x18) as *const usize) == 0 { return false; }
    core::ptr::read_unaligned((ev + 0x2c) as *const u8) == 0 && core::ptr::read_unaligned((ev + 0x20) as *const usize) == 0
}

unsafe extern "C" fn inactive_string_detour(cco: P, setter: P) {
    let Some(h) = INACTIVE.get() else { return };
    if !cco.is_null() && inactive_would_fault(cco as usize) {
        let n = INACTIVE_HITS.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::chatty(&LOGGED_INACTIVE, 5) {
            log!("engine guard: CcoDiplomacyEvent.InactiveString on an event with no inactive factor group (#{n}); no string set");
        }
        return;
    }
    h.call(cco, setter)
}

/// `this` is owner+0x10 and *(this+8) the owner's CEO container; vanilla reads both at once.
unsafe fn ceo_owner_unusable(this: usize) -> bool {
    this < 0x10000 || core::ptr::read_unaligned((this + 8) as *const usize) == 0
}

unsafe extern "C" fn ceo_owner_has_detour(this: P, key: P) -> usize {
    let Some(h) = CEO_OWNER.get() else { return 0 };
    if ceo_owner_unusable(this as usize) {
        let n = CEO_OWNER_HITS.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::chatty(&LOGGED_CEO, 5) {
            log!("engine guard: CEO lookup on a missing owner (this {:#x}, #{n}); answered 'not held'", this as usize);
        }
        return 0;
    }
    h.call(this, key)
}

/// The building-browser stub, entered by the `call` at exe+0x30505ca (rsp = 8 mod 16):
/// ```text
///   sub rsp,0x28 ; mov rax,resolve ; call rax ; add rsp,0x28
///   test rax,rax ; jz null ; ret
/// null:
///   mov rcx,&BROWSER_HITS ; lock inc qword [rcx]
///   add rsp,8 ; mov rax,continue ; jmp rax
/// ```
/// On NULL the return address is dropped and execution continues where the function goes for a
/// level that is already in the list (it reloads r14/r15 from its frame there). Only rax, rcx and
/// the flags differ from the original, all dead at both continuation points.
fn browser_stub(resolve: usize, cont: usize, counter: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(64);
    s.extend_from_slice(&[0x48, 0x83, 0xec, 0x28]);
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(resolve as u64).to_le_bytes());
    s.extend_from_slice(&[0xff, 0xd0]);
    s.extend_from_slice(&[0x48, 0x83, 0xc4, 0x28]);
    s.extend_from_slice(&[0x48, 0x85, 0xc0]);
    s.extend_from_slice(&[0x74, 0x01]);
    s.push(0xc3);
    s.extend_from_slice(&[0x48, 0xb9]);
    s.extend_from_slice(&(counter as u64).to_le_bytes());
    s.extend_from_slice(&[0xf0, 0x48, 0xff, 0x01]);
    s.extend_from_slice(&[0x48, 0x83, 0xc4, 0x08]);
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(cont as u64).to_le_bytes());
    s.extend_from_slice(&[0xff, 0xe0]);
    s
}

/// One executable page within rel32 reach of `near` (free regions below the exe first, then above).
pub(crate) unsafe fn alloc_near(near: usize) -> Option<usize> {
    const GRAN: usize = 0x10000;
    const REACH: usize = 0x7000_0000;
    let try_at = |a: usize| -> Option<usize> {
        let mut mbi: MemoryBasicInformation = core::mem::zeroed();
        if VirtualQuery(a as *const c_void, &mut mbi, core::mem::size_of::<MemoryBasicInformation>()) == 0 { return None; }
        if mbi.state != MEM_FREE || mbi.region_size < 0x1000 { return None; }
        let p = VirtualAlloc(a as *mut c_void, 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
        if p.is_null() { None } else { Some(p as usize) }
    };
    let start = near & !(GRAN - 1);
    let mut a = start;
    while start - a < REACH && a > GRAN {
        a -= GRAN;
        if let Some(p) = try_at(a) { return Some(p); }
    }
    let mut a = start;
    while a - start < REACH {
        a += GRAN;
        if let Some(p) = try_at(a) { return Some(p); }
    }
    None
}

/// Write `new` over `addr` with the other threads frozen and out of the range.
unsafe fn write_code(name: &'static str, addr: usize, new: &[u8]) -> Result<(), String> {
    write_code_for(name, "engine_crash_guards", addr, new)
}

/// Make a freshly written stub page read+execute.
pub(crate) unsafe fn make_executable(stub: usize, len: usize) {
    let mut old = 0u32;
    VirtualProtect(stub as *mut c_void, 0x1000, PAGE_EXECUTE_READ, &mut old);
    FlushInstructionCache(GetCurrentProcess(), stub as *const c_void, len);
}

/// `write_code` for another module's cfg key.
pub(crate) unsafe fn write_code_for(name: &'static str, cfg_key: &'static str, addr: usize, new: &[u8]) -> Result<(), String> {
    let before: [u8; 16] = bytes_at(addr).unwrap_or([0; 16]);
    crate::freeze::with_threads_frozen(addr, new.len(), || {
        let mut old = 0u32;
        if VirtualProtect(addr as *mut c_void, new.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return Err("VirtualProtect failed".to_string());
        }
        core::ptr::copy_nonoverlapping(new.as_ptr(), addr as *mut u8, new.len());
        let mut tmp = 0u32;
        VirtualProtect(addr as *mut c_void, new.len(), old, &mut tmp);
        FlushInstructionCache(GetCurrentProcess(), addr as *const c_void, new.len());
        Ok(())
    })??;
    let after: [u8; 16] = bytes_at(addr).unwrap_or([0; 16]);
    crate::status::record_patch(name, cfg_key, addr, before, after);
    crate::crash::hook_installed(name, cfg_key, addr);
    Ok(())
}

unsafe fn install_inactive_string(base: usize) -> Result<(), String> {
    let target = base + INACTIVE_STRING_RVA;
    if bytes_at::<16>(target) != Some(INACTIVE_STRING_ANCHOR) { return Err(format!("bytes at {target:#x} differ")); }
    let f: InactiveString = core::mem::transmute(target);
    let d = GenericDetour::new(f, inactive_string_detour).map_err(|e| e.to_string())?;
    let _ = INACTIVE.set(d);
    let d = INACTIVE.get().ok_or("detour not stored")?;
    crate::freeze::enable_detour("guard_inactive_string", "engine_crash_guards", target, d)
}

unsafe fn install_ceo_owner(base: usize) -> Result<(), String> {
    let target = base + CEO_OWNER_HAS_RVA;
    if bytes_at::<16>(target) != Some(CEO_OWNER_HAS_ANCHOR) { return Err(format!("bytes at {target:#x} differ")); }
    let f: CeoOwnerHas = core::mem::transmute(target);
    let d = GenericDetour::new(f, ceo_owner_has_detour).map_err(|e| e.to_string())?;
    let _ = CEO_OWNER.set(d);
    let d = CEO_OWNER.get().ok_or("detour not stored")?;
    crate::freeze::enable_detour("guard_ceo_owner", "engine_crash_guards", target, d)
}

unsafe fn install_font(base: usize) -> Result<(), String> {
    let site = base + FONT_SITE_RVA;
    if bytes_at::<9>(base + FONT_LOOP_BOTTOM_RVA) != Some(FONT_LOOP_BOTTOM) { return Err("loop bottom bytes differ".into()); }
    match bytes_at::<11>(site) {
        Some(b) if b == FONT_SITE_BEFORE => {}
        Some(b) if b == FONT_SITE_AFTER => return Err("already patched (second injection?)".into()),
        _ => return Err(format!("bytes at {site:#x} differ")),
    }
    write_code("guard_text_font", site, &FONT_SITE_AFTER)?;
    FONT_PATCHED.store(true, Ordering::Relaxed);
    Ok(())
}

unsafe fn install_browser(base: usize) -> Result<(), String> {
    let site = base + BROWSER_CALL_RVA;
    if bytes_at::<11>(site) != Some(BROWSER_CALL_BYTES) { return Err(format!("bytes at {site:#x} differ")); }
    if bytes_at::<8>(base + BROWSER_CONTINUE_RVA) != Some(BROWSER_CONTINUE_BYTES) { return Err("continuation bytes differ".into()); }
    let resolve = base + BROWSER_RESOLVE_RVA;
    // the call's own target must be resolve (the bytes above already pin the rel32)
    let rel = core::ptr::read_unaligned((site + 1) as *const i32);
    if (site as isize + 5 + rel as isize) as usize != resolve { return Err("call target is not resolve".into()); }
    let stub = alloc_near(site).ok_or("no free page within rel32 reach")?;
    let code = browser_stub(resolve, base + BROWSER_CONTINUE_RVA, &BROWSER_HITS as *const AtomicU64 as usize);
    core::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
    let mut old = 0u32;
    VirtualProtect(stub as *mut c_void, 0x1000, PAGE_EXECUTE_READ, &mut old);
    FlushInstructionCache(GetCurrentProcess(), stub as *const c_void, code.len());
    let disp = stub as i64 - (site as i64 + 5);
    if disp < i32::MIN as i64 || disp > i32::MAX as i64 { return Err("stub out of rel32 reach".into()); }
    let mut call = [0u8; 5];
    call[0] = 0xe8;
    call[1..].copy_from_slice(&(disp as i32).to_le_bytes());
    write_code("guard_building_browser", site, &call)?;
    BROWSER_STUB.store(stub, Ordering::Relaxed);
    Ok(())
}


// ---- weak_ptr.lock() used without a null check (battle effect/audio handle updater) ----------
// Crash reports exe+0x26a3e68 and exe+0x26a47ea (Windows "Application Error" events, no dump):
// the code locks a weak_ptr stored at object+0x20/+0x28, and on an expired handle keeps the
// null it got and does `mov rax,[reg]; call [rax+N]`. Skipping the call is what the engine's own
// checked paths do for an expired handle. Sibling sites in the same function share the pattern
// and are guarded too. Notes: notes/battlecrash_1105.md.
struct WeakSite {
    name: &'static str,
    rva: usize,
    bytes: &'static [u8],
    /// `test reg,reg` for the register that holds the locked pointer
    test: [u8; 3],
}
const WEAK_SITES: [WeakSite; 4] = [
    // FUN_1426a3e20: mov rax,[r8]; mov rcx,r8; call [rax+0x30]   (crash 0x26a3e68)
    WeakSite { name: "guard_weak_3e68", rva: 0x26a3e68, bytes: &[0x49, 0x8b, 0x00, 0x49, 0x8b, 0xc8, 0xff, 0x50, 0x30], test: [0x4d, 0x85, 0xc0] },
    // FUN_1426a3ee0 cold part: call [rax+0x78]
    WeakSite { name: "guard_weak_4778", rva: 0x26a4778, bytes: &[0x49, 0x8b, 0x00, 0x48, 0x8d, 0x55, 0x80, 0x49, 0x8b, 0xc8, 0xff, 0x50, 0x78], test: [0x4d, 0x85, 0xc0] },
    // call [rax+0xc0]   (crash 0x26a47ea, the most frequent)
    WeakSite { name: "guard_weak_47ea", rva: 0x26a47ea, bytes: &[0x49, 0x8b, 0x00, 0x48, 0x8d, 0x54, 0x24, 0x30, 0x49, 0x8b, 0xc8, 0xff, 0x90, 0xc0, 0x00, 0x00, 0x00], test: [0x4d, 0x85, 0xc0] },
    // r12 variant: mov rax,[r12]; mov rcx,r12; call [rax+0x28]
    WeakSite { name: "guard_weak_485a", rva: 0x26a485a, bytes: &[0x49, 0x8b, 0x04, 0x24, 0x49, 0x8b, 0xcc, 0xff, 0x50, 0x28], test: [0x4d, 0x85, 0xe4] },
];
/// incremented by the stubs themselves
pub static WEAK_HITS: AtomicU64 = AtomicU64::new(0);

/// ```text
///   test reg,reg ; jz null
///   <the original instructions>      ; rip-independent by construction
///   jmp [rip+0] ; dq cont
/// null:
///   push rax ; mov rax,counter ; lock inc qword [rax] ; pop rax
///   jmp [rip+0] ; dq cont
/// ```
/// Only the flags differ from the original; what the skipped call would have returned (rax, rcx,
/// rdx) is dead at the continuation.
fn weak_stub(test: [u8; 3], orig: &[u8], cont: usize, counter: usize) -> Vec<u8> {
    null_branch_stub(test, orig, cont, cont, counter)
}

/// `weak_stub` with its own continuation for the NULL case:
/// `test reg,reg ; jz null ; <orig> ; jmp cont ; null: <count> ; jmp null_cont`.
fn null_branch_stub(test: [u8; 3], orig: &[u8], cont: usize, null_cont: usize, counter: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(64 + orig.len());
    s.extend_from_slice(&test);
    s.extend_from_slice(&[0x74, 0x00]);
    let jz = s.len() - 1;
    s.extend_from_slice(orig);
    s.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    s.extend_from_slice(&(cont as u64).to_le_bytes());
    let null = s.len();
    s[jz] = (null - (jz + 1)) as u8;
    s.push(0x50);
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(counter as u64).to_le_bytes());
    s.extend_from_slice(&[0xf0, 0x48, 0xff, 0x00, 0x58]);
    s.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    s.extend_from_slice(&(null_cont as u64).to_le_bytes());
    s
}

unsafe fn install_weak_site(base: usize, w: &WeakSite) -> Result<(), String> {
    let site = base + w.rva;
    if !readable(site, w.bytes.len()) { return Err(format!("{site:#x} unreadable")); }
    let cur = core::slice::from_raw_parts(site as *const u8, w.bytes.len());
    if cur != w.bytes { return Err(format!("bytes at {site:#x} differ")); }
    let stub = alloc_near(site).ok_or("no free page within rel32 reach")?;
    let code = weak_stub(w.test, w.bytes, site + w.bytes.len(), &WEAK_HITS as *const AtomicU64 as usize);
    core::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
    let mut old = 0u32;
    VirtualProtect(stub as *mut c_void, 0x1000, PAGE_EXECUTE_READ, &mut old);
    FlushInstructionCache(GetCurrentProcess(), stub as *const c_void, code.len());
    let disp = stub as i64 - (site as i64 + 5);
    if disp < i32::MIN as i64 || disp > i32::MAX as i64 { return Err("stub out of rel32 reach".into()); }
    let mut patch = vec![0x90u8; w.bytes.len()];
    patch[0] = 0xe9;
    patch[1..5].copy_from_slice(&(disp as i32).to_le_bytes());
    write_code(w.name, site, &patch)
}

unsafe fn install_weak(base: usize) -> Result<(), String> {
    let mut n = 0;
    for w in WEAK_SITES.iter() {
        match install_weak_site(base, w) {
            Ok(()) => n += 1,
            Err(e) => log!("engine guard {} not installed: {e}", w.name),
        }
    }
    if n == 0 { Err("no weak_ptr site matched".into()) } else { Ok(()) }
}

// ---- stale court-screen selection list (UI use-after-free) -------------------------------------
// Crash exe+0x55b7e0 (thread 1568832033214697514, "Clicked on court tab"; same site as cluster 14
// "loading into a save"). A court seat's CourtScreenStagedSlot click callback (thunk 0x142f0e9b0,
// context CcoCampaignCourtScreenStagedSlot) calls FUN_14055b770 = ClearSelection(this, notify) on
// the court screen's selection list after that object was freed: the selected-count at +0x10c
// read 0x7ff7 and the array at +0x110 was a non-canonical pointer. Layout of the list (from
// FUN_140593ec0 / FUN_1405b6230): vtable at +0, all items +0xfc count / +0x100 array, selected
// items +0x10c count / +0x110 array. The detour skips the call (nothing selected = nothing to
// clear, the function's own empty path) when the object is not a plausible live list.
// Notes: 190Expanded Beta/Output/Batch1107/CourtCrash/README.md.
type ClearSelection = unsafe extern "C" fn(P, u8);
const CLEAR_SELECTION_RVA: usize = 0x55b770;
const CLEAR_SELECTION_ANCHOR: [u8; 16] = [0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57];
/// Larger than any UI list the engine builds; the crash read 0x7ff7.
const SELECTION_COUNT_CAP: u32 = 0x4000;
static CLEAR_SEL: OnceLock<GenericDetour<ClearSelection>> = OnceLock::new();
static IMAGE: OnceLock<(usize, usize)> = OnceLock::new();
pub static CLEAR_SEL_HITS: AtomicU64 = AtomicU64::new(0);
static LOGGED_CLEAR_SEL: AtomicU32 = AtomicU32::new(0);

/// [base, base + SizeOfImage) of the exe, from its own PE header.
unsafe fn image_range(base: usize) -> Option<(usize, usize)> {
    let lfanew = core::ptr::read_unaligned((base + 0x3c) as *const u32) as usize;
    if !readable(base + lfanew, 0x58) { return None; }
    let size = core::ptr::read_unaligned((base + lfanew + 0x50) as *const u32) as usize;
    Some((base, base + size))
}

/// One (count, array) pair of the list: count within the cap and, when non-zero, the whole
/// array readable.
unsafe fn list_pair_ok(this: usize, count_off: usize, array_off: usize) -> bool {
    let n = core::ptr::read_unaligned((this + count_off) as *const u32);
    if n > SELECTION_COUNT_CAP { return false; }
    if n == 0 { return true; }
    let arr = core::ptr::read_unaligned((this + array_off) as *const usize);
    readable(arr, n as usize * 8)
}

/// True when `this` looks like a live selection list: readable through +0x118, vtable inside the
/// exe image, and both (count, array) pairs sane. A freed and reused block fails at least one.
unsafe fn selection_list_ok(this: usize, image: (usize, usize)) -> bool {
    if !readable(this, 0x118) { return false; }
    let vt = core::ptr::read_unaligned(this as *const usize);
    if vt < image.0 || vt >= image.1 { return false; }
    list_pair_ok(this, 0xfc, 0x100) && list_pair_ok(this, 0x10c, 0x110)
}

unsafe extern "C" fn clear_selection_detour(this: P, notify: u8) {
    let Some(h) = CLEAR_SEL.get() else { return };
    let image = IMAGE.get().copied().unwrap_or((0, usize::MAX));
    if !selection_list_ok(this as usize, image) {
        let n = CLEAR_SEL_HITS.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::chatty(&LOGGED_CLEAR_SEL, 5) {
            log!("engine guard: ClearSelection on a stale UI selection list (this {:#x}, #{n}); skipped", this as usize);
        }
        return;
    }
    h.call(this, notify)
}

unsafe fn install_clear_selection(base: usize) -> Result<(), String> {
    let target = base + CLEAR_SELECTION_RVA;
    if bytes_at::<16>(target) != Some(CLEAR_SELECTION_ANCHOR) { return Err(format!("bytes at {target:#x} differ")); }
    let image = image_range(base).ok_or("PE header unreadable")?;
    let _ = IMAGE.set(image);
    let f: ClearSelection = core::mem::transmute(target);
    let d = GenericDetour::new(f, clear_selection_detour).map_err(|e| e.to_string())?;
    let _ = CLEAR_SEL.set(d);
    let d = CLEAR_SEL.get().ok_or("detour not stored")?;
    crate::freeze::enable_detour("guard_clear_selection", "engine_crash_guards", target, d)
}

// ---- dangling ability-phase record in the battle unit phase walk -----------------------------
// Crash exe+0x2326aa4 (thread 1568838476181545013, sandbox battle, general starting to rout; no
// dump, Windows event only). FUN_142326940 walks a battle unit's list of active ability phases
// (log "Unit %u: Phase: %s FINISHED"); for each node rbx it loads the phase record
// rsi = [rbx+0x48], tests it for NULL (je 0x2326b1b) and then reads [rsi+0x78], [rsi+0x94],
// [rsi+0x98]. A non-null but unmapped record faults on the first read. The stub replaces the 26
// bytes at 0x2326aa4 (no rip-relative operand; no branch lands inside, checked against the whole
// .text). When the record is unreadable it clears the node's slot [rbx+0x48] (so every later read
// of node[9] in this function takes its own NULL path instead of passing the dangling pointer to
// vtable +0xa0/+0xb0) and continues at 0x2326b1b, the engine's own "no phase record" path.
const PHASE_SITE_RVA: usize = 0x2326aa4;
/// add r14d,[rsi+78h]; mov [rsp+6Ch],r14d; addss xmm7,[rsi+94h]; addss xmm8,[rsi+98h]
const PHASE_SITE_BYTES: [u8; 26] = [
    0x44, 0x03, 0x76, 0x78, 0x44, 0x89, 0x74, 0x24, 0x6c, 0xf3, 0x0f, 0x58, 0xbe, 0x94, 0x00, 0x00, 0x00,
    0xf3, 0x44, 0x0f, 0x58, 0x86, 0x98, 0x00, 0x00, 0x00,
];
/// test rsi,rsi; je 0x2326b1b (the 2 instructions before the site; pins the NULL path)
const PHASE_PRE_RVA: usize = 0x2326a9f;
const PHASE_PRE_BYTES: [u8; 5] = [0x48, 0x85, 0xf6, 0x74, 0x77];
const PHASE_NULL_PATH_RVA: usize = 0x2326b1b;
static PHASE_STUB: AtomicUsize = AtomicUsize::new(0);
pub static PHASE_HITS: AtomicU64 = AtomicU64::new(0);

/// Called from the stub with rcx = the phase record; true when the 0xa0 bytes it reads are mapped.
unsafe extern "C" fn phase_record_ok(p: usize) -> bool { readable(p, 0xa0) }

/// ```text
///   push rax rcx rdx r8 r9 r10 r11       ; volatile GPRs (rsp 16-aligned in the body -> 8 mod 16)
///   sub rsp,0x68 ; movdqu [rsp+8+16*i],xmm_i (i=0..5)   ; -> 16-aligned again
///   sub rsp,0x20 ; mov rcx,rsi ; mov rax,check ; call rax
///   test al,al                           ; flags survive: lea/movdqu/pop do not touch them
///   lea rsp,[rsp+0x20] ; movdqu xmm_i,[rsp+8+16*i] ; lea rsp,[rsp+0x68] ; pop r11..rax
///   jz bad
///   <the original 26 bytes> ; jmp [rip+0] ; dq cont
/// bad:
///   mov qword [rbx+0x48],0               ; node's phase slot -> NULL
///   push rax ; mov rax,counter ; lock inc qword [rax] ; pop rax
///   jmp [rip+0] ; dq null_path
/// ```
fn phase_stub(orig: &[u8], check: usize, cont: usize, null_path: usize, counter: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(160 + orig.len());
    s.extend_from_slice(&[0x50, 0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53]);
    s.extend_from_slice(&[0x48, 0x83, 0xec, 0x68]);
    for i in 0..6u8 {
        // movdqu [rsp+disp8],xmm_i : f3 0f 7f /r (modrm 01 reg 100, sib 24)
        s.extend_from_slice(&[0xf3, 0x0f, 0x7f, 0x44 | (i << 3), 0x24, 8 + 16 * i]);
    }
    s.extend_from_slice(&[0x48, 0x83, 0xec, 0x20]);
    s.extend_from_slice(&[0x48, 0x89, 0xf1]); // mov rcx,rsi
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(check as u64).to_le_bytes());
    s.extend_from_slice(&[0xff, 0xd0]); // call rax
    s.extend_from_slice(&[0x84, 0xc0]); // test al,al
    s.extend_from_slice(&[0x48, 0x8d, 0x64, 0x24, 0x20]); // lea rsp,[rsp+0x20]
    for i in 0..6u8 {
        s.extend_from_slice(&[0xf3, 0x0f, 0x6f, 0x44 | (i << 3), 0x24, 8 + 16 * i]);
    }
    s.extend_from_slice(&[0x48, 0x8d, 0x64, 0x24, 0x68]); // lea rsp,[rsp+0x68]
    s.extend_from_slice(&[0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5a, 0x59, 0x58]);
    s.extend_from_slice(&[0x74, 0x00]);
    let jz = s.len() - 1;
    s.extend_from_slice(orig);
    s.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    s.extend_from_slice(&(cont as u64).to_le_bytes());
    let bad = s.len();
    s[jz] = (bad - (jz + 1)) as u8;
    s.extend_from_slice(&[0x48, 0xc7, 0x43, 0x48, 0, 0, 0, 0]); // mov qword [rbx+0x48],0
    s.push(0x50);
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(counter as u64).to_le_bytes());
    s.extend_from_slice(&[0xf0, 0x48, 0xff, 0x00, 0x58]);
    s.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    s.extend_from_slice(&(null_path as u64).to_le_bytes());
    s
}

unsafe fn install_phase(base: usize) -> Result<(), String> {
    let site = base + PHASE_SITE_RVA;
    if bytes_at::<5>(base + PHASE_PRE_RVA) != Some(PHASE_PRE_BYTES) { return Err("NULL-test bytes differ".into()); }
    if bytes_at::<26>(site) != Some(PHASE_SITE_BYTES) { return Err(format!("bytes at {site:#x} differ")); }
    let stub = alloc_near(site).ok_or("no free page within rel32 reach")?;
    let code = phase_stub(
        &PHASE_SITE_BYTES,
        phase_record_ok as *const () as usize,
        site + PHASE_SITE_BYTES.len(),
        base + PHASE_NULL_PATH_RVA,
        &PHASE_HITS as *const AtomicU64 as usize,
    );
    core::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
    make_executable(stub, code.len());
    let disp = stub as i64 - (site as i64 + 5);
    if disp < i32::MIN as i64 || disp > i32::MAX as i64 { return Err("stub out of rel32 reach".into()); }
    let mut patch = vec![0x90u8; PHASE_SITE_BYTES.len()];
    patch[0] = 0xe9;
    patch[1..5].copy_from_slice(&(disp as i32).to_le_bytes());
    write_code("guard_phase_record", site, &patch)?;
    PHASE_STUB.store(stub, Ordering::Relaxed);
    Ok(())
}

// ---- stale payload container behind CcoCampaignPayloadInfo (start-of-turn notifications) ------
// Crash exe+0x19f23c2 (Mr.NanaLaptop threads 1566711255 / 1567618285 "closing start of turn
// notifications" at 0x19f23c2/0x19f23cd, and the 190E Emishi T1->T2 hang/crash, 8 Nov). The event
// UI builds a CcoCampaignPayloadInfo (0x142f7b020 -> 0x142f2da50 -> 0x1419f2320) and
// FUN_1419f2370(container, ctx, out) walks the payload container (+4 count, +8 array of payload
// objects; vtable +0x38 / +0x18 per payload). The container was freed: count 0x35395f72, array
// garbage. The detour skips the walk when the container or any payload in it is not plausible,
// leaving `out` as the caller passed it (0x1419f2320 zero-fills it first = an empty payload
// list; the other caller, the tail-jump at 0x1419989ff, refreshes an existing list, which then
// keeps its previous contents).
// LIMITS: it turns the crash into a skipped payload list; it cannot stop a hang where the freed
// container still holds a plausible count and plausible-looking entries. The Emishi root cause is
// data (EVD C099's CREATE_AUTOMATIC_DIPLOMACY_DEAL payloads; wafinish), not this guard.
type PayloadWalk = unsafe extern "C" fn(P, P, P);
const PAYLOAD_WALK_RVA: usize = 0x19f2370;
/// mov rax,rsp; mov [rax+18h],r8; mov [rax+10h],rdx; push r15; sub rsp,80h
const PAYLOAD_WALK_ANCHOR: [u8; 16] = [0x48, 0x8b, 0xc4, 0x4c, 0x89, 0x40, 0x18, 0x48, 0x89, 0x50, 0x10, 0x41, 0x57, 0x48, 0x81, 0xec];
/// An event carries a handful of payloads; vanilla's largest incident has well under 64.
const PAYLOAD_COUNT_CAP: u32 = 0x400;
static PAYLOAD_WALK: OnceLock<GenericDetour<PayloadWalk>> = OnceLock::new();
pub static PAYLOAD_HITS: AtomicU64 = AtomicU64::new(0);
static LOGGED_PAYLOAD: AtomicU32 = AtomicU32::new(0);

/// Container readable, count within the cap, array readable, and every payload readable with a
/// vtable inside the exe image (the walk calls vtable +0x18 and +0x38 and reads payload+8).
unsafe fn payload_container_ok(c: usize, image: (usize, usize)) -> bool {
    if !readable(c, 0x10) { return false; }
    let n = core::ptr::read_unaligned((c + 4) as *const u32);
    if n > PAYLOAD_COUNT_CAP { return false; }
    if n == 0 { return true; }
    let arr = core::ptr::read_unaligned((c + 8) as *const usize);
    if !readable(arr, n as usize * 8) { return false; }
    (0..n as usize).all(|i| {
        let e = core::ptr::read_unaligned((arr + i * 8) as *const usize);
        readable(e, 0x10) && {
            let vt = core::ptr::read_unaligned(e as *const usize);
            vt >= image.0 && vt < image.1
        }
    })
}

unsafe extern "C" fn payload_walk_detour(c: P, ctx: P, out: P) {
    let Some(h) = PAYLOAD_WALK.get() else { return };
    let image = IMAGE.get().copied().unwrap_or((0, usize::MAX));
    if !payload_container_ok(c as usize, image) {
        let n = PAYLOAD_HITS.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::chatty(&LOGGED_PAYLOAD, 5) {
            log!("engine guard: payload info on a stale payload container ({:#x}, #{n}); list left empty", c as usize);
        }
        return;
    }
    h.call(c, ctx, out)
}

unsafe fn install_payload_walk(base: usize) -> Result<(), String> {
    let target = base + PAYLOAD_WALK_RVA;
    if bytes_at::<16>(target) != Some(PAYLOAD_WALK_ANCHOR) { return Err(format!("bytes at {target:#x} differ")); }
    if IMAGE.get().is_none() {
        let image = image_range(base).ok_or("PE header unreadable")?;
        let _ = IMAGE.set(image);
    }
    let f: PayloadWalk = core::mem::transmute(target);
    let d = GenericDetour::new(f, payload_walk_detour).map_err(|e| e.to_string())?;
    let _ = PAYLOAD_WALK.set(d);
    let d = PAYLOAD_WALK.get().ok_or("detour not stored")?;
    crate::freeze::enable_detour("guard_payload_walk", "engine_crash_guards", target, d)
}

// ---- move_to_faction_and_make_recruited with a faction key that does not exist -----------------
// Crash exe+0x17c60d0 (Batch1104 MoveToFaction, 3-5 Nov; Batch1112 threads 1570313732217766004 /
// 1570353268746489960, 5 identical reports on SE 0.48.0). FUN_1415ec970 (the engine routine behind
// modify_character:move_to_faction_and_make_recruited) looks the faction up by key (rsi); when it
// is NULL the `je` at 0x15ec9ae skips the court move but falls into the pool branch, which does
// `lea rcx,[rsi+0xd60]; call FUN_1417c60c0` for a recruitment-pool character and reads 0xdb0.
// The stub replaces `mov rsi,rax; test rax,rax; je` (8 bytes at 0x15ec9a8) and sends a NULL
// faction to the function's epilogue at 0x15eca2c (rbx untouched on that path, rbp/rsi/rdi are
// restored from the frame there): a move to an unknown key does nothing, for every caller.
// A known key runs exactly as before. Off with guard_move_faction=0.
const MOVE_SITE_RVA: usize = 0x15ec9a8;
/// mov rsi,rax; test rax,rax; je 0x15ec9f9
const MOVE_SITE_BYTES: [u8; 8] = [0x48, 0x8b, 0xf0, 0x48, 0x85, 0xc0, 0x74, 0x49];
/// the instruction after the je (mov rcx,[rdi+18h]) = the "faction found" continuation
const MOVE_CONT_RVA: usize = 0x15ec9b0;
const MOVE_CONT_BYTES: [u8; 4] = [0x48, 0x8b, 0x4f, 0x18];
/// epilogue: mov rcx,[rbp+8]; lea rax,[rip+..]
const MOVE_EPILOGUE_RVA: usize = 0x15eca2c;
const MOVE_EPILOGUE_BYTES: [u8; 7] = [0x48, 0x8b, 0x4d, 0x08, 0x48, 0x8d, 0x05];
static MOVE_STUB: AtomicUsize = AtomicUsize::new(0);
/// incremented by the stub itself
pub static MOVE_HITS: AtomicU64 = AtomicU64::new(0);

unsafe fn install_move_faction(base: usize) -> Result<(), String> {
    if !crate::build::hook_enabled("guard_move_faction") { return Err("off (guard_move_faction=0)".into()); }
    let site = base + MOVE_SITE_RVA;
    if bytes_at::<4>(base + MOVE_CONT_RVA) != Some(MOVE_CONT_BYTES) { return Err("continuation bytes differ".into()); }
    if bytes_at::<7>(base + MOVE_EPILOGUE_RVA) != Some(MOVE_EPILOGUE_BYTES) { return Err("epilogue bytes differ".into()); }
    if bytes_at::<8>(site) != Some(MOVE_SITE_BYTES) { return Err(format!("bytes at {site:#x} differ")); }
    let stub = alloc_near(site).ok_or("no free page within rel32 reach")?;
    let code = null_branch_stub(
        [0x48, 0x85, 0xc0],
        &MOVE_SITE_BYTES[..3],
        base + MOVE_CONT_RVA,
        base + MOVE_EPILOGUE_RVA,
        &MOVE_HITS as *const AtomicU64 as usize,
    );
    core::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
    make_executable(stub, code.len());
    let patch = jmp_patch(site, stub, MOVE_SITE_BYTES.len())?;
    write_code_for("guard_move_faction", "guard_move_faction", site, &patch)?;
    MOVE_STUB.store(stub, Ordering::Relaxed);
    Ok(())
}

/// `jmp rel32 stub` padded with nops to `len` bytes.
fn jmp_patch(site: usize, stub: usize, len: usize) -> Result<Vec<u8>, String> {
    let disp = stub as i64 - (site as i64 + 5);
    if disp < i32::MIN as i64 || disp > i32::MAX as i64 { return Err("stub out of rel32 reach".into()); }
    let mut patch = vec![0x90u8; len];
    patch[0] = 0xe9;
    patch[1..5].copy_from_slice(&(disp as i32).to_le_bytes());
    Ok(patch)
}

/// True when the 5-byte jmp at `site` lands on `stub`.
unsafe fn jumps_to(site: usize, stub: usize) -> bool {
    stub != 0 && bytes_at::<1>(site) == Some([0xe9]) && {
        let rel = core::ptr::read_unaligned((site + 1) as *const i32);
        (site as isize + 5 + rel as isize) as usize == stub
    }
}

// ---- UI Cco list getter walking an unlinked list -----------------------------------------------
// Crash exe+0x1c254d4 `mov ecx,[rcx+0x228]` with rcx = 0x18 (Batch1112 threads 1570343238504550456
// and 1570131720823382047, end turn early in a campaign; in-house 5 Nov SiegeAI soak and 8 Nov
// InstallF dry run). FUN_142f99bf0(obj, out) walks the list at obj+0x38 (head [obj+0x40], next at
// node+8, sentinel obj+0x38) and looks up node+0x18 for each node; a NULL next pointer (the list
// was unlinked under the UI) reached the lookup as 0x18. Two stubs check the node before the body
// uses it: at the loop entry (0x2f99c2b `cmp rbx,rbp; je 0x2f99d6d`) and at the loop bottom
// (0x2f99d50 `mov rbx,[rbx+8]; cmp rbx,rbp; jne 0x2f99c40`). A node below 0x10000, not 8-aligned
// or not a canonical user address ends the walk on the function's own exits (0x2f99d6d before the
// first node, 0x2f99d5d after it), so the caller gets the entries collected so far. rax is kept
// (live at the entry: the frame pointer); the flags are dead at every target. No direct branch in
// .text lands inside either patched range (whole-.text scan, notes/guards_1112.md). A freed node
// that is still a plausible pointer is not detected. Off with guard_cco_list_walk=0.
const LIST_ENTRY_RVA: usize = 0x2f99c2b;
/// cmp rbx,rbp; je 0x2f99d6d
const LIST_ENTRY_BYTES: [u8; 9] = [0x48, 0x3b, 0xdd, 0x0f, 0x84, 0x39, 0x01, 0x00, 0x00];
/// mov [rax+18h],rsi; mov [rax-8],rdi (the first-node continuation)
const LIST_ENTRY_CONT_RVA: usize = 0x2f99c34;
const LIST_ENTRY_CONT_BYTES: [u8; 8] = [0x48, 0x89, 0x70, 0x18, 0x48, 0x89, 0x78, 0xf8];
const LIST_BOTTOM_RVA: usize = 0x2f99d50;
/// mov rbx,[rbx+8]; cmp rbx,rbp; jne 0x2f99c40
const LIST_BOTTOM_BYTES: [u8; 13] = [0x48, 0x8b, 0x5b, 0x08, 0x48, 0x3b, 0xdd, 0x0f, 0x85, 0xe3, 0xfe, 0xff, 0xff];
/// loop head: mov rax,[rip+..]
const LIST_HEAD_RVA: usize = 0x2f99c40;
const LIST_HEAD_BYTES: [u8; 3] = [0x48, 0x8b, 0x05];
/// exit after the loop (restores rdi, rsi, rbx, rbp): mov rdi,[rsp+0C0h]
const LIST_DONE_RVA: usize = 0x2f99d5d;
const LIST_DONE_BYTES: [u8; 8] = [0x48, 0x8b, 0xbc, 0x24, 0xc0, 0x00, 0x00, 0x00];
/// exit for an empty list (restores rbx, rbp): mov rbx,[rsp+0D0h]
const LIST_EMPTY_RVA: usize = 0x2f99d6d;
const LIST_EMPTY_BYTES: [u8; 8] = [0x48, 0x8b, 0x9c, 0x24, 0xd0, 0x00, 0x00, 0x00];
/// The function itself (prologue `mov rax,rsp; sub rsp,0C8h`), for the selftest call.
const LIST_FN_RVA: usize = 0x2f99bf0;
const LIST_FN_ANCHOR: [u8; 10] = [0x48, 0x8b, 0xc4, 0x48, 0x81, 0xec, 0xc8, 0x00, 0x00, 0x00];
type CcoListGet = unsafe extern "C" fn(P, P);
/// the stub page: entry stub at +0, bottom stub at +0x80
static LIST_STUBS: AtomicUsize = AtomicUsize::new(0);
/// incremented by the stubs themselves
pub static LIST_HITS: AtomicU64 = AtomicU64::new(0);

/// ```text
///   <prefix>                              ; bottom: mov rbx,[rbx+8]; entry: nothing
///   cmp rbx,rbp ; je end
///   cmp rbx,0x10000 ; jb bad
///   test bl,7 ; jnz bad
///   push rax ; mov rax,rbx ; shr rax,47 ; pop rax ; jnz bad     ; rax kept, flags from shr
///   jmp [rip+0] ; dq cont
/// end:
///   jmp [rip+0] ; dq exit
/// bad:
///   push rax ; mov rax,counter ; lock inc qword [rax] ; pop rax
///   jmp end
/// ```
fn list_stub(prefix: &[u8], cont: usize, exit: usize, counter: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(96);
    s.extend_from_slice(prefix);
    s.extend_from_slice(&[0x48, 0x3b, 0xdd]); // cmp rbx,rbp
    s.extend_from_slice(&[0x74, 0x00]);
    let je_end = s.len() - 1;
    let mut to_bad = Vec::new();
    s.extend_from_slice(&[0x48, 0x81, 0xfb, 0x00, 0x00, 0x01, 0x00]); // cmp rbx,0x10000
    s.extend_from_slice(&[0x72, 0x00]);
    to_bad.push(s.len() - 1);
    s.extend_from_slice(&[0xf6, 0xc3, 0x07]); // test bl,7
    s.extend_from_slice(&[0x75, 0x00]);
    to_bad.push(s.len() - 1);
    s.extend_from_slice(&[0x50, 0x48, 0x89, 0xd8, 0x48, 0xc1, 0xe8, 0x2f, 0x58]); // push rax; mov rax,rbx; shr rax,47; pop rax
    s.extend_from_slice(&[0x75, 0x00]);
    to_bad.push(s.len() - 1);
    s.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    s.extend_from_slice(&(cont as u64).to_le_bytes());
    let end = s.len();
    s.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    s.extend_from_slice(&(exit as u64).to_le_bytes());
    let bad = s.len();
    s.push(0x50);
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(counter as u64).to_le_bytes());
    s.extend_from_slice(&[0xf0, 0x48, 0xff, 0x00, 0x58]);
    s.extend_from_slice(&[0xeb, 0x00]);
    let back = s.len();
    s[back - 1] = (end as isize - back as isize) as i8 as u8;
    s[je_end] = (end - (je_end + 1)) as u8;
    for at in to_bad { s[at] = (bad - (at + 1)) as u8; }
    s
}

unsafe fn install_cco_list_walk(base: usize) -> Result<(), String> {
    if !crate::build::hook_enabled("guard_cco_list_walk") { return Err("off (guard_cco_list_walk=0)".into()); }
    let checks: [(usize, &[u8]); 7] = [
        (LIST_FN_RVA, &LIST_FN_ANCHOR),
        (LIST_ENTRY_CONT_RVA, &LIST_ENTRY_CONT_BYTES),
        (LIST_HEAD_RVA, &LIST_HEAD_BYTES),
        (LIST_DONE_RVA, &LIST_DONE_BYTES),
        (LIST_EMPTY_RVA, &LIST_EMPTY_BYTES),
        (LIST_ENTRY_RVA, &LIST_ENTRY_BYTES),
        (LIST_BOTTOM_RVA, &LIST_BOTTOM_BYTES),
    ];
    for (rva, b) in checks {
        let a = base + rva;
        if !readable(a, b.len()) || core::slice::from_raw_parts(a as *const u8, b.len()) != b { return Err(format!("bytes at {a:#x} differ")); }
    }
    let page = alloc_near(base + LIST_ENTRY_RVA).ok_or("no free page within rel32 reach")?;
    let counter = &LIST_HITS as *const AtomicU64 as usize;
    let entry = list_stub(&[], base + LIST_ENTRY_CONT_RVA, base + LIST_EMPTY_RVA, counter);
    let bottom = list_stub(&LIST_BOTTOM_BYTES[..4], base + LIST_HEAD_RVA, base + LIST_DONE_RVA, counter);
    let (entry_at, bottom_at) = (page, page + 0x80);
    core::ptr::copy_nonoverlapping(entry.as_ptr(), entry_at as *mut u8, entry.len());
    core::ptr::copy_nonoverlapping(bottom.as_ptr(), bottom_at as *mut u8, bottom.len());
    make_executable(page, 0x80 + bottom.len());
    let p_entry = jmp_patch(base + LIST_ENTRY_RVA, entry_at, LIST_ENTRY_BYTES.len())?;
    let p_bottom = jmp_patch(base + LIST_BOTTOM_RVA, bottom_at, LIST_BOTTOM_BYTES.len())?;
    write_code_for("guard_cco_list_bottom", "guard_cco_list_walk", base + LIST_BOTTOM_RVA, &p_bottom)?;
    write_code_for("guard_cco_list_entry", "guard_cco_list_walk", base + LIST_ENTRY_RVA, &p_entry)?;
    LIST_STUBS.store(page, Ordering::Relaxed);
    Ok(())
}

static LIST_OUT_CALLS: AtomicU64 = AtomicU64::new(0);
static LIST_OUT_COUNT: AtomicU64 = AtomicU64::new(u64::MAX);
/// `out` vtable slot 0x30 for the selftest: records the element count of the list it receives.
unsafe extern "C" fn fake_list_out(_this: P, list: *const u32) {
    LIST_OUT_CALLS.fetch_add(1, Ordering::Relaxed);
    LIST_OUT_COUNT.store(core::ptr::read_unaligned(list.add(1)) as u64, Ordering::Relaxed);
}

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("engine_crash_guards") {
        log!("engine crash guards off (engine_crash_guards=0 in script_extender.cfg)");
        return;
    }
    let base = t.base();
    BASE.store(base, Ordering::Relaxed);
    // SAFETY: every guard verifies its bytes first and writes only with other threads frozen.
    unsafe {
        let steps: [(&str, unsafe fn(usize) -> Result<(), String>); 10] = [
            ("inactive_string", install_inactive_string),
            ("ceo_owner", install_ceo_owner),
            ("text_font", install_font),
            ("building_browser", install_browser),
            ("weak_lock", install_weak),
            ("clear_selection", install_clear_selection),
            ("phase_record", install_phase),
            ("payload_walk", install_payload_walk),
            ("move_faction", install_move_faction),
            ("cco_list_walk", install_cco_list_walk),
        ];
        let mut ok = Vec::new();
        for (name, f) in steps {
            match f(base) {
                Ok(()) => ok.push(name),
                Err(e) => log!("engine guard {name} not installed: {e}"),
            }
        }
        log!("engine crash guards installed: {}", ok.join(", "));
    }
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_engine_guards_info", se_engine_guards_info);
    lua::set_global_fn(l, "se_engine_guards_selftest", se_engine_guards_selftest);
}

fn info() -> String {
    format!(
        "inactive_string={};inactive_string_hits={};ceo_owner={};ceo_owner_hits={};text_font={};building_browser={};building_browser_hits={};weak_lock_hits={};clear_selection={};clear_selection_hits={};phase_record={};phase_record_hits={};payload_walk={};payload_walk_hits={};move_faction={};move_faction_hits={};cco_list_walk={};cco_list_walk_hits={}",
        INACTIVE.get().is_some_and(|d| d.is_enabled()) as u8, INACTIVE_HITS.load(Ordering::Relaxed),
        CEO_OWNER.get().is_some_and(|d| d.is_enabled()) as u8, CEO_OWNER_HITS.load(Ordering::Relaxed),
        FONT_PATCHED.load(Ordering::Relaxed) as u8,
        (BROWSER_STUB.load(Ordering::Relaxed) != 0) as u8, BROWSER_HITS.load(Ordering::Relaxed), WEAK_HITS.load(Ordering::Relaxed),
        CLEAR_SEL.get().is_some_and(|d| d.is_enabled()) as u8, CLEAR_SEL_HITS.load(Ordering::Relaxed),
        (PHASE_STUB.load(Ordering::Relaxed) != 0) as u8, PHASE_HITS.load(Ordering::Relaxed),
        PAYLOAD_WALK.get().is_some_and(|d| d.is_enabled()) as u8, PAYLOAD_HITS.load(Ordering::Relaxed),
        (MOVE_STUB.load(Ordering::Relaxed) != 0) as u8, MOVE_HITS.load(Ordering::Relaxed),
        (LIST_STUBS.load(Ordering::Relaxed) != 0) as u8, LIST_HITS.load(Ordering::Relaxed),
    )
}

/// se_engine_guards_info() -> "inactive_string=1;inactive_string_hits=0;..."
unsafe extern "C" fn se_engine_guards_info(l: *mut LuaState) -> c_int {
    lua::push_str(l, &info());
    1
}

static SETTER_CALLS: AtomicU64 = AtomicU64::new(0);
unsafe extern "C" fn fake_setter_slot(_this: P, _value: P) { SETTER_CALLS.fetch_add(1, Ordering::Relaxed); }
unsafe extern "C" fn fake_setter_other(_this: P) {}

/// se_engine_guards_selftest() -> ok, message
///   Calls the two detoured engine functions, through their real (patched) entry points, on
///   objects built here: InactiveString on an event with no inactive group (must return without
///   touching the setter), the same with a group (the original must call the setter once), and the
///   CEO lookup with `this` = 0x10 (must answer 0). Re-reads the two code patches. Nothing in the
///   game's own objects is touched. Refuses for a guard that is not installed.
unsafe extern "C" fn se_engine_guards_selftest(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let base = BASE.load(Ordering::Relaxed);
    let mut lines: Vec<String> = Vec::new();
    let mut ok = true;
    if INACTIVE.get().is_some_and(|d| d.is_enabled()) {
        // setter: vtable slot 0x80 = fake_setter_slot (the only slot this path calls)
        let mut vt = [fake_setter_other as *const () as usize; 0x20];
        vt[0x80 / 8] = fake_setter_slot as *const () as usize;
        let setter: [usize; 2] = [vt.as_ptr() as usize, 0];
        let rec: [usize; 4] = [0, 0, 0, 0x1234];
        let mut ev = [0u8; 0x60];
        ev[0x18..0x20].copy_from_slice(&(rec.as_ptr() as usize).to_le_bytes());
        let cco: [usize; 6] = [0, 0, 0, ev.as_ptr() as usize, 0, 0];
        let f: InactiveString = core::mem::transmute(base + INACTIVE_STRING_RVA);
        let (h0, s0) = (INACTIVE_HITS.load(Ordering::Relaxed), SETTER_CALLS.load(Ordering::Relaxed));
        f(cco.as_ptr() as P, setter.as_ptr() as P);
        let guarded = INACTIVE_HITS.load(Ordering::Relaxed) == h0 + 1 && SETTER_CALLS.load(Ordering::Relaxed) == s0;
        // control: with an inactive group the original runs and sets its string
        ev[0x20..0x28].copy_from_slice(&(rec.as_ptr() as usize).to_le_bytes());
        f(cco.as_ptr() as P, setter.as_ptr() as P);
        let control = INACTIVE_HITS.load(Ordering::Relaxed) == h0 + 1 && SETTER_CALLS.load(Ordering::Relaxed) == s0 + 1;
        ok &= guarded && control;
        lines.push(format!("inactive_string: guarded={guarded} original={control}"));
    } else {
        ok = false;
        lines.push("inactive_string: not installed".into());
    }
    if CEO_OWNER.get().is_some_and(|d| d.is_enabled()) {
        let f: CeoOwnerHas = core::mem::transmute(base + CEO_OWNER_HAS_RVA);
        let h0 = CEO_OWNER_HITS.load(Ordering::Relaxed);
        let r = f(0x10 as P, core::ptr::null_mut());
        let pass = r == 0 && CEO_OWNER_HITS.load(Ordering::Relaxed) == h0 + 1;
        ok &= pass;
        lines.push(format!("ceo_owner: this=0x10 -> {r} pass={pass}"));
    } else {
        ok = false;
        lines.push("ceo_owner: not installed".into());
    }
    let font = FONT_PATCHED.load(Ordering::Relaxed) && bytes_at::<11>(base + FONT_SITE_RVA) == Some(FONT_SITE_AFTER);
    ok &= font;
    lines.push(format!("text_font: patch in place={font}"));
    let stub = BROWSER_STUB.load(Ordering::Relaxed);
    let browser = stub != 0 && {
        let site = base + BROWSER_CALL_RVA;
        let rel = core::ptr::read_unaligned((site + 1) as *const i32);
        (site as isize + 5 + rel as isize) as usize == stub
    };
    ok &= browser;
    lines.push(format!("building_browser: call redirected={browser}"));
    // stale inputs only: the guards reject them before any engine code runs
    if CLEAR_SEL.get().is_some_and(|d| d.is_enabled()) {
        let mut stale = [0u8; 0x118];
        stale[0..8].copy_from_slice(&0x2_0000_1000usize.to_le_bytes()); // vtable outside the exe
        stale[0x10c..0x110].copy_from_slice(&0x7ff7u32.to_le_bytes());
        stale[0x110..0x118].copy_from_slice(&0x4e1d_29c3_fca5_21a7usize.to_le_bytes());
        let f: ClearSelection = core::mem::transmute(base + CLEAR_SELECTION_RVA);
        let h0 = CLEAR_SEL_HITS.load(Ordering::Relaxed);
        f(stale.as_mut_ptr() as P, 0);
        let pass = CLEAR_SEL_HITS.load(Ordering::Relaxed) == h0 + 1;
        ok &= pass;
        lines.push(format!("clear_selection: stale list skipped={pass}"));
    } else {
        ok = false;
        lines.push("clear_selection: not installed".into());
    }
    let phase = PHASE_STUB.load(Ordering::Relaxed) != 0 && bytes_at::<1>(base + PHASE_SITE_RVA) == Some([0xe9]);
    ok &= phase;
    lines.push(format!("phase_record: jump in place={phase}"));
    if PAYLOAD_WALK.get().is_some_and(|d| d.is_enabled()) {
        let mut stale = [0u8; 0x10];
        stale[4..8].copy_from_slice(&0x3539_5f72u32.to_le_bytes());
        stale[8..16].copy_from_slice(&0x0226_0376_0226_0300usize.to_le_bytes());
        let mut out = [0usize; 8];
        let f: PayloadWalk = core::mem::transmute(base + PAYLOAD_WALK_RVA);
        let h0 = PAYLOAD_HITS.load(Ordering::Relaxed);
        f(stale.as_mut_ptr() as P, core::ptr::null_mut(), out.as_mut_ptr() as P);
        let pass = PAYLOAD_HITS.load(Ordering::Relaxed) == h0 + 1 && out.iter().all(|&v| v == 0);
        ok &= pass;
        lines.push(format!("payload_walk: stale container skipped={pass}"));
    } else {
        ok = false;
        lines.push("payload_walk: not installed".into());
    }
    // move_faction: the jump is in place (calling the move routine needs a live character)
    // A guard switched off in the cfg is reported, not failed (its own switch exists for that).
    if !crate::build::hook_enabled("guard_move_faction") {
        lines.push("move_faction: off (cfg)".into());
    } else {
        let mv = jumps_to(base + MOVE_SITE_RVA, MOVE_STUB.load(Ordering::Relaxed));
        ok &= mv;
        lines.push(format!("move_faction: jump in place={mv}"));
    }
    // cco_list_walk: the getter on objects built here (the loop body, which reads the game, is
    // never reached): a NULL head must end the walk (hit, empty list handed to out); an empty
    // list (head = sentinel) must take the vanilla exit (no hit)
    let page = LIST_STUBS.load(Ordering::Relaxed);
    if !crate::build::hook_enabled("guard_cco_list_walk") {
        lines.push("cco_list_walk: off (cfg)".into());
    } else if page != 0 && jumps_to(base + LIST_ENTRY_RVA, page) && jumps_to(base + LIST_BOTTOM_RVA, page + 0x80) {
        let mut vt = [0usize; 8];
        vt[0x30 / 8] = fake_list_out as *const () as usize;
        let out: [usize; 2] = [vt.as_ptr() as usize, 0];
        let mut obj = [0usize; 10];
        let f: CcoListGet = core::mem::transmute(base + LIST_FN_RVA);
        let (h0, c0) = (LIST_HITS.load(Ordering::Relaxed), LIST_OUT_CALLS.load(Ordering::Relaxed));
        f(obj.as_mut_ptr() as P, out.as_ptr() as P);
        let guarded = LIST_HITS.load(Ordering::Relaxed) == h0 + 1 && LIST_OUT_CALLS.load(Ordering::Relaxed) == c0 + 1 && LIST_OUT_COUNT.load(Ordering::Relaxed) == 0;
        obj[8] = obj.as_ptr() as usize + 0x38; // head = the sentinel at obj+0x38
        f(obj.as_mut_ptr() as P, out.as_ptr() as P);
        let control = LIST_HITS.load(Ordering::Relaxed) == h0 + 1 && LIST_OUT_CALLS.load(Ordering::Relaxed) == c0 + 2 && LIST_OUT_COUNT.load(Ordering::Relaxed) == 0;
        ok &= guarded && control;
        lines.push(format!("cco_list_walk: null head stopped={guarded} empty list vanilla={control}"));
    } else {
        ok = false;
        lines.push("cco_list_walk: not installed".into());
    }
    let msg = lines.join("; ");
    log!("se_engine_guards_selftest: ok={ok}: {msg}");
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &msg);
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_patch_targets() {
        // jz at site+7 (2 bytes) lands on 0x670; jmp at site+9 lands on the loop bottom 0x66b
        let site = FONT_SITE_RVA;
        assert_eq!(site + 9 + FONT_SITE_AFTER[8] as usize, 0x641670);
        assert_eq!(site + 11 + FONT_SITE_AFTER[10] as usize, FONT_LOOP_BOTTOM_RVA);
        // the original jnc went to the same 0x670
        assert_eq!(site + 9 + FONT_SITE_BEFORE[8] as usize, 0x641670);
        // same length, same first instruction, next instruction boundary (0x650) unchanged
        assert_eq!(FONT_SITE_BEFORE.len(), FONT_SITE_AFTER.len());
        assert_eq!(FONT_SITE_BEFORE[..4], FONT_SITE_AFTER[..4]);
        // the loop bottom's jc goes back to 0x650 = site + 11
        assert_eq!((FONT_LOOP_BOTTOM_RVA + 5) as isize + (FONT_LOOP_BOTTOM[4] as i8) as isize, (site + 11) as isize);
    }

    #[test]
    fn weak_stub_layout() {
        for w in WEAK_SITES.iter() {
            let s = weak_stub(w.test, w.bytes, 0x1111_2222_3333_4444, 0x5555_6666_7777_8888);
            // jz lands on the null path, right after the original bytes + jmp [rip+0] + target
            let jz = 5 + s[4] as usize;
            assert_eq!(jz, 5 + w.bytes.len() + 14);
            assert_eq!(s[jz], 0x50);
            assert_eq!(&s[5..5 + w.bytes.len()], w.bytes);
        }
    }

    #[test]
    fn browser_call_pins_resolve() {
        let rel = i32::from_le_bytes(BROWSER_CALL_BYTES[1..5].try_into().unwrap());
        assert_eq!((BROWSER_CALL_RVA as isize + 5 + rel as isize) as usize, BROWSER_RESOLVE_RVA);
    }

    #[test]
    fn browser_stub_layout() {
        let s = browser_stub(0x1111_2222_3333_4444, 0x5555_6666_7777_8888, 0x9999_aaaa_bbbb_cccc);
        // jz +1 skips exactly the ret
        let jz = s.windows(2).position(|w| w == [0x74, 0x01]).unwrap();
        assert_eq!(s[jz + 2], 0xc3);
        assert_eq!(&s[jz + 3..jz + 5], &[0x48, 0xb9]);
        assert_eq!(&s[6..14], &0x1111_2222_3333_4444u64.to_le_bytes());
        assert_eq!(&s[s.len() - 10..s.len() - 2], &0x5555_6666_7777_8888u64.to_le_bytes());
        assert_eq!(&s[s.len() - 2..], &[0xff, 0xe0]);
        // stack: one sub 0x28 balanced by add 0x28, then the dropped return address
        assert_eq!(&s[..4], &[0x48, 0x83, 0xec, 0x28]);
        assert!(s.windows(4).any(|w| w == [0x48, 0x83, 0xc4, 0x28]));
        assert!(s.windows(4).any(|w| w == [0x48, 0x83, 0xc4, 0x08]));
    }

    #[test]
    fn inactive_decision() {
        unsafe {
            let rec = [0usize; 4];
            let mut ev = [0u8; 0x60];
            let cco_of = |ev: &[u8; 0x60]| [0usize, 0, 0, ev.as_ptr() as usize];
            // no active pointer at +0x18: the original returns by itself
            assert!(!inactive_would_fault(cco_of(&ev).as_ptr() as usize));
            ev[0x18..0x20].copy_from_slice(&(rec.as_ptr() as usize).to_le_bytes());
            assert!(inactive_would_fault(cco_of(&ev).as_ptr() as usize));
            // value-dependent branch (+0x2c != 0) is null-checked by the engine
            ev[0x2c] = 1;
            assert!(!inactive_would_fault(cco_of(&ev).as_ptr() as usize));
            ev[0x2c] = 0;
            ev[0x20..0x28].copy_from_slice(&(rec.as_ptr() as usize).to_le_bytes());
            assert!(!inactive_would_fault(cco_of(&ev).as_ptr() as usize));
            // no event at all
            assert!(!inactive_would_fault([0usize; 4].as_ptr() as usize));
        }
    }

    #[test]
    fn clear_selection_anchor_is_prologue() {
        // mov [rsp+8],rbx; mov [rsp+10h],rbp; mov [rsp+18h],rsi; push rdi (the dump's bytes)
        assert_eq!(CLEAR_SELECTION_ANCHOR[..5], [0x48, 0x89, 0x5c, 0x24, 0x08]);
        assert_eq!(CLEAR_SELECTION_ANCHOR[15], 0x57);
    }

    #[test]
    fn selection_list_decision() {
        unsafe {
            let items = [0x1111usize, 0x2222, 0x3333];
            let mut obj = [0u8; 0x118];
            let image = (0x1_4000_0000usize, 0x1_4483_6000usize);
            let set_u32 = |o: &mut [u8; 0x118], off: usize, v: u32| o[off..off + 4].copy_from_slice(&v.to_le_bytes());
            let set_ptr = |o: &mut [u8; 0x118], off: usize, v: usize| o[off..off + 8].copy_from_slice(&v.to_le_bytes());
            set_ptr(&mut obj, 0, 0x1_4382_0000);
            // live, empty list
            assert!(selection_list_ok(obj.as_ptr() as usize, image));
            // live list, 3 items, 1 selected
            set_u32(&mut obj, 0xfc, 3);
            set_ptr(&mut obj, 0x100, items.as_ptr() as usize);
            set_u32(&mut obj, 0x10c, 1);
            set_ptr(&mut obj, 0x110, items.as_ptr() as usize);
            assert!(selection_list_ok(obj.as_ptr() as usize, image));
            // the crash: count 0x7ff7, non-canonical array
            set_u32(&mut obj, 0x10c, 0x7ff7);
            set_ptr(&mut obj, 0x110, 0x4e1d_29c3_fca5_21a7);
            assert!(!selection_list_ok(obj.as_ptr() as usize, image));
            // sane count, unreadable array
            set_u32(&mut obj, 0x10c, 2);
            assert!(!selection_list_ok(obj.as_ptr() as usize, image));
            set_ptr(&mut obj, 0x110, items.as_ptr() as usize);
            assert!(selection_list_ok(obj.as_ptr() as usize, image));
            // vtable outside the exe image (block reused by something else)
            set_ptr(&mut obj, 0, 0x2_0000_1000);
            assert!(!selection_list_ok(obj.as_ptr() as usize, image));
            // null / tiny this
            assert!(!selection_list_ok(0, image));
            assert!(!selection_list_ok(0x10, image));
        }
    }

    #[test]
    fn phase_site_pins() {
        // je at 0x2326aa2 (2 bytes, rel8 0x77) lands on the NULL path
        assert_eq!(PHASE_PRE_RVA + 5 + PHASE_PRE_BYTES[4] as usize, PHASE_NULL_PATH_RVA);
        // the site follows the je directly
        assert_eq!(PHASE_PRE_RVA + 5, PHASE_SITE_RVA);
        // continuation = test r13b,r13b at 0x2326abe
        assert_eq!(PHASE_SITE_RVA + PHASE_SITE_BYTES.len(), 0x2326abe);
        // the 26 bytes read rsi only (modrm rm=110 with disp) and carry no rip-relative operand
        assert_eq!(PHASE_SITE_BYTES[2] & 0xc7, 0x46);
        assert_eq!(PHASE_SITE_BYTES[12] & 0xc7, 0x86);
        assert_eq!(PHASE_SITE_BYTES[21] & 0xc7, 0x86);
    }

    #[test]
    fn phase_stub_layout() {
        let (check, cont, null_path, counter) = (0x1111_2222_3333_4444u64, 0x5555_6666_7777_8888u64, 0x9999_aaaa_bbbb_ccccu64, 0xdddd_eeee_ffff_0001u64);
        let s = phase_stub(&PHASE_SITE_BYTES, check as usize, cont as usize, null_path as usize, counter as usize);
        // 7 pushes + sub 0x68 keep rsp 16-aligned at the call (body rsp is 16-aligned)
        assert_eq!((7 * 8 + 0x68 + 0x20) % 16, 0);
        let pushes = &s[..11];
        let pops_at = s.windows(11).position(|w| w == [0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5a, 0x59, 0x58]).unwrap();
        assert_eq!(pushes, &[0x50, 0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53]);
        // the check is called with rcx = rsi and its bool tested before any flag-writing op
        let call = s.windows(2).position(|w| w == [0xff, 0xd0]).unwrap();
        assert_eq!(&s[call - 13..call - 10], &[0x48, 0x89, 0xf1]);
        assert_eq!(&s[call - 8..call], &check.to_le_bytes());
        assert_eq!(&s[call + 2..call + 4], &[0x84, 0xc0]);
        // between the test and the jz: only lea / movdqu / pop (no add/sub/cmp)
        let between = &s[call + 4..pops_at + 11];
        assert!(!between.windows(3).any(|w| w == [0x48, 0x83, 0xc4] || w == [0x48, 0x83, 0xec]));
        let jz = pops_at + 11;
        assert_eq!(s[jz], 0x74);
        // ok path: the original bytes, then jmp [rip+0] cont
        let orig_at = jz + 2;
        assert_eq!(&s[orig_at..orig_at + 26], &PHASE_SITE_BYTES);
        assert_eq!(&s[orig_at + 26..orig_at + 32], &[0xff, 0x25, 0, 0, 0, 0]);
        assert_eq!(&s[orig_at + 32..orig_at + 40], &cont.to_le_bytes());
        // bad path: jz lands on mov qword [rbx+0x48],0, then counter, then jmp to the NULL path
        let bad = jz + 2 + s[jz + 1] as usize;
        assert_eq!(bad, orig_at + 40);
        assert_eq!(&s[bad..bad + 8], &[0x48, 0xc7, 0x43, 0x48, 0, 0, 0, 0]);
        assert_eq!(&s[bad + 11..bad + 19], &counter.to_le_bytes());
        assert_eq!(&s[s.len() - 8..], &null_path.to_le_bytes());
        // xmm0-5 saved and restored at the same slots
        for i in 0..6u8 {
            let st = [0xf3, 0x0f, 0x7f, 0x44 | (i << 3), 0x24, 8 + 16 * i];
            let ld = [0xf3, 0x0f, 0x6f, 0x44 | (i << 3), 0x24, 8 + 16 * i];
            assert!(s.windows(6).any(|w| w == st) && s.windows(6).any(|w| w == ld));
        }
    }

    /// Runs the real stub: harness sets rsi = record, rbx = node, keeps rsp 16-aligned like the
    /// engine body, saves the callee-saved registers the original bytes write (r14, xmm7, xmm8),
    /// and jumps in; the continuation and the NULL path land on two exits returning 1 and 2.
    #[test]
    fn phase_stub_executes() {
        unsafe {
            let page = VirtualAlloc(core::ptr::null_mut(), 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
            assert!(page != 0);
            let exit = |n: u8| -> Vec<u8> {
                let mut e = vec![0xf3, 0x0f, 0x6f, 0xbc, 0x24, 0x80, 0, 0, 0]; // movdqu xmm7,[rsp+0x80]
                e.extend_from_slice(&[0xf3, 0x44, 0x0f, 0x6f, 0x84, 0x24, 0x90, 0, 0, 0]); // movdqu xmm8,[rsp+0x90]
                e.extend_from_slice(&[0x48, 0x81, 0xc4, 0xa0, 0, 0, 0]); // add rsp,0xa0
                e.extend_from_slice(&[0x41, 0x5e, 0x5e, 0x5b]); // pop r14; pop rsi; pop rbx
                e.extend_from_slice(&[0xb8, n, 0, 0, 0, 0xc3]); // mov eax,n; ret
                e
            };
            let (ok_exit, bad_exit) = (exit(1), exit(2));
            let ok_at = page + 0x200;
            let bad_at = ok_at + ok_exit.len();
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let stub = phase_stub(&PHASE_SITE_BYTES, phase_record_ok as *const () as usize, ok_at, bad_at, &COUNTER as *const AtomicU64 as usize);
            let stub_at = page + 0x400;
            let mut h = vec![0x53, 0x56, 0x41, 0x56]; // push rbx; push rsi; push r14
            h.extend_from_slice(&[0x48, 0x81, 0xec, 0xa0, 0, 0, 0]); // sub rsp,0xa0
            h.extend_from_slice(&[0xf3, 0x0f, 0x7f, 0xbc, 0x24, 0x80, 0, 0, 0]); // movdqu [rsp+0x80],xmm7
            h.extend_from_slice(&[0xf3, 0x44, 0x0f, 0x7f, 0x84, 0x24, 0x90, 0, 0, 0]); // movdqu [rsp+0x90],xmm8
            h.extend_from_slice(&[0x48, 0x89, 0xce, 0x48, 0x89, 0xd3]); // mov rsi,rcx; mov rbx,rdx
            h.extend_from_slice(&[0x48, 0xb8]);
            h.extend_from_slice(&(stub_at as u64).to_le_bytes());
            h.extend_from_slice(&[0xff, 0xe0]); // jmp rax
            core::ptr::copy_nonoverlapping(h.as_ptr(), page as *mut u8, h.len());
            core::ptr::copy_nonoverlapping(ok_exit.as_ptr(), ok_at as *mut u8, ok_exit.len());
            core::ptr::copy_nonoverlapping(bad_exit.as_ptr(), bad_at as *mut u8, bad_exit.len());
            core::ptr::copy_nonoverlapping(stub.as_ptr(), stub_at as *mut u8, stub.len());
            FlushInstructionCache(GetCurrentProcess(), page as *const c_void, 0x1000);
            let run: unsafe extern "C" fn(usize, usize) -> u32 = core::mem::transmute(page);

            let record = [7u8; 0xa0];
            let mut node = [0usize; 10];
            node[9] = record.as_ptr() as usize;
            assert_eq!(run(record.as_ptr() as usize, node.as_mut_ptr() as usize), 1);
            assert_eq!(node[9], record.as_ptr() as usize);
            assert_eq!(COUNTER.load(Ordering::Relaxed), 0);

            node[9] = 0x4e1d_29c3_fca5_21a7;
            assert_eq!(run(0x4e1d_29c3_fca5_21a7, node.as_mut_ptr() as usize), 2);
            assert_eq!(node[9], 0, "node phase slot cleared");
            assert_eq!(COUNTER.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn payload_anchor_is_prologue() {
        // mov rax,rsp ... push r15; sub rsp,imm32 (no rip-relative operand for the trampoline)
        assert_eq!(PAYLOAD_WALK_ANCHOR[..3], [0x48, 0x8b, 0xc4]);
        assert_eq!(PAYLOAD_WALK_ANCHOR[11..13], [0x41, 0x57]);
    }

    #[test]
    fn payload_container_decision() {
        unsafe {
            let image = (0x1_4000_0000usize, 0x1_4483_6000usize);
            // two payload objects with an in-image vtable
            let p1 = [0x1_434a_0000usize, 7];
            let p2 = [0x1_434a_0100usize, 8];
            let mut arr = [p1.as_ptr() as usize, p2.as_ptr() as usize];
            let mk = |count: u32, arr: usize| -> [u8; 0x10] {
                let mut c = [0u8; 0x10];
                c[4..8].copy_from_slice(&count.to_le_bytes());
                c[8..16].copy_from_slice(&arr.to_le_bytes());
                c
            };
            let empty = mk(0, 0);
            assert!(payload_container_ok(empty.as_ptr() as usize, image));
            let good = mk(2, arr.as_ptr() as usize);
            assert!(payload_container_ok(good.as_ptr() as usize, image));
            // the Emishi dump: count 0x35395f72, garbage array
            let dump = mk(0x3539_5f72, 0x0226_0376_0226_0300);
            assert!(!payload_container_ok(dump.as_ptr() as usize, image));
            // plausible count, unreadable array
            let bad_arr = mk(2, 0x0226_0376_0226_0300);
            assert!(!payload_container_ok(bad_arr.as_ptr() as usize, image));
            // one entry unreadable
            arr[1] = 0x10;
            assert!(!payload_container_ok(good.as_ptr() as usize, image));
            // one entry readable but its vtable outside the image (freed + reused block)
            let junk = [0x2_0000_1000usize, 0];
            arr[1] = junk.as_ptr() as usize;
            assert!(!payload_container_ok(good.as_ptr() as usize, image));
            arr[1] = p2.as_ptr() as usize;
            assert!(payload_container_ok(good.as_ptr() as usize, image));
            assert!(!payload_container_ok(0, image));
        }
    }

    #[test]
    fn phase_record_check() {
        let rec = [0u8; 0xa0];
        unsafe {
            assert!(phase_record_ok(rec.as_ptr() as usize));
            assert!(!phase_record_ok(0));
            assert!(!phase_record_ok(0x4e1d_29c3_fca5_21a7));
        }
    }

    #[test]
    fn ceo_owner_decision() {
        unsafe {
            assert!(ceo_owner_unusable(0x10));
            assert!(ceo_owner_unusable(0));
            let empty = [0usize; 2];
            assert!(ceo_owner_unusable(empty.as_ptr() as usize));
            let full = [0usize, 0x1234];
            assert!(!ceo_owner_unusable(full.as_ptr() as usize));
        }
    }

    #[test]
    fn move_site_pins() {
        // the je rel8 at site+6 went to 0x15ec9f9 (the vanilla NULL path that still reaches the pool branch)
        assert_eq!(MOVE_SITE_RVA + 8 + MOVE_SITE_BYTES[7] as usize, 0x15ec9f9);
        assert_eq!(MOVE_SITE_RVA + MOVE_SITE_BYTES.len(), MOVE_CONT_RVA);
        // the triage's 1-byte proposal (49 -> 7c) lands on the same epilogue the stub uses
        assert_eq!(MOVE_SITE_RVA + 8 + 0x7c, MOVE_EPILOGUE_RVA);
        // the copied instruction is mov rsi,rax (rip-independent)
        assert_eq!(MOVE_SITE_BYTES[..3], [0x48, 0x8b, 0xf0]);
    }

    #[test]
    fn list_site_pins() {
        // entry je rel32 -> empty exit; bottom jne rel32 -> loop head; bottom falls through to done
        let je = i32::from_le_bytes(LIST_ENTRY_BYTES[5..9].try_into().unwrap());
        assert_eq!((LIST_ENTRY_RVA as isize + 9 + je as isize) as usize, LIST_EMPTY_RVA);
        assert_eq!(LIST_ENTRY_RVA + LIST_ENTRY_BYTES.len(), LIST_ENTRY_CONT_RVA);
        let jne = i32::from_le_bytes(LIST_BOTTOM_BYTES[9..13].try_into().unwrap());
        assert_eq!((LIST_BOTTOM_RVA as isize + 13 + jne as isize) as usize, LIST_HEAD_RVA);
        assert_eq!(LIST_BOTTOM_RVA + LIST_BOTTOM_BYTES.len(), LIST_DONE_RVA);
        // both patched ranges hold the 5-byte jmp
        assert!(LIST_ENTRY_BYTES.len() >= 5 && LIST_BOTTOM_BYTES.len() >= 5 && MOVE_SITE_BYTES.len() >= 5);
    }

    #[test]
    fn jmp_patch_layout() {
        let p = jmp_patch(0x1000, 0x2000, 9).unwrap();
        assert_eq!(p[0], 0xe9);
        assert_eq!(i32::from_le_bytes(p[1..5].try_into().unwrap()), 0x2000 - 0x1005);
        assert_eq!(&p[5..], &[0x90; 4]);
        assert!(jmp_patch(0x1000, 0x1_0000_1000 + 0x1000, 5).is_err());
    }

    /// Executable rig: `run(rcx, rdx, r8) -> code` jumps to the stub with rbx = rcx, rbp = rdx,
    /// rsi = 0x5a, rax = r8; exit n stores rax / rsi into SCRATCH and returns n.
    struct Rig { page: usize }
    static SCRATCH: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
    static RIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl Rig {
        unsafe fn new() -> Rig {
            let page = VirtualAlloc(core::ptr::null_mut(), 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
            assert!(page != 0);
            Rig { page }
        }
        fn exit_at(&self, n: usize) -> usize { self.page + 0x100 + n * 0x40 }
        unsafe fn build(&self, stub: &[u8], exits: usize) -> unsafe extern "C" fn(usize, usize, usize) -> u32 {
            let stub_at = self.page + 0x400;
            let mut h = vec![0x53, 0x55, 0x56]; // push rbx; push rbp; push rsi
            h.extend_from_slice(&[0x48, 0x89, 0xcb, 0x48, 0x89, 0xd5]); // mov rbx,rcx; mov rbp,rdx
            h.extend_from_slice(&[0xbe, 0x5a, 0, 0, 0]); // mov esi,0x5a
            h.extend_from_slice(&[0x49, 0xbb]);
            h.extend_from_slice(&(stub_at as u64).to_le_bytes()); // mov r11,stub
            h.extend_from_slice(&[0x4c, 0x89, 0xc0]); // mov rax,r8
            h.extend_from_slice(&[0x41, 0xff, 0xe3]); // jmp r11
            core::ptr::copy_nonoverlapping(h.as_ptr(), self.page as *mut u8, h.len());
            for n in 1..=exits {
                let mut e = vec![0x49, 0xbb];
                e.extend_from_slice(&(SCRATCH.as_ptr() as u64).to_le_bytes()); // mov r11,&SCRATCH
                e.extend_from_slice(&[0x49, 0x89, 0x03, 0x49, 0x89, 0x73, 0x08]); // mov [r11],rax; mov [r11+8],rsi
                e.extend_from_slice(&[0x5e, 0x5d, 0x5b, 0xb8, n as u8, 0, 0, 0, 0xc3]); // pop rsi; pop rbp; pop rbx; mov eax,n; ret
                core::ptr::copy_nonoverlapping(e.as_ptr(), self.exit_at(n) as *mut u8, e.len());
            }
            core::ptr::copy_nonoverlapping(stub.as_ptr(), stub_at as *mut u8, stub.len());
            FlushInstructionCache(GetCurrentProcess(), self.page as *const c_void, 0x1000);
            core::mem::transmute(self.page)
        }
    }
    fn scratch() -> (u64, u64) { (SCRATCH[0].load(Ordering::Relaxed), SCRATCH[1].load(Ordering::Relaxed)) }

    #[test]
    fn move_stub_executes() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let _g = RIG_LOCK.lock().unwrap();
        unsafe {
            let rig = Rig::new();
            let stub = null_branch_stub([0x48, 0x85, 0xc0], &MOVE_SITE_BYTES[..3], rig.exit_at(1), rig.exit_at(2), &COUNTER as *const AtomicU64 as usize);
            let run = rig.build(&stub, 2);
            // faction found: continuation with rsi = rax (the copied mov), not counted
            assert_eq!(run(0, 0, 0x1234_5678), 1);
            assert_eq!(scratch(), (0x1234_5678, 0x1234_5678));
            assert_eq!(COUNTER.load(Ordering::Relaxed), 0);
            // NULL faction: epilogue, counted
            assert_eq!(run(0, 0, 0), 2);
            assert_eq!(scratch().0, 0);
            assert_eq!(COUNTER.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn weak_stub_still_one_continuation() {
        let s = weak_stub([0x4d, 0x85, 0xc0], &[0x90], 0x1111_2222_3333_4444, 0x5555);
        let c = 0x1111_2222_3333_4444u64.to_le_bytes();
        assert_eq!(s.windows(8).filter(|w| *w == c).count(), 2);
    }

    #[test]
    fn list_stubs_execute() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let _g = RIG_LOCK.lock().unwrap();
        unsafe {
            let rig = Rig::new();
            let ctr = &COUNTER as *const AtomicU64 as usize;
            let sentinel = [0usize; 2];
            let sent = sentinel.as_ptr() as usize;
            let rax = 0x7777_0000_1110usize;
            // entry stub: exit 1 = first-node continuation, exit 2 = empty-list exit
            let entry = list_stub(&[], rig.exit_at(1), rig.exit_at(2), ctr);
            let run = rig.build(&entry, 2);
            let node = [0usize; 0x50];
            let n = node.as_ptr() as usize;
            assert_eq!(run(n, sent, rax), 1, "plausible first node walks on");
            assert_eq!(scratch(), (rax as u64, 0x5a), "rax kept (frame pointer at the entry)");
            assert_eq!(run(sent, sent, rax), 2, "empty list: vanilla exit");
            assert_eq!(COUNTER.load(Ordering::Relaxed), 0);
            for bad in [0usize, 0x18, n + 4, 0x4e1d_29c3_fca5_21a7, 0xffff_8000_0000_0000] {
                assert_eq!(run(bad, sent, rax), 2, "bad head {bad:#x} ends the walk");
                assert_eq!(scratch().0, rax as u64);
            }
            assert_eq!(COUNTER.load(Ordering::Relaxed), 5);
            // bottom stub: rbx = current node (next at +8); exit 1 = loop head, exit 2 = done
            let bottom = list_stub(&LIST_BOTTOM_BYTES[..4], rig.exit_at(1), rig.exit_at(2), ctr);
            let run = rig.build(&bottom, 2);
            let mut cur = [0usize; 2];
            cur[1] = n;
            assert_eq!(run(cur.as_ptr() as usize, sent, rax), 1, "next node walks on");
            cur[1] = sent;
            assert_eq!(run(cur.as_ptr() as usize, sent, rax), 2, "next = sentinel: done");
            assert_eq!(COUNTER.load(Ordering::Relaxed), 5);
            cur[1] = 0;
            assert_eq!(run(cur.as_ptr() as usize, sent, rax), 2, "NULL next (the crash): done");
            assert_eq!(COUNTER.load(Ordering::Relaxed), 6);
        }
    }
}
