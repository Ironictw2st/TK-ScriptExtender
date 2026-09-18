//! Dev loader: LoadLibrary-injects script_extender.dll into a running game.
//!
//! Usage:  injector [DLL_PATH] [PROCESS_NAME]
//!   DLL_PATH       defaults to script_extender.dll next to this exe
//!   PROCESS_NAME   defaults to Three_Kingdoms.exe
//!
//! Attach at the main menu (post-OEP) so the protector stub has already run. All the
//! Win32 work lives in the shared `inject_core` crate (used by the launcher too).
#![cfg(windows)]

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let dll = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_dll_path);
    let dll = match std::fs::canonicalize(&dll) {
        Ok(p) => p,
        Err(e) => fail(&format!("DLL not found: {} ({e})", dll.display())),
    };
    let proc_name = args.get(2).map(String::as_str).unwrap_or("Three_Kingdoms.exe");

    println!("injector: dll  = {}", dll.display());
    println!("injector: proc = {proc_name}");

    let pid = match inject_core::find_pid(proc_name) {
        Some(p) => p,
        None => fail(&format!("process '{proc_name}' not running")),
    };
    println!("injector: pid  = {pid}");

    match inject_core::inject(pid, &dll) {
        Ok(base) => println!("injector: OK, remote HMODULE low = 0x{base:x}"),
        Err(e) => fail(&e),
    }
}

fn default_dll_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("script_extender.dll")))
        .unwrap_or_else(|| PathBuf::from("script_extender.dll"))
}

fn fail(msg: &str) -> ! {
    eprintln!("injector: error: {msg}");
    std::process::exit(1);
}
