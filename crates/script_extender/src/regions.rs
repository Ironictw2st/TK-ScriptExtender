//! Campaign maps with more than 511 regions (notes/map_regions.md). map_data.esf / hlp_data.esf
//! store an area key as u16 `region | area << 9`; CA's builder stops at 511 regions. The exe
//! never splits the key on the load / movement / hover paths; only five `and r, 0x1ff`
//! instructions (AI-side, the 0x141d2xxxx block) take the region out of it. cfg
//! `map_region_bits = 10` widens those masks to 0x3ff, for maps written as `region | area << 10`
//! (1023 regions, 64 areas per region). Default 9 = vanilla, nothing patched.

use crate::addrs::Table;
use crate::log;
use core::ffi::c_void;

/// (anchor name, offset of the mask's high byte inside the instruction)
/// `81 e2/e7/e1 ff 01 00 00` = and r32, 0x1ff (imm32 at +2); `25 ff 01 00 00` = and eax, 0x1ff.
const SITES: &[(&str, usize)] = &[
    ("region_mask_a", 3),
    ("region_mask_b", 3),
    ("region_mask_c", 3),
    ("region_mask_d", 2),
    ("region_mask_e", 3),
];

const PAGE_EXECUTE_READWRITE: u32 = 0x40;

extern "system" {
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn FlushInstructionCache(process: *mut c_void, addr: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}

/// cfg `map_region_bits`: 9 (vanilla) or 10. Anything else reads as 9. Also hashed by the sync tag.
pub fn bits() -> u32 {
    match crate::build::config_value("map_region_bits").as_deref() {
        Some("10") => 10,
        _ => 9,
    }
}

unsafe fn write_byte(addr: usize, v: u8) -> bool {
    let mut old = 0u32;
    if VirtualProtect(addr as *mut c_void, 1, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
        return false;
    }
    core::ptr::write_volatile(addr as *mut u8, v);
    let mut tmp = 0u32;
    VirtualProtect(addr as *mut c_void, 1, old, &mut tmp);
    FlushInstructionCache(GetCurrentProcess(), addr as *const c_void, 1);
    true
}

pub fn install(t: &Table) {
    let bits = bits();
    if bits == 9 {
        log!("map regions: vanilla 9-bit area keys (map_region_bits not 10); nothing patched");
        return;
    }
    let high = (((1u32 << bits) - 1) >> 8) as u8;
    // all or nothing: a map keyed one way read by masks of both widths would be worse than vanilla
    let mut done: Vec<(&'static str, usize, [u8; 16])> = Vec::new();
    for (name, hi_off) in SITES {
        let insn = t.get(name);
        let before = unsafe { core::ptr::read_unaligned(insn as *const [u8; 16]) };
        // the anchor (8 bytes at the instruction) was verified at bootstrap; threads are kept out
        // of the instruction while its immediate changes
        let r = crate::freeze::with_threads_frozen(insn, 6, || unsafe { write_byte(insn + hi_off, high) });
        match r {
            Ok(true) => done.push((name, insn, before)),
            Ok(false) => { log!("map regions: VirtualProtect failed at 0x{insn:x} ({name})"); break; }
            Err(e) => { log!("map regions: {name} not patched ({e})"); break; }
        }
    }
    if done.len() != SITES.len() {
        for (name, insn, before) in &done {
            let hi_off = SITES.iter().find(|(n, _)| n == name).map(|(_, o)| *o).unwrap_or(0);
            let ok = crate::freeze::with_threads_frozen(*insn, 6, || unsafe { write_byte(insn + hi_off, before[hi_off]) });
            log!("map regions: reverted {name} ({})", if matches!(ok, Ok(true)) { "ok" } else { "FAILED" });
        }
        log!("map regions: widening failed; vanilla 9-bit masks kept. A map with more than 511 regions will misbehave in AI turns");
        return;
    }
    for (name, insn, before) in &done {
        let after = unsafe { core::ptr::read_unaligned(*insn as *const [u8; 16]) };
        crate::status::record_patch(name, "map_region_bits", *insn, *before, after);
    }
    log!("map regions: {} region masks widened to 0x{:x} ({bits}-bit region, {}-bit area keys)", SITES.len(), (1u32 << bits) - 1, 16 - bits);
}
