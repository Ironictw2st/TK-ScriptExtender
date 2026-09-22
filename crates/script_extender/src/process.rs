//! Main-module discovery and our own on-disk location. PE header parsing only —
//! no dependency on the PSAPI/toolhelp APIs.

use core::ffi::c_void;
use std::path::PathBuf;

/// Base address and `SizeOfImage` of the main executable (the module with a NULL name).
pub fn main_module() -> (usize, usize) {
    // hot callers (income hook, caches) ask on every call; the exe never moves
    static MAIN: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *MAIN.get_or_init(|| unsafe {
        let base = GetModuleHandleW(core::ptr::null()) as usize;
        (base, image_size(base))
    })
}

/// Reads `OptionalHeader.SizeOfImage` (PE32+). Assumes a valid loaded image at `base`.
pub(crate) unsafe fn image_size(base: usize) -> usize {
    if base == 0 {
        return 0;
    }
    let e_lfanew = *((base + 0x3C) as *const i32) as usize; // DOS header -> NT offset
    let nt = base + e_lfanew;
    let opt = nt + 0x18; // skip PE sig (4) + FileHeader (20)
    *((opt + 0x38) as *const u32) as usize // OptionalHeader.SizeOfImage
}

/// Full path of the host executable (the module with a NULL name).
pub fn main_module_path() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let n = unsafe { GetModuleFileNameW(core::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Directory containing our own DLL, for locating the signature/log files.
pub fn self_dir() -> Option<PathBuf> {
    let hmod = crate::self_hmodule();
    let mut buf = [0u16; 260];
    let n = unsafe { GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..n]);
    PathBuf::from(s).parent().map(|p| p.to_path_buf())
}

extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetModuleFileNameW(hmod: *mut c_void, buf: *mut u16, size: u32) -> u32;
}
