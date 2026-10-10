//! Campaign maps with more than 511 regions (notes/map_regions.md). map_data.esf / hlp_data.esf
//! store an area key as u16 `region | area << 9` (0x1ff region / 0x7f area = none); CA's builder
//! stops at 511 regions. The exe splits the key inline in ~85 functions (region_sites.rs, found by
//! tools/regions/). cfg `map_region_bits = 10` rewrites every one of them for keys written as
//! `region | area << 10` (1023 regions, 63 areas per region). Default 9 = vanilla, nothing patched.
//!
//! The site anchors are checked here, only when the feature is on: a mismatch (other build, another
//! mod's patch) turns the feature off instead of refusing the whole DLL. All or nothing.

use crate::addrs::Table;
use crate::log;
use crate::region_sites::SITES;
use core::ffi::c_void;

const PAGE_EXECUTE_READWRITE: u32 = 0x40;

extern "system" {
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn FlushInstructionCache(process: *mut c_void, addr: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}

/// cfg `map_region_bits`: 9 (vanilla) or 10. Anything else reads as 9. Also hashed by the sync tag.
pub fn bits() -> u32 {
    match crate::build::config_value("map_region_bits").as_deref() {
        Some("10") => 10,
        _ => 9,
    }
}

/// Write one byte of code; false if the page could not be made writable.
unsafe fn write_byte(addr: usize, v: u8) -> bool {
    let mut old = 0u32;
    if VirtualProtect(addr as *mut c_void, 1, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
        return false;
    }
    core::ptr::write_volatile(addr as *mut u8, v);
    let mut tmp = 0u32;
    VirtualProtect(addr as *mut c_void, 1, old, &mut tmp);
    true
}

pub fn install(t: &Table) {
    let bits = bits();
    if bits == 9 {
        log!("map regions: vanilla 9-bit area keys (map_region_bits not 10); nothing patched");
        return;
    }
    let base = t.base();
    // 1. every anchor first: one mismatch and nothing is written
    let mut bad = 0;
    for (rva, anchor, off, old, _) in SITES {
        let addr = base + *rva as usize;
        let ok = unsafe {
            IsBadReadPtr(addr as *const c_void, 8) == 0
                && core::ptr::read_unaligned(addr as *const [u8; 8]) == *anchor
                && anchor[*off as usize] == *old
        };
        if !ok {
            if bad < 8 {
                let got = unsafe { core::ptr::read_unaligned(addr as *const [u8; 8]) };
                log!("map regions: anchor mismatch at Three_Kingdoms.exe+0x{rva:x}: got {got:02x?}, want {anchor:02x?}");
            }
            bad += 1;
        }
    }
    if bad > 0 {
        log!("map regions: {bad}/{} anchors differ; 10-bit area keys OFF (a map with more than 511 regions will crash)", SITES.len());
        return;
    }
    // 2. all bytes in one freeze, threads kept out of every patched instruction
    let first = base + SITES[0].0 as usize;
    let before = unsafe { core::ptr::read_unaligned(first as *const [u8; 16]) };
    let ranges: Vec<(usize, usize)> = SITES.iter().map(|(rva, ..)| (base + *rva as usize, 8)).collect();
    let r = crate::freeze::with_threads_frozen_ranges(&ranges, || unsafe {
        let mut written = 0usize;
        for (rva, _, off, _, new) in SITES {
            if !write_byte(base + *rva as usize + *off as usize, *new) {
                break;
            }
            written += 1;
        }
        if written != SITES.len() {
            // put back what was changed: half-patched decoders are worse than vanilla
            for (rva, _, off, old, _) in &SITES[..written] {
                write_byte(base + *rva as usize + *off as usize, *old);
            }
        }
        FlushInstructionCache(GetCurrentProcess(), core::ptr::null(), 0);
        written
    });
    match r {
        Ok(n) if n == SITES.len() => {}
        Ok(n) => {
            log!("map regions: VirtualProtect failed after {n} of {} sites; reverted, 10-bit area keys OFF", SITES.len());
            return;
        }
        Err(e) => {
            log!("map regions: not patched ({e}); 10-bit area keys OFF");
            return;
        }
    }
    // the inventory lists the first site; the full table is region_sites.rs
    let after = unsafe { core::ptr::read_unaligned(first as *const [u8; 16]) };
    crate::status::record_patch("map_region_keys", "map_region_bits", first, before, after);
    log!("map regions: {} area-key sites rewritten for {bits}-bit region / {}-bit area keys (region mask 0x{:x})", SITES.len(), 16 - bits, (1u32 << bits) - 1);
}
