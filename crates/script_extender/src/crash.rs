//! In-process crash attribution (`diag_crash` in script_extender.cfg: 0 off, 1 report +
//! breadcrumbs (default), 2 also a continuous activity trace). Diagnostic only: it observes
//! faults, records names and writes files; it never touches engine memory and is not part of the
//! multiplayer version lock.
//!
//! The game's own reporter (`%APPDATA%\The Creative Assembly\ThreeKingdoms\crash_report\`) and
//! WER (`%LOCALAPPDATA%\CrashDumps`) keep writing their minidumps: the vectored exception handler
//! here answers CONTINUE_SEARCH for every real fault. What it adds is `se_crash.txt` next to the
//! DLL: the fault as module+RVA (and Ghidra address), the register set, a call stack unwound with
//! the modules' own unwind tables (RtlVirtualUnwind; a frame without unwind info is a retour
//! trampoline and is popped), a scan of the stack for return addresses, the SE scopes active on
//! the faulting thread (which hook / native was running), the last 64 SE events of all threads,
//! and the list of installed hooks. That is what ties a crash to a feature, or clears the DLL.
//!
//! A vectored handler is first-chance, so it also sees exceptions the program handles itself.
//! The DLL's own `IsBadReadPtr` probes fault inside KernelBase; C++ throws have their own code.
//! An access violation is therefore reported only when the faulting RIP is inside the exe or
//! this DLL, or when an SE scope is active on the thread and RIP is outside the system DLLs.
//! Reports are deduplicated per RIP and capped per session. A report written while the game
//! kept running was an exception the engine handled (the report says so).
//!
//! The handler path allocates nothing, takes no lock and cannot panic (`panic = "abort"` would
//! end the process): a static scratch area, a bounds-checked formatter, raw WriteFile.

use crate::log;
use crate::lua::{self, LuaState};
use core::cell::{Cell, UnsafeCell};
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------------------------
// Win32
// ---------------------------------------------------------------------------------------------

/// Opaque x64 CONTEXT (1232 bytes, 16-byte aligned). EFlags +0x44, Rax.. +0x78, Rsp +0x98, Rip +0xF8.
#[repr(C, align(16))]
pub struct Ctx(pub [u8; 1232]);
impl Ctx {
    const fn zero() -> Self { Ctx([0; 1232]) }
}

#[repr(C)]
struct ExceptionRecord { code: u32, flags: u32, record: *mut ExceptionRecord, address: *mut c_void, nparams: u32, info: [usize; 15] }
#[repr(C)]
struct ExceptionPointers { record: *mut ExceptionRecord, context: *mut Ctx }
#[repr(C)]
struct RuntimeFunction { _begin: u32, _end: u32, _unwind: u32 }
#[repr(C)]
#[derive(Default)]
struct SystemTime { year: u16, month: u16, _dow: u16, day: u16, hour: u16, minute: u16, second: u16, _ms: u16 }

extern "system" {
    fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32) -> *mut c_void;
    fn RtlLookupFunctionEntry(pc: u64, image_base: *mut u64, history: *mut c_void) -> *mut RuntimeFunction;
    fn RtlVirtualUnwind(handler_type: u32, image_base: u64, pc: u64, entry: *mut RuntimeFunction, ctx: *mut Ctx, handler_data: *mut *mut c_void, establisher: *mut u64, nv: *mut c_void) -> *mut c_void;
    fn RtlCaptureContext(ctx: *mut Ctx);
    fn RaiseException(code: u32, flags: u32, nargs: u32, args: *const usize);
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetCurrentThreadId() -> u32;
    fn GetTickCount64() -> u64;
    fn GetLocalTime(t: *mut SystemTime);
    fn CreateFileW(path: *const u16, access: u32, share: u32, sa: *mut c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void;
    fn WriteFile(h: *mut c_void, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn OutputDebugStringA(s: *const u8);
    fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
}

const CONTINUE_SEARCH: i32 = 0;
const CONTINUE_EXECUTION: i32 = -1;
const FILE_APPEND_DATA: u32 = 4;
const FILE_SHARE_READ_WRITE: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const INVALID_HANDLE: usize = usize::MAX;

const OFF_EFLAGS: usize = 0x44;
const OFF_RAX: usize = 0x78;
const OFF_RSP: usize = 0x98;
const OFF_RIP: usize = 0xF8;

const AV: u32 = 0xC000_0005;
const IN_PAGE: u32 = 0xC000_0006;
const STACK_OVERFLOW: u32 = 0xC000_00FD;
const HEAP_CORRUPTION: u32 = 0xC000_0374;
/// Private codes of `se_crash_selftest`: 1 = report written directly, 2 = raised through the VEH.
const TEST_DIRECT: u32 = 0xE000_5E01;
const TEST_VEH: u32 = 0xE000_5E02;

fn fatal(code: u32) -> bool {
    matches!(code, AV | IN_PAGE | 0xC000_001D | STACK_OVERFLOW | 0xC000_0094 | 0xC000_0096 | 0xC000_008C | 0xC000_0409 | HEAP_CORRUPTION)
}

fn code_name(code: u32) -> &'static str {
    match code {
        AV => "ACCESS_VIOLATION",
        IN_PAGE => "IN_PAGE_ERROR",
        0xC000_001D => "ILLEGAL_INSTRUCTION",
        STACK_OVERFLOW => "STACK_OVERFLOW",
        0xC000_0094 => "INTEGER_DIVIDE_BY_ZERO",
        0xC000_0096 => "PRIVILEGED_INSTRUCTION",
        0xC000_008C => "ARRAY_BOUNDS_EXCEEDED",
        0xC000_0409 => "STACK_BUFFER_OVERRUN",
        HEAP_CORRUPTION => "HEAP_CORRUPTION",
        TEST_DIRECT => "SELFTEST_DIRECT",
        TEST_VEH => "SELFTEST_VEH",
        _ => "?",
    }
}

// ---------------------------------------------------------------------------------------------
// State (all static, fixed size)
// ---------------------------------------------------------------------------------------------

struct SyncCell<T>(UnsafeCell<T>);
// SAFETY: every SyncCell is written once on the bootstrap thread before the handler exists, or
// holds plain bytes whose torn reads are tolerated (marks) and re-validated before use.
unsafe impl<T> Sync for SyncCell<T> {}
impl<T> SyncCell<T> {
    const fn new(v: T) -> Self { SyncCell(UnsafeCell::new(v)) }
    fn get(&self) -> *mut T { self.0.get() }
}

#[derive(Clone, Copy)]
struct Range { base: usize, size: usize }
impl Range {
    const NONE: Range = Range { base: 0, size: 0 };
    fn has(&self, a: usize) -> bool { self.size != 0 && a >= self.base && a < self.base.wrapping_add(self.size) }
}
pub(crate) struct Ranges { exe: Range, dll: Range, sys: [Range; 3] }
const SYS_NAMES: [&str; 3] = ["ntdll.dll", "kernelbase.dll", "kernel32.dll"];
static RANGES: SyncCell<Ranges> = SyncCell::new(Ranges { exe: Range::NONE, dll: Range::NONE, sys: [Range::NONE; 3] });

const BUF_BYTES: usize = 32768;
struct Work { ctx: Ctx, buf: [u8; BUF_BYTES], ods: [u8; 4096] }
/// The handler's scratch: static, not on the stack (a stack overflow leaves almost none).
static WORK: SyncCell<Work> = SyncCell::new(Work { ctx: Ctx::zero(), buf: [0; BUF_BYTES], ods: [0; 4096] });
static CRASH_PATH: SyncCell<[u16; 320]> = SyncCell::new([0; 320]);
static CFG_SUMMARY: SyncCell<[u8; 1024]> = SyncCell::new([0; 1024]);
static CFG_LEN: AtomicUsize = AtomicUsize::new(0);

static LEVEL: AtomicU8 = AtomicU8::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static BOOT_TID: AtomicU32 = AtomicU32::new(0);
static LUA_TIDS: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static START_TICK: AtomicU64 = AtomicU64::new(0);
static REPORTS: AtomicU32 = AtomicU32::new(0);
static REPEATS: AtomicU32 = AtomicU32::new(0);
static LAST_CODE: AtomicU32 = AtomicU32::new(0);
static LAST_RVA: AtomicUsize = AtomicUsize::new(0);
/// Thread id of the thread inside the handler (0 = free). A fault raised by the handler's own
/// probes on that thread is ignored; a second thread faulting meanwhile is counted, not reported.
static OWNER: AtomicU32 = AtomicU32::new(0);
static SEEN_RIP: [AtomicUsize; 16] = [const { AtomicUsize::new(0) }; 16];
static SEEN_N: AtomicUsize = AtomicUsize::new(0);
const MAX_REPORTS: u32 = 8;

// breadcrumbs -------------------------------------------------------------------------------

pub const KIND_HOOK: u8 = 1;
pub const KIND_HOOK_EXIT: u8 = 2;
pub const KIND_NATIVE: u8 = 3;
pub const KIND_NATIVE_EXIT: u8 = 4;
pub const KIND_MARK: u8 = 5;
pub const KIND_BOOT: u8 = 6;

fn kind_name(k: u8) -> &'static str {
    match k { KIND_HOOK => "hook", KIND_HOOK_EXIT => "hook-exit", KIND_NATIVE => "native", KIND_NATIVE_EXIT => "native-exit", KIND_MARK => "mark", KIND_BOOT => "boot", _ => "?" }
}

const SCOPE_DEPTH: usize = 8;
#[derive(Clone, Copy)]
struct ScopeStack { depth: u8, names: [(usize, usize, u8); SCOPE_DEPTH] }
thread_local! {
    static STACK: Cell<ScopeStack> = const { Cell::new(ScopeStack { depth: 0, names: [(0, 0, 0); SCOPE_DEPTH] }) };
}
static SCOPE_OVERFLOW: AtomicU32 = AtomicU32::new(0);

struct Crumb { tick: AtomicU32, tid: AtomicU32, kind: AtomicU8, ptr: AtomicUsize, len: AtomicUsize }
impl Crumb {
    const fn new() -> Self { Crumb { tick: AtomicU32::new(0), tid: AtomicU32::new(0), kind: AtomicU8::new(0), ptr: AtomicUsize::new(0), len: AtomicUsize::new(0) } }
}
const RING_N: usize = 64;
static RING: [Crumb; RING_N] = [const { Crumb::new() }; RING_N];
static RING_CUR: AtomicUsize = AtomicUsize::new(0);

const MARK_BYTES: usize = 64;
const MARK_N: usize = 16;
static MARKS: SyncCell<[[u8; MARK_BYTES]; MARK_N]> = SyncCell::new([[0; MARK_BYTES]; MARK_N]);
static MARK_CUR: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct HookRec { name: (usize, usize), key: (usize, usize), target: usize }
const HOOK_MAX: usize = 48;
static HOOKS: SyncCell<[HookRec; HOOK_MAX]> = SyncCell::new([HookRec { name: (0, 0), key: (0, 0), target: 0 }; HOOK_MAX]);
static HOOK_N: AtomicUsize = AtomicUsize::new(0);

// level 2: every crumb also goes into a large ring drained by a writer thread -----------------

struct Ev { seq: AtomicU64, tick: AtomicU32, tid: AtomicU32, kind: AtomicU8, ptr: AtomicUsize, len: AtomicUsize }
impl Ev {
    const fn new() -> Self { Ev { seq: AtomicU64::new(0), tick: AtomicU32::new(0), tid: AtomicU32::new(0), kind: AtomicU8::new(0), ptr: AtomicUsize::new(0), len: AtomicUsize::new(0) } }
}
const EV_N: usize = 16384;
static EVENTS: [Ev; EV_N] = [const { Ev::new() }; EV_N];
static EV_HEAD: AtomicU64 = AtomicU64::new(0);
static EV_DROPPED: AtomicU64 = AtomicU64::new(0);
const ACTIVITY_MAX_BYTES: u64 = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Public API used by the rest of the DLL
// ---------------------------------------------------------------------------------------------

/// RAII breadcrumb: `let _g = crate::crash::enter("ar_compute_results");` at the top of a detour.
pub struct Scope { name: (usize, usize), kind: u8 }

#[inline]
pub fn enter(name: &'static str) -> Scope { enter_kind(name, KIND_HOOK) }

/// The same for a Lua native (the shim in lua.rs).
#[inline]
pub fn enter_native(name: &'static str) -> Scope { enter_kind(name, KIND_NATIVE) }

fn enter_kind(name: &'static str, kind: u8) -> Scope {
    let inactive = Scope { name: (0, 0), kind: 0 };
    if LEVEL.load(Ordering::Relaxed) == 0 { return inactive; }
    let entry = (name.as_ptr() as usize, name.len(), kind);
    let pushed = STACK.try_with(|s| {
        let mut st = s.get();
        let d = st.depth as usize;
        match st.names.get_mut(d) {
            Some(slot) => { *slot = entry; st.depth += 1; s.set(st); true }
            None => false,
        }
    }).unwrap_or(false);
    if !pushed {
        SCOPE_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        return inactive;
    }
    push_crumb(kind, entry.0, entry.1);
    Scope { name: (entry.0, entry.1), kind }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if self.kind == 0 { return; }
        let _ = STACK.try_with(|s| {
            let mut st = s.get();
            if st.depth > 0 { st.depth -= 1; s.set(st); }
        });
        push_crumb(self.kind + 1, self.name.0, self.name.1);
    }
}

/// A bootstrap phase marker (no scope).
pub fn boot(name: &'static str) {
    if LEVEL.load(Ordering::Relaxed) == 0 { return; }
    push_crumb(KIND_BOOT, name.as_ptr() as usize, name.len());
}

/// A script-side marker (`se.crash.mark`): the text is copied into a static slot.
pub fn mark(text: &[u8]) {
    if LEVEL.load(Ordering::Relaxed) == 0 { return; }
    let i = MARK_CUR.fetch_add(1, Ordering::Relaxed) % MARK_N;
    let n = text.len().min(MARK_BYTES - 1);
    // SAFETY: static storage; a concurrent writer of the same slot only produces torn text.
    let (ptr, len) = unsafe {
        let marks = &mut *MARKS.get();
        let Some(slot) = marks.get_mut(i) else { return };
        if let (Some(dst), Some(src)) = (slot.get_mut(..n), text.get(..n)) { dst.copy_from_slice(src); }
        if let Some(z) = slot.get_mut(n) { *z = 0; }
        (slot.as_ptr() as usize, n)
    };
    push_crumb(KIND_MARK, ptr, len);
}

/// Registry of installed detours, printed in every report: addrs.rs name, cfg key ("-" = always), target.
pub fn hook_installed(name: &'static str, cfg_key: &'static str, target: usize) {
    let i = HOOK_N.load(Ordering::Relaxed);
    // SAFETY: only the bootstrap thread installs hooks, sequentially.
    unsafe {
        let hooks = &mut *HOOKS.get();
        if let Some(slot) = hooks.get_mut(i) {
            *slot = HookRec { name: (name.as_ptr() as usize, name.len()), key: (cfg_key.as_ptr() as usize, cfg_key.len()), target };
            HOOK_N.store(i + 1, Ordering::Release);
        }
    }
}

/// hook.rs: the thread that registered natives into a Lua state (the report says "lua-thread").
pub fn note_lua_thread() {
    let me = unsafe { GetCurrentThreadId() };
    for slot in LUA_TIDS.iter() {
        let v = slot.load(Ordering::Relaxed);
        if v == me { return; }
        if v == 0 && slot.compare_exchange(0, me, Ordering::Relaxed, Ordering::Relaxed).is_ok() { return; }
    }
}

fn tick_now() -> u32 {
    let t = unsafe { GetTickCount64() }.wrapping_sub(START_TICK.load(Ordering::Relaxed));
    (t as u32).wrapping_add(1).max(1)
}

fn push_crumb(kind: u8, ptr: usize, len: usize) {
    let tid = unsafe { GetCurrentThreadId() };
    let tick = tick_now();
    let i = RING_CUR.fetch_add(1, Ordering::Relaxed) % RING_N;
    if let Some(c) = RING.get(i) {
        c.tick.store(0, Ordering::Relaxed);
        c.tid.store(tid, Ordering::Relaxed);
        c.kind.store(kind, Ordering::Relaxed);
        c.ptr.store(ptr, Ordering::Relaxed);
        c.len.store(len, Ordering::Relaxed);
        c.tick.store(tick, Ordering::Release);
    }
    if LEVEL.load(Ordering::Relaxed) >= 2 {
        let s = EV_HEAD.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = EVENTS.get((s % EV_N as u64) as usize) {
            e.seq.store(0, Ordering::Relaxed);
            e.tick.store(tick, Ordering::Relaxed);
            e.tid.store(tid, Ordering::Relaxed);
            e.kind.store(kind, Ordering::Relaxed);
            e.ptr.store(ptr, Ordering::Relaxed);
            e.len.store(len, Ordering::Relaxed);
            e.seq.store(s + 1, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------------------------

pub fn install() {
    START_TICK.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
    let level = crate::build::config_value("diag_crash").and_then(|v| v.parse::<u8>().ok()).unwrap_or(1).min(2);
    if level == 0 {
        log!("crash reporter off (diag_crash=0)");
        return;
    }
    let Some(dir) = crate::process::self_dir() else {
        log!("crash reporter: own folder unknown; off");
        return;
    };
    let path = dir.join("se_crash.txt");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(core::iter::once(0)).collect();
    // SAFETY: bootstrap thread, before the handler exists.
    unsafe {
        let dst = &mut *CRASH_PATH.get();
        if wide.len() > dst.len() {
            log!("crash reporter: path too long ({}); off", path.display());
            return;
        }
        if let Some(d) = dst.get_mut(..wide.len()) { d.copy_from_slice(&wide); }
        let (exe_base, exe_size) = crate::process::main_module();
        let dll_base = crate::self_hmodule() as usize;
        let r = &mut *RANGES.get();
        r.exe = Range { base: exe_base, size: exe_size };
        r.dll = Range { base: dll_base, size: crate::process::image_size(dll_base) };
        for (i, name) in SYS_NAMES.iter().enumerate() {
            let w: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();
            let base = GetModuleHandleW(w.as_ptr()) as usize;
            if let Some(slot) = r.sys.get_mut(i) { *slot = Range { base, size: if base != 0 { crate::process::image_size(base) } else { 0 } }; }
        }
        let mut summary = String::new();
        for key in crate::build::KNOWN_KEYS {
            if key.starts_with("build_") { continue; }
            summary.push_str(key);
            summary.push('=');
            summary.push_str(&crate::build::config_value(key).unwrap_or_else(|| "default".into()));
            summary.push(';');
        }
        let cfg = &mut *CFG_SUMMARY.get();
        let n = summary.len().min(cfg.len());
        if let (Some(d), Some(s)) = (cfg.get_mut(..n), summary.as_bytes().get(..n)) { d.copy_from_slice(s); }
        CFG_LEN.store(n, Ordering::Relaxed);
        BOOT_TID.store(GetCurrentThreadId(), Ordering::Relaxed);
        LEVEL.store(level, Ordering::Relaxed);
        if AddVectoredExceptionHandler(1, veh).is_null() {
            LEVEL.store(0, Ordering::Relaxed);
            log!("crash reporter: AddVectoredExceptionHandler failed; off");
            return;
        }
    }
    INSTALLED.store(true, Ordering::Release);
    if level >= 2 {
        std::thread::spawn(activity_writer);
    }
    log!("crash reporter installed (diag_crash={level}): {}{}", path.display(), if level >= 2 { " + se_activity.txt" } else { "" });
    boot("crash");
}

use std::os::windows::ffi::OsStrExt;

// ---------------------------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------------------------

unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }

fn ctx_get(ctx: &Ctx, off: usize) -> u64 {
    ctx.0.get(off..off + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).unwrap_or(0)
}
fn ctx_get32(ctx: &Ctx, off: usize) -> u32 {
    ctx.0.get(off..off + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).unwrap_or(0)
}
fn ctx_set(ctx: &mut Ctx, off: usize, v: u64) {
    if let Some(b) = ctx.0.get_mut(off..off + 8) { b.copy_from_slice(&v.to_le_bytes()); }
}

/// Current thread's stack (limit, base) from the TEB: no API call, no allocation.
unsafe fn stack_bounds() -> (usize, usize) {
    let hi: usize;
    let lo: usize;
    core::arch::asm!("mov {}, qword ptr gs:[0x08]", out(reg) hi, options(nostack, readonly, preserves_flags));
    core::arch::asm!("mov {}, qword ptr gs:[0x10]", out(reg) lo, options(nostack, readonly, preserves_flags));
    (lo, hi)
}

fn wanted(rip: usize, depth: u8) -> bool {
    // SAFETY: RANGES is written once before the handler is installed.
    let r = unsafe { &*RANGES.get() };
    if r.exe.has(rip) || r.dll.has(rip) { return true; }
    depth > 0 && !r.sys.iter().any(|s| s.has(rip))
}

/// Per-RIP dedup and the session cap.
fn fresh(rip: usize) -> bool {
    if REPORTS.load(Ordering::Relaxed) >= MAX_REPORTS { return false; }
    let n = SEEN_N.load(Ordering::Relaxed).min(SEEN_RIP.len());
    if SEEN_RIP.iter().take(n).any(|s| s.load(Ordering::Relaxed) == rip) { return false; }
    if let Some(slot) = SEEN_RIP.get(n) {
        slot.store(rip, Ordering::Relaxed);
        SEEN_N.store(n + 1, Ordering::Relaxed);
    }
    true
}

unsafe extern "system" fn veh(ep: *mut ExceptionPointers) -> i32 {
    if ep.is_null() { return CONTINUE_SEARCH; }
    let (rec, ctx) = ((*ep).record, (*ep).context);
    if rec.is_null() || ctx.is_null() { return CONTINUE_SEARCH; }
    let code = (*rec).code;
    let test = code == TEST_VEH;
    if !test && !fatal(code) { return CONTINUE_SEARCH; }
    let me = GetCurrentThreadId();
    if OWNER.load(Ordering::Acquire) == me { return CONTINUE_SEARCH; }
    let rip = ctx_get(&*ctx, OFF_RIP) as usize;
    if !test {
        let depth = STACK.try_with(|s| s.get().depth).unwrap_or(0);
        if !wanted(rip, depth) { return CONTINUE_SEARCH; }
        if !fresh(rip) {
            REPEATS.fetch_add(1, Ordering::Relaxed);
            return CONTINUE_SEARCH;
        }
    }
    if OWNER.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        write_report(&*rec, &*ctx, if test { "SELFTEST via VEH" } else { "FAULT" });
        OWNER.store(0, Ordering::Release);
    }
    // The self-test exception is ours: dismissed here, RaiseException returns normally.
    if test { CONTINUE_EXECUTION } else { CONTINUE_SEARCH }
}

unsafe fn write_report(rec: &ExceptionRecord, ctx: &Ctx, tag: &str) {
    let n = REPORTS.fetch_add(1, Ordering::Relaxed) + 1;
    LAST_CODE.store(rec.code, Ordering::Relaxed);
    let r = &*RANGES.get();
    let rip = ctx_get(ctx, OFF_RIP) as usize;
    LAST_RVA.store(if r.exe.has(rip) { rip - r.exe.base } else { 0 }, Ordering::Relaxed);
    let me = GetCurrentThreadId();
    let mut t = SystemTime::default();
    GetLocalTime(&mut t);
    let work = &mut *WORK.get();
    let Work { ctx: wctx, buf, ods } = work;
    wctx.0.copy_from_slice(&ctx.0);
    let mut w = W::new(buf);

    w.s("==== script_extender ").s(env!("CARGO_PKG_VERSION")).s(" crash report #").dec(n as u64).s(" (").s(tag).s(") ")
        .dec(t.year as u64).c(b'-').dec2(t.month).c(b'-').dec2(t.day).c(b' ').dec2(t.hour).c(b':').dec2(t.minute).c(b':').dec2(t.second)
        .s(" uptime ").dec(tick_now() as u64).s(" ms ====").nl();
    w.s("exception ").hex(rec.code as u64).c(b' ').s(code_name(rec.code)).s(" at ").addr(rip, r).nl();
    if rec.address as usize != rip { w.s("  record address ").addr(rec.address as usize, r).nl(); }
    if (rec.code == AV || rec.code == IN_PAGE) && rec.nparams >= 2 {
        let (kind, target) = (rec.info.first().copied().unwrap_or(0), rec.info.get(1).copied().unwrap_or(0));
        w.s("  ").s(match kind { 0 => "read from", 1 => "write to", 8 => "execute at", _ => "access to" }).c(b' ').hex(target as u64);
        if target < 0x10000 { w.s(" (null-based)"); }
        w.nl();
    }
    let boot_tid = BOOT_TID.load(Ordering::Relaxed);
    let lua_thread = LUA_TIDS.iter().any(|s| s.load(Ordering::Relaxed) == me);
    w.s("thread ").hex(me as u64).s(if me == boot_tid { " bootstrap-thread=yes" } else { " bootstrap-thread=no" }).s(if lua_thread { " lua-thread=yes" } else { " lua-thread=no" }).nl();
    w.s("exe base ").hex(r.exe.base as u64).s(" size ").hex(r.exe.size as u64).s("  dll base ").hex(r.dll.base as u64).s(" size ").hex(r.dll.size as u64).nl();
    w.s("cfg ");
    let cfg = &*CFG_SUMMARY.get();
    if let Some(bytes) = cfg.get(..CFG_LEN.load(Ordering::Relaxed).min(cfg.len())) { w.raw(bytes); }
    w.nl();

    w.s("registers").nl();
    const NAMES: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8 ", "r9 ", "r10", "r11", "r12", "r13", "r14", "r15"];
    for (i, name) in NAMES.iter().enumerate() {
        if i % 4 == 0 { w.s("  "); }
        w.s(name).c(b'=').hex16(ctx_get(ctx, OFF_RAX + i * 8)).s(if i % 4 == 3 { "\n" } else { "  " });
    }
    w.s("  rip=").hex16(ctx_get(ctx, OFF_RIP)).s("  eflags=").hex(ctx_get32(ctx, OFF_EFLAGS) as u64).nl();

    let (stack_lo, stack_hi) = stack_bounds();
    w.s("unwound stack (RtlVirtualUnwind; '~' = no unwind info, return address popped; innermost first)").nl();
    unwind(&mut w, wctx, r, stack_lo, stack_hi);

    w.s("stack scan (first 4 KB above rsp: values inside the exe / dll that follow a call)").nl();
    let rsp = ctx_get(ctx, OFF_RSP) as usize;
    if rsp >= stack_lo && rsp < stack_hi {
        let end = rsp.saturating_add(4096).min(stack_hi);
        let (mut a, mut hits) = (rsp & !7, 0);
        while a + 8 <= end && hits < 48 {
            let v = *(a as *const usize);
            let inside = (r.exe.has(v) && v >= r.exe.base + 16) || (r.dll.has(v) && v >= r.dll.base + 16);
            if inside && crate::profiler::follows_call(v) {
                w.s("  [rsp+").hex((a - rsp) as u64).s("] ").addr(v, r).nl();
                hits += 1;
            }
            a += 8;
        }
    } else {
        w.s("  rsp outside the thread's stack").nl();
    }

    let st = STACK.try_with(|s| s.get()).unwrap_or(ScopeStack { depth: 0, names: [(0, 0, 0); SCOPE_DEPTH] });
    w.s("se scopes on this thread (innermost first, depth ").dec(st.depth as u64).s(", refused at depth limit ").dec(SCOPE_OVERFLOW.load(Ordering::Relaxed) as u64).c(b')').nl();
    let depth = (st.depth as usize).min(SCOPE_DEPTH);
    for i in (0..depth).rev() {
        if let Some((p, l, k)) = st.names.get(i) { w.s("  ").s(kind_name(*k)).c(b' ').name(*p, *l, r).nl(); }
    }

    w.s("last ").dec(RING_N as u64).s(" se events (all threads, newest first): tick_ms thread kind name").nl();
    let cur = RING_CUR.load(Ordering::Relaxed);
    for i in 0..RING_N {
        let Some(c) = RING.get(cur.wrapping_sub(1).wrapping_sub(i) % RING_N) else { continue };
        let tick = c.tick.load(Ordering::Acquire);
        if tick == 0 { continue; }
        w.s("  ").dec(tick as u64).c(b' ').hex(c.tid.load(Ordering::Relaxed) as u64).c(b' ').s(kind_name(c.kind.load(Ordering::Relaxed))).c(b' ')
            .name(c.ptr.load(Ordering::Relaxed), c.len.load(Ordering::Relaxed), r).nl();
    }

    let hn = HOOK_N.load(Ordering::Acquire).min(HOOK_MAX);
    w.s("installed hooks (").dec(hn as u64).s("): name cfg_key target").nl();
    let hooks = &*HOOKS.get();
    for h in hooks.iter().take(hn) {
        w.s("  ").name(h.name.0, h.name.1, r).c(b' ').name(h.key.0, h.key.1, r).c(b' ').addr(h.target, r).nl();
    }
    w.s("natives registered ").dec(lua::native_count() as u64).s("; reports ").dec(n as u64).s("; repeats suppressed ").dec(REPEATS.load(Ordering::Relaxed) as u64)
        .s("; activity events dropped ").dec(EV_DROPPED.load(Ordering::Relaxed)).nl();
    if !tag.starts_with("SELFTEST") {
        w.s("note: first-chance report. If the game kept running, the engine handled this exception itself.").nl();
    }
    w.s("==== end ====").nl().nl();

    let len = w.len();
    let h = CreateFileW((*CRASH_PATH.get()).as_ptr(), FILE_APPEND_DATA, FILE_SHARE_READ_WRITE, core::ptr::null_mut(), OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, core::ptr::null_mut());
    if h as usize != INVALID_HANDLE && !h.is_null() {
        let mut written = 0u32;
        WriteFile(h, buf.as_ptr(), len as u32, &mut written, core::ptr::null_mut());
        CloseHandle(h);
    }
    if rec.code != STACK_OVERFLOW {
        // DebugView / x64dbg mirror, in chunks the 4 KB DBWIN buffer accepts, split at line ends
        let mut at = 0usize;
        while at < len {
            let mut end = (at + 3900).min(len);
            if end < len {
                if let Some(chunk) = buf.get(at..end) {
                    if let Some(p) = chunk.iter().rposition(|b| *b == b'\n') { end = at + p + 1; }
                }
            }
            let k = end - at;
            if let (Some(dst), Some(src)) = (ods.get_mut(..k), buf.get(at..end)) { dst.copy_from_slice(src); }
            if let Some(z) = ods.get_mut(k) { *z = 0; }
            OutputDebugStringA(ods.as_ptr());
            at = end;
        }
        if rec.code != HEAP_CORRUPTION {
            crate::logging::try_line("crash report written to se_crash.txt");
        }
    }
}

/// Frames from the CONTEXT in `ctx` (modified in place), at most 48.
unsafe fn unwind(w: &mut W, ctx: &mut Ctx, r: &Ranges, stack_lo: usize, stack_hi: usize) {
    let mut frames = 0u64;
    while frames < 48 {
        let rip = ctx_get(ctx, OFF_RIP) as usize;
        let rsp = ctx_get(ctx, OFF_RSP) as usize;
        if rip == 0 { break; }
        w.s("  #").dec(frames).c(b' ');
        if !readable(rip, 1) {
            w.s("(unreadable) ").hex(rip as u64).nl();
            break;
        }
        let mut base: u64 = 0;
        let entry = RtlLookupFunctionEntry(rip as u64, &mut base, core::ptr::null_mut());
        if entry.is_null() {
            w.s("~ ").addr(rip, r).nl();
            if rsp < stack_lo || rsp + 8 > stack_hi { break; }
            let ret = *(rsp as *const usize);
            ctx_set(ctx, OFF_RIP, ret as u64);
            ctx_set(ctx, OFF_RSP, (rsp + 8) as u64);
        } else {
            w.addr(rip, r).nl();
            let mut hd: *mut c_void = core::ptr::null_mut();
            let mut est: u64 = 0;
            RtlVirtualUnwind(0, base, rip as u64, entry, ctx, &mut hd, &mut est, core::ptr::null_mut());
            let nrsp = ctx_get(ctx, OFF_RSP) as usize;
            if nrsp <= rsp || nrsp < stack_lo || nrsp > stack_hi { break; }
        }
        frames += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// The no-alloc formatter
// ---------------------------------------------------------------------------------------------

pub(crate) struct W<'a> { b: &'a mut [u8], n: usize }

impl<'a> W<'a> {
    pub(crate) fn new(b: &'a mut [u8]) -> Self { W { b, n: 0 } }
    pub(crate) fn len(&self) -> usize { self.n }
    #[allow(dead_code)]
    pub(crate) fn as_slice(&self) -> &[u8] { self.b.get(..self.n).unwrap_or(&[]) }

    pub(crate) fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        let room = self.b.len().saturating_sub(self.n);
        let k = bytes.len().min(room);
        if let (Some(dst), Some(src)) = (self.b.get_mut(self.n..self.n + k), bytes.get(..k)) {
            dst.copy_from_slice(src);
            self.n += k;
        }
        self
    }
    pub(crate) fn s(&mut self, s: &str) -> &mut Self { self.raw(s.as_bytes()) }
    pub(crate) fn c(&mut self, c: u8) -> &mut Self { self.raw(&[c]) }
    pub(crate) fn nl(&mut self) -> &mut Self { self.c(b'\n') }

    fn hex_digits(&mut self, mut v: u64, min_digits: usize) -> &mut Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut tmp = [b'0'; 16];
        let mut i = 16usize;
        loop {
            i -= 1;
            if let Some(d) = tmp.get_mut(i) { *d = HEX.get((v & 0xf) as usize).copied().unwrap_or(b'?'); }
            v >>= 4;
            if v == 0 || i == 0 { break; }
        }
        let start = i.min(16usize.saturating_sub(min_digits));
        let digits = tmp.get(start..).unwrap_or(&[]);
        self.raw(digits)
    }
    pub(crate) fn hex(&mut self, v: u64) -> &mut Self { self.s("0x"); self.hex_digits(v, 1) }
    pub(crate) fn hex16(&mut self, v: u64) -> &mut Self { self.hex_digits(v, 16) }
    pub(crate) fn dec(&mut self, mut v: u64) -> &mut Self {
        let mut tmp = [b'0'; 20];
        let mut i = 20usize;
        loop {
            i -= 1;
            if let Some(d) = tmp.get_mut(i) { *d = b'0' + (v % 10) as u8; }
            v /= 10;
            if v == 0 || i == 0 { break; }
        }
        let digits = tmp.get(i..).unwrap_or(&[]);
        self.raw(digits)
    }
    fn dec2(&mut self, v: u16) -> &mut Self {
        if v < 10 { self.c(b'0'); }
        self.dec(v as u64)
    }
    /// `Three_Kingdoms.exe+0x.. (ghidra 0x14..)` | `script_extender.dll+0x..` | `ntdll.dll+0x..` | raw.
    pub(crate) fn addr(&mut self, a: usize, r: &Ranges) -> &mut Self {
        if r.exe.has(a) {
            let rva = a - r.exe.base;
            return self.s("Three_Kingdoms.exe+").hex(rva as u64).s(" (ghidra ").hex(rva as u64 + 0x1_4000_0000).c(b')');
        }
        if r.dll.has(a) { return self.s("script_extender.dll+").hex((a - r.dll.base) as u64); }
        for (i, s) in r.sys.iter().enumerate() {
            if s.has(a) { return self.s(SYS_NAMES.get(i).copied().unwrap_or("?")).c(b'+').hex((a - s.base) as u64); }
        }
        self.hex(a as u64)
    }
    /// A crumb name: printed only when it points into this DLL's image (string literals) or the
    /// marks area, and is at most 64 bytes.
    pub(crate) fn name(&mut self, ptr: usize, len: usize, r: &Ranges) -> &mut Self {
        if len == 0 || len > MARK_BYTES { return self.s("?"); }
        let marks = MARKS.get() as usize;
        let in_marks = ptr >= marks && ptr + len <= marks + MARK_BYTES * MARK_N;
        let in_dll = r.dll.has(ptr) && r.dll.has(ptr + len - 1);
        if !in_marks && !in_dll { return self.s("?"); }
        // SAFETY: the range was just checked against static storage / the loaded image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        self.raw(bytes)
    }
}

// ---------------------------------------------------------------------------------------------
// Level 2: the activity writer thread (ordinary std I/O; the producers never wait for it)
// ---------------------------------------------------------------------------------------------

fn crumb_text(ptr: usize, len: usize) -> String {
    let r = unsafe { &*RANGES.get() };
    let marks = MARKS.get() as usize;
    let ok = len > 0 && len <= MARK_BYTES && ((ptr >= marks && ptr + len <= marks + MARK_BYTES * MARK_N) || (r.dll.has(ptr) && r.dll.has(ptr + len - 1)));
    if !ok { return "?".into(); }
    // SAFETY: checked above.
    String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) }).into_owned()
}

fn activity_writer() {
    use std::io::Write as _;
    let Some(dir) = crate::process::self_dir() else { return };
    let path = dir.join("se_activity.txt");
    let mut tail: u64 = 0;
    let mut out = String::new();
    let _ = std::fs::write(&path, format!("# se_activity {} (tick_ms thread kind name)\n", env!("CARGO_PKG_VERSION")));
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let head = EV_HEAD.load(Ordering::Acquire);
        if head == tail { continue; }
        out.clear();
        if head - tail > EV_N as u64 {
            let lost = head - tail - EV_N as u64;
            EV_DROPPED.fetch_add(lost, Ordering::Relaxed);
            tail = head - EV_N as u64;
            out.push_str(&format!("# dropped {lost}\n"));
        }
        while tail < head {
            if let Some(e) = EVENTS.get((tail % EV_N as u64) as usize) {
                if e.seq.load(Ordering::Acquire) == tail + 1 {
                    let (tick, tid, kind, ptr, len) = (e.tick.load(Ordering::Relaxed), e.tid.load(Ordering::Relaxed), e.kind.load(Ordering::Relaxed), e.ptr.load(Ordering::Relaxed), e.len.load(Ordering::Relaxed));
                    if e.seq.load(Ordering::Acquire) == tail + 1 {
                        out.push_str(&format!("{tick} {tid:x} {} {}\n", kind_name(kind), crumb_text(ptr, len)));
                    }
                }
            }
            tail += 1;
        }
        let rotate = std::fs::metadata(&path).map(|m| m.len() > ACTIVITY_MAX_BYTES).unwrap_or(false);
        let file = if rotate {
            std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&path).map(|mut f| { let _ = f.write_all(b"# rotated\n"); f })
        } else {
            std::fs::OpenOptions::new().create(true).append(true).open(&path)
        };
        if let Ok(mut f) = file { let _ = f.write_all(out.as_bytes()); }
    }
}

// ---------------------------------------------------------------------------------------------
// Lua natives
// ---------------------------------------------------------------------------------------------

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_crash_mark", se_crash_mark);
    lua::set_global_fn(l, "se_crash_info", se_crash_info);
    lua::set_global_fn(l, "se_crash_selftest", se_crash_selftest);
}

/// se_crash_mark(text) -> ok, message : a script-side breadcrumb (at most 63 bytes kept).
unsafe extern "C" fn se_crash_mark(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let text = lua::to_str(l, 1);
    let on = LEVEL.load(Ordering::Relaxed) != 0;
    if on { mark(text.as_bytes()); }
    (api.pushboolean)(l, on as c_int);
    lua::push_str(l, if on { "marked" } else { "crash reporter off (diag_crash=0)" });
    2
}

/// se_crash_info() -> "k=v;..." (installed, level, natives, hooks, reports, repeats, dropped, last_code, last_rva, scope_overflow)
unsafe extern "C" fn se_crash_info(l: *mut LuaState) -> c_int {
    let s = format!("installed={};level={};natives={};hooks={};reports={};repeats={};dropped={};last_code={:#x};last_rva={:#x};scope_overflow={}",
        INSTALLED.load(Ordering::Relaxed) as u8, LEVEL.load(Ordering::Relaxed), lua::native_count(), HOOK_N.load(Ordering::Relaxed),
        REPORTS.load(Ordering::Relaxed), REPEATS.load(Ordering::Relaxed), EV_DROPPED.load(Ordering::Relaxed),
        LAST_CODE.load(Ordering::Relaxed), LAST_RVA.load(Ordering::Relaxed), SCOPE_OVERFLOW.load(Ordering::Relaxed));
    lua::push_str(l, &s);
    1
}

/// se_crash_selftest([mode]) -> ok, message. mode 1 (default): a report of the current context
/// is written directly; mode 2: a private exception is raised and travels through the vectored
/// handler, which dismisses it (the call returns normally). Neither faults. Mode 2 pauses the game
/// in an attached debugger (first-chance exception).
unsafe extern "C" fn se_crash_selftest(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let mode = (api.tointeger)(l, 1);
    let mode = if mode == 0 { 1 } else { mode };
    let before = REPORTS.load(Ordering::Relaxed);
    let (ok, msg) = if !INSTALLED.load(Ordering::Acquire) {
        (false, "crash reporter not installed (diag_crash=0)".to_string())
    } else if mode == 2 {
        RaiseException(TEST_VEH, 0, 0, core::ptr::null());
        let after = REPORTS.load(Ordering::Relaxed);
        (after > before, format!("report #{after} written through the exception handler"))
    } else {
        let me = GetCurrentThreadId();
        if OWNER.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            (false, "another thread is writing a report".to_string())
        } else {
            let mut ctx = Ctx::zero();
            RtlCaptureContext(&mut ctx);
            let rec = ExceptionRecord { code: TEST_DIRECT, flags: 0, record: core::ptr::null_mut(), address: ctx_get(&ctx, OFF_RIP) as *mut c_void, nparams: 0, info: [0; 15] };
            write_report(&rec, &ctx, "SELFTEST direct");
            OWNER.store(0, Ordering::Release);
            (true, format!("report #{} written to se_crash.txt", REPORTS.load(Ordering::Relaxed)))
        }
    };
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &msg);
    2
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges() -> Ranges {
        Ranges { exe: Range { base: 0x7ff7_0000_0000, size: 0x100 }, dll: Range { base: 0x7ff8_0000_0000, size: 0x100 }, sys: [Range { base: 0x7ffa_0000_0000, size: 0x10 }, Range::NONE, Range::NONE] }
    }

    #[test]
    fn hex_and_dec() {
        let mut b = [0u8; 64];
        let mut w = W::new(&mut b);
        w.hex(0).c(b' ').hex(0x1f).c(b' ').hex16(0xabc).c(b' ').dec(0).c(b' ').dec(1234567).c(b' ').dec2(7);
        assert_eq!(w.as_slice(), b"0x0 0x1f 0000000000000abc 0 1234567 07");
    }

    #[test]
    fn truncates_silently() {
        let mut b = [0u8; 5];
        let mut w = W::new(&mut b);
        w.s("abc").s("defgh").dec(42).hex(1);
        assert_eq!(w.as_slice(), b"abcde");
        assert_eq!(w.len(), 5);
    }

    #[test]
    fn addr_classification() {
        let r = ranges();
        let mut b = [0u8; 256];
        let mut w = W::new(&mut b);
        w.addr(0x7ff7_0000_0010, &r).c(b'|').addr(0x7ff8_0000_0020, &r).c(b'|').addr(0x7ffa_0000_0001, &r).c(b'|').addr(0x1234, &r);
        assert_eq!(w.as_slice(), b"Three_Kingdoms.exe+0x10 (ghidra 0x140000010)|script_extender.dll+0x20|ntdll.dll+0x1|0x1234");
    }

    #[test]
    fn name_rejects_foreign_pointers() {
        let r = ranges();
        let mut b = [0u8; 64];
        let mut w = W::new(&mut b);
        let s = "hello";
        w.name(s.as_ptr() as usize, s.len(), &r).c(b'|').name(0, 0, &r);
        assert_eq!(w.as_slice(), b"?|?");
    }

    #[test]
    fn scope_depth_limit() {
        LEVEL.store(1, Ordering::Relaxed);
        let mut guards = Vec::new();
        for _ in 0..(SCOPE_DEPTH + 2) { guards.push(enter("x")); }
        assert_eq!(STACK.with(|s| s.get().depth) as usize, SCOPE_DEPTH);
        assert_eq!(SCOPE_OVERFLOW.load(Ordering::Relaxed), 2);
        drop(guards);
        assert_eq!(STACK.with(|s| s.get().depth), 0);
        LEVEL.store(0, Ordering::Relaxed);
    }

    #[test]
    fn ring_wraps() {
        LEVEL.store(1, Ordering::Relaxed);
        for _ in 0..(RING_N + 3) { boot("b"); }
        let cur = RING_CUR.load(Ordering::Relaxed);
        assert!(cur >= RING_N + 3);
        assert!(RING.iter().all(|c| c.tick.load(Ordering::Relaxed) != 0));
        LEVEL.store(0, Ordering::Relaxed);
    }
}
