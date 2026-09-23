//! TW:3K script extender, injected DLL core (build 1.7.2.0, plain PE, ASLR).
//!
//! `DllMain` only spawns a bootstrap thread; everything else runs off the loader lock.
//! The bootstrap fingerprints the host exe, resolves fixed RVAs (verified by anchor bytes),
//! and hooks `lua_gettop`. The hook registers our C functions into every Lua state it sees,
//! on the thread that owns that state, so injecting into a running campaign works.
#![cfg(windows)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

mod addrs;
mod freeze;
mod hook;
mod logging;
mod lua;
mod pool;
mod process;
mod recruit;
mod units;
mod progression;
mod chars;
mod cai;
mod potential;
mod xp;
mod build;
mod buildings;
mod alliances;
mod autoresolve;
mod income;
mod airecruit;
mod diag;
mod diptrace;
mod dipui;
mod followup;
mod fileprobe;
mod perf;
mod permcache;
mod profiler;
mod bundles;
mod diplomacy;
mod crash;
mod marriage;
mod status;

static SELF_HMODULE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn self_hmodule() -> *mut c_void {
    SELF_HMODULE.load(Ordering::Relaxed) as *mut c_void
}

const DLL_PROCESS_ATTACH: u32 = 1;

#[no_mangle]
#[allow(non_snake_case, unused_variables)]
pub extern "system" fn DllMain(hinst: *mut c_void, reason: u32, reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        SELF_HMODULE.store(hinst as usize, Ordering::Relaxed);
        unsafe { DisableThreadLibraryCalls(hinst) };
        std::thread::spawn(|| {
            if std::panic::catch_unwind(bootstrap).is_err() {
                status::refuse(status::REFUSED, "bootstrap panicked (see script_extender.log)");
            }
        });
    }
    1
}

fn bootstrap() {
    logging::init();
    log!("bootstrap thread started (script_extender {})", env!("CARGO_PKG_VERSION"));

    // Host-identity gate: no-op in any process that is not the game.
    let host_ok = process::main_module_path()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_ascii_lowercase()))
        .map(|f| f == "three_kingdoms.exe")
        .unwrap_or(false);
    if !host_ok {
        log!("host is not Three_Kingdoms.exe; doing nothing");
        status::set_state(status::NOT_GAME);
        return;
    }

    let (base, size) = process::main_module();
    log!("main module base=0x{base:x} size=0x{size:x}");
    let Some(table) = addrs::resolve(base, size) else {
        // resolve() has already published REFUSED and written se_inventory.json
        log!("address table does not match this build; doing nothing");
        return;
    };
    // first, so a fault while a detour is being written is already reported
    crash::install();
    crash::boot("init");
    lua::init(&table);
    pool::init(&table);
    recruit::init(&table);
    units::init(&table);
    progression::init(&table);
    cai::init(&table);
    potential::init(&table);
    xp::init(&table);
    build::init(&table);
    buildings::init(&table);
    alliances::init(&table);
    bundles::init(&table);
    diplomacy::init(&table);
    build::apply_config();
    crash::boot("autoresolve");
    autoresolve::install_hooks(&table);
    crash::boot("income");
    income::install(&table);
    crash::boot("perf");
    perf::install(&table);
    crash::boot("airecruit");
    airecruit::install(&table);
    crash::boot("permcache");
    permcache::install(&table);
    crash::boot("fileprobe");
    fileprobe::install(&table);
    crash::boot("diag");
    diag::install(&table);
    crash::boot("followup");
    followup::install(&table);
    crash::boot("marriage");
    marriage::install(&table);
    crash::boot("hook");
    if !hook::install(&table) {
        // without lua_gettop no Lua state ever gets the API: not "ready"
        status::refuse(status::REFUSED, "lua_gettop hook failed");
        return;
    }
    crash::boot("complete");
    status::set_state(status::READY);
    status::write_inventory();
    log!("bootstrap complete ({} patches); waiting for the game's Lua to tick", status::patch_count());
}

extern "system" {
    fn DisableThreadLibraryCalls(hmod: *mut c_void) -> i32;
}

/// Read-only natives are called thousands of times per turn by report scripts (0.36.1: a 70 MB
/// log from one traced end turn). Their per-call log lines are for layout work only: true for
/// the first `limit` calls of a session.
pub(crate) fn chatty(counter: &std::sync::atomic::AtomicU32, limit: u32) -> bool {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < limit
}
