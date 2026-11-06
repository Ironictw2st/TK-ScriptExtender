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
    s.extend_from_slice(&(cont as u64).to_le_bytes());
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

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("engine_crash_guards") {
        log!("engine crash guards off (engine_crash_guards=0 in script_extender.cfg)");
        return;
    }
    let base = t.base();
    BASE.store(base, Ordering::Relaxed);
    // SAFETY: every guard verifies its bytes first and writes only with other threads frozen.
    unsafe {
        let steps: [(&str, unsafe fn(usize) -> Result<(), String>); 5] = [
            ("inactive_string", install_inactive_string),
            ("ceo_owner", install_ceo_owner),
            ("text_font", install_font),
            ("building_browser", install_browser),
            ("weak_lock", install_weak),
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
        "inactive_string={};inactive_string_hits={};ceo_owner={};ceo_owner_hits={};text_font={};building_browser={};building_browser_hits={};weak_lock_hits={}",
        INACTIVE.get().is_some_and(|d| d.is_enabled()) as u8, INACTIVE_HITS.load(Ordering::Relaxed),
        CEO_OWNER.get().is_some_and(|d| d.is_enabled()) as u8, CEO_OWNER_HITS.load(Ordering::Relaxed),
        FONT_PATCHED.load(Ordering::Relaxed) as u8,
        (BROWSER_STUB.load(Ordering::Relaxed) != 0) as u8, BROWSER_HITS.load(Ordering::Relaxed), WEAK_HITS.load(Ordering::Relaxed),
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
}
