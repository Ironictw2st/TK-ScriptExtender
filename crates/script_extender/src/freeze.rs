//! Suspend every other thread of the process while a code patch is applied, and make sure no
//! suspended thread's instruction pointer sits inside the bytes being rewritten. Raw Win32
//! toolhelp + GetThreadContext, no external crates.

use crate::log;
use core::ffi::c_void;

const TH32CS_SNAPTHREAD: u32 = 0x4;
const THREAD_SUSPEND_RESUME: u32 = 0x2;
const THREAD_GET_CONTEXT: u32 = 0x8;
const CONTEXT_CONTROL: u32 = 0x0010_0001;
const INVALID_HANDLE: usize = usize::MAX;

#[repr(C)]
struct ThreadEntry32 {
    dw_size: u32,
    cnt_usage: u32,
    th32_thread_id: u32,
    th32_owner_process_id: u32,
    tp_base_pri: i32,
    tp_delta_pri: i32,
    dw_flags: u32,
}

/// Opaque x64 CONTEXT (1232 bytes, 16-byte aligned). ContextFlags at +0x30, Rip at +0xF8.
#[repr(C, align(16))]
struct Ctx([u8; 1232]);

extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
    fn Thread32First(snap: *mut c_void, e: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snap: *mut c_void, e: *mut ThreadEntry32) -> i32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
    fn SuspendThread(h: *mut c_void) -> u32;
    fn ResumeThread(h: *mut c_void) -> u32;
    fn GetThreadContext(h: *mut c_void, ctx: *mut Ctx) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
    fn Sleep(ms: u32);
}

/// Run `f` with all other threads suspended and none of them executing inside
/// `[range_start, range_start + range_len)`. Threads caught inside the range are resumed
/// briefly and re-suspended (up to a few attempts). If one never leaves it, `f` is NOT run
/// and Err is returned: patching bytes under a running instruction pointer is not safe.
pub fn with_threads_frozen<R>(range_start: usize, range_len: usize, f: impl FnOnce() -> R) -> Result<R, String> {
    let pid = unsafe { GetCurrentProcessId() };
    let me = unsafe { GetCurrentThreadId() };
    let mut handles: Vec<*mut c_void> = Vec::new();
    let mut stuck: Option<u32> = None;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap as usize != INVALID_HANDLE && !snap.is_null() {
            let mut e = ThreadEntry32 {
                dw_size: core::mem::size_of::<ThreadEntry32>() as u32,
                cnt_usage: 0,
                th32_thread_id: 0,
                th32_owner_process_id: 0,
                tp_base_pri: 0,
                tp_delta_pri: 0,
                dw_flags: 0,
            };
            if Thread32First(snap, &mut e) != 0 {
                loop {
                    if e.th32_owner_process_id == pid && e.th32_thread_id != me {
                        let h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, 0, e.th32_thread_id);
                        if !h.is_null() {
                            if SuspendThread(h) != u32::MAX {
                                // Make sure it is not parked inside the patch range.
                                let mut ok = false;
                                for _ in 0..8 {
                                    let mut ctx = Ctx([0u8; 1232]);
                                    core::ptr::write_unaligned(ctx.0.as_mut_ptr().add(0x30) as *mut u32, CONTEXT_CONTROL);
                                    let rip = if GetThreadContext(h, &mut ctx) != 0 {
                                        core::ptr::read_unaligned(ctx.0.as_ptr().add(0xF8) as *const u64) as usize
                                    } else {
                                        0
                                    };
                                    if rip < range_start || rip >= range_start + range_len {
                                        ok = true;
                                        break;
                                    }
                                    ResumeThread(h);
                                    Sleep(1);
                                    SuspendThread(h);
                                }
                                if !ok {
                                    log!("thread {} stuck inside patch range 0x{range_start:x}; not patching", e.th32_thread_id);
                                    stuck = Some(e.th32_thread_id);
                                    ResumeThread(h);
                                    CloseHandle(h);
                                    break;
                                } else {
                                    handles.push(h);
                                }
                            } else {
                                CloseHandle(h);
                            }
                        }
                    }
                    if Thread32Next(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
    }
    let n = handles.len();
    let r = if stuck.is_none() { Some(f()) } else { None };
    unsafe {
        for h in handles {
            ResumeThread(h);
            CloseHandle(h);
        }
    }
    match (r, stuck) {
        (Some(r), _) => {
            log!("patched with {n} other thread(s) frozen");
            Ok(r)
        }
        (None, tid) => Err(format!("thread {} stayed inside 0x{range_start:x}..+{range_len}", tid.unwrap_or(0))),
    }
}

unsafe fn read16(addr: usize) -> [u8; 16] {
    core::ptr::read_unaligned(addr as *const [u8; 16])
}

/// Enable one detour the safe way and account for it: other threads frozen and out of the first
/// 16 bytes (else nothing is written), the bytes before / after recorded in the inventory
/// (status.rs, what other native mods compare against) and the hook announced to crash.rs.
/// Publish the detour's `OnceLock` BEFORE calling this: the detour runs as soon as the jump lands.
pub unsafe fn enable_detour<T: retour::Function>(name: &'static str, cfg_key: &'static str, target: usize, d: &retour::GenericDetour<T>) -> Result<(), String> {
    let before = read16(target);
    with_threads_frozen(target, 16, || d.enable())?.map_err(|e| e.to_string())?;
    crate::status::record_patch(name, cfg_key, target, before, read16(target));
    crate::crash::hook_installed(name, cfg_key, target);
    Ok(())
}
