//! Shared injection primitives for the dev injector and the launcher.
//!
//! Classic `CreateRemoteThread` + `LoadLibraryW` injection plus the process- and
//! window-discovery helpers a launch-time loader needs. Hand-declared Win32 FFI,
//! zero external dependencies — the same style as the original `injector` bin.
//!
//! The intended target (`Three_Kingdoms.exe`) is protected by a mutation stub at
//! its entry point, so the real code/IAT are only settled *after* OEP. Callers must
//! therefore attach post-init (at the main menu): use [`wait_for_main_window`] as a
//! readiness gate before [`inject`].
#![cfg(windows)]

use core::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

type Handle = *mut c_void;
type Hwnd = *mut c_void;

const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const MEM_COMMIT_RESERVE: u32 = 0x3000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;
const INFINITE: u32 = 0xFFFF_FFFF;
// Only the rights we need: create-thread | vm-operation | vm-write | vm-read | query.
const PROCESS_ACCESS: u32 = 0x0002 | 0x0008 | 0x0020 | 0x0010 | 0x0400;
// For WaitForInputIdle: query-information | synchronize.
const PROCESS_QUERY_SYNC: u32 = 0x0400 | 0x0010_0000;
const GA_ROOT: u32 = 2;

/// A single process from a toolhelp snapshot.
#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
}

#[repr(C)]
struct ProcessEntry32W {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u16; 260],
}

/// Snapshots every process (pid, parent pid, exe name). Empty on snapshot failure.
pub fn enumerate_processes() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap as isize == -1 {
            return out;
        }
        let mut entry: ProcessEntry32W = core::mem::zeroed();
        entry.dw_size = core::mem::size_of::<ProcessEntry32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let len = entry.sz_exe_file.iter().position(|&c| c == 0).unwrap_or(260);
                out.push(ProcInfo {
                    pid: entry.th32_process_id,
                    parent_pid: entry.th32_parent_process_id,
                    name: String::from_utf16_lossy(&entry.sz_exe_file[..len]),
                });
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// First PID whose exe name matches `name` (case-insensitive), if any.
pub fn find_pid(name: &str) -> Option<u32> {
    enumerate_processes()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.pid)
}

/// All PIDs whose exe name matches `name` (case-insensitive).
pub fn find_pids(name: &str) -> Vec<u32> {
    enumerate_processes()
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.pid)
        .collect()
}

/// LoadLibrary-injects `dll` into process `pid`.
///
/// Returns the low 32 bits of the remote `HMODULE` (0 means the load failed inside
/// the target — on x64 the true HMODULE is truncated to the 32-bit thread exit code,
/// so success is inferred, not verified).
pub fn inject(pid: u32, dll: &Path) -> Result<u32, String> {
    let mut wide: Vec<u16> = dll.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;

    unsafe {
        let proc = OpenProcess(PROCESS_ACCESS, 0, pid);
        if proc.is_null() {
            return Err("OpenProcess failed (run as admin?)".into());
        }

        let remote = VirtualAllocEx(proc, core::ptr::null(), bytes, MEM_COMMIT_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            CloseHandle(proc);
            return Err("VirtualAllocEx failed".into());
        }

        let mut written = 0usize;
        if WriteProcessMemory(proc, remote, wide.as_ptr() as *const c_void, bytes, &mut written) == 0 {
            VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
            CloseHandle(proc);
            return Err("WriteProcessMemory failed".into());
        }

        let k32 = GetModuleHandleW(wstr("kernel32.dll").as_ptr());
        let load_library = GetProcAddress(k32, b"LoadLibraryW\0".as_ptr());
        if load_library.is_null() {
            VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
            CloseHandle(proc);
            return Err("GetProcAddress(LoadLibraryW) failed".into());
        }

        let thread = CreateRemoteThread(
            proc,
            core::ptr::null(),
            0,
            core::mem::transmute::<*const c_void, ThreadStart>(load_library),
            remote,
            0,
            core::ptr::null_mut(),
        );
        if thread.is_null() {
            VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
            CloseHandle(proc);
            return Err("CreateRemoteThread failed".into());
        }

        WaitForSingleObject(thread, INFINITE);
        let mut exit_code = 0u32;
        GetExitCodeThread(thread, &mut exit_code);

        VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
        CloseHandle(thread);
        CloseHandle(proc);

        if exit_code == 0 {
            Err("LoadLibraryW returned 0 inside target (DLL failed to load)".into())
        } else {
            Ok(exit_code)
        }
    }
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    lp_reserved: *mut u16,
    lp_desktop: *mut u16,
    lp_title: *mut u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *mut u8,
    h_std_input: Handle,
    h_std_output: Handle,
    h_std_error: Handle,
}

#[repr(C)]
struct ProcessInformation {
    h_process: Handle,
    h_thread: Handle,
    pid: u32,
    tid: u32,
}

/// Spawns `exe` directly (`CreateProcessW`) with command line `"<exe name>" <args>` and
/// working directory `cwd`. Returns the new PID and the process handle (caller closes it
/// via [`close_handle`]). This is the Runcher-proven way to launch TW:3K with a mod list:
/// the game becomes our own child (no Steam/Electron launcher in the tree), so we hold
/// the real PID directly. Steam must be running and the game owned for DRM to pass.
pub fn spawn_game(exe: &Path, args: &str, cwd: &Path) -> Result<(u32, *mut c_void), String> {
    // lpCommandLine must be mutable. Quote the exe path as argv[0], then the args verbatim.
    let cmdline = format!("\"{}\" {}", exe.to_string_lossy(), args);
    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let app: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let dir: Vec<u16> = cwd.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut si: StartupInfoW = core::mem::zeroed();
        si.cb = core::mem::size_of::<StartupInfoW>() as u32;
        let mut pi: ProcessInformation = core::mem::zeroed();

        let ok = CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            0,
            core::ptr::null(),
            dir.as_ptr(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err("CreateProcessW failed (is the game path correct?)".into());
        }
        // We don't need the thread handle.
        CloseHandle(pi.h_thread);
        Ok((pi.pid, pi.h_process))
    }
}

/// Closes a handle returned by [`spawn_game`].
pub fn close_handle(h: *mut c_void) {
    if !h.is_null() {
        unsafe { CloseHandle(h) };
    }
}

/// Readiness gate: blocks until `pid` has a visible top-level window (its main menu
/// is up, so we're safely past the protector stub / OEP), or `timeout` elapses.
///
/// `WaitForInputIdle` is a cheap first pass; it can return early, so we then poll for
/// a genuine visible, top-level window owned by the PID before returning `true`. On
/// timeout returns `false` — the caller may still choose to inject.
pub fn wait_for_main_window(pid: u32, timeout: Duration) -> bool {
    unsafe {
        let proc = OpenProcess(PROCESS_QUERY_SYNC, 0, pid);
        if !proc.is_null() {
            // 0xFFFFFFFF-safe cap; WaitForInputIdle takes ms.
            let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            WaitForInputIdle(proc, ms);
            CloseHandle(proc);
        }
    }

    let deadline = Instant::now() + timeout;
    loop {
        if has_visible_top_window(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct FindWindowCtx {
    pid: u32,
    found: bool,
}

fn has_visible_top_window(pid: u32) -> bool {
    let mut ctx = FindWindowCtx { pid, found: false };
    unsafe {
        EnumWindows(enum_proc, &mut ctx as *mut FindWindowCtx as isize);
    }
    ctx.found
}

unsafe extern "system" fn enum_proc(hwnd: Hwnd, lparam: isize) -> i32 {
    let ctx = &mut *(lparam as *mut FindWindowCtx);
    let mut wpid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut wpid);
    if wpid == ctx.pid
        && IsWindowVisible(hwnd) != 0
        && GetAncestor(hwnd, GA_ROOT) == hwnd
        && window_has_title(hwnd)
    {
        ctx.found = true;
        return 0; // stop enumeration
    }
    1 // continue
}

unsafe fn window_has_title(hwnd: Hwnd) -> bool {
    let mut buf = [0u16; 128];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    n > 0
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

type ThreadStart = unsafe extern "system" fn(*mut c_void) -> u32;

extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    fn Process32FirstW(snap: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snap: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn VirtualAllocEx(proc: Handle, addr: *const c_void, size: usize, typ: u32, protect: u32) -> *mut c_void;
    fn VirtualFreeEx(proc: Handle, addr: *mut c_void, size: usize, typ: u32) -> i32;
    fn WriteProcessMemory(proc: Handle, addr: *mut c_void, buf: *const c_void, size: usize, written: *mut usize) -> i32;
    fn GetModuleHandleW(name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
    fn CreateRemoteThread(proc: Handle, attrs: *const c_void, stack: usize, start: ThreadStart, param: *mut c_void, flags: u32, tid: *mut u32) -> Handle;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn GetExitCodeThread(h: Handle, code: *mut u32) -> i32;
    fn CreateProcessW(
        app: *const u16,
        cmdline: *mut u16,
        proc_attrs: *const c_void,
        thread_attrs: *const c_void,
        inherit: i32,
        flags: u32,
        env: *const c_void,
        cwd: *const u16,
        startup: *const StartupInfoW,
        info: *mut ProcessInformation,
    ) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn WaitForInputIdle(proc: Handle, ms: u32) -> u32;
    fn EnumWindows(cb: unsafe extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn IsWindowVisible(hwnd: Hwnd) -> i32;
    fn GetAncestor(hwnd: Hwnd, flags: u32) -> Hwnd;
    fn GetWindowTextW(hwnd: Hwnd, buf: *mut u16, max: i32) -> i32;
}
