//! Two engine patches, re-implemented here from the ideas in two third-party mod packs
//! (`z_ngc_cai_null_guard.pack`, `z_ngc_battle_crash_fix.pack`) by **Sleeping Rowlet**, who is credited
//! for both fixes (see notes/SEPATCHES_zngc.md). No code or binary of those packs is used:
//! the addresses were re-derived and re-verified on exe 1.7.2.0 (TimeDateStamp 0x69CE4C84) and the
//! replacement code below was written from the original function's own disassembly.
//!
//! 1. **CAI null guard** (cfg `cai_null_guard`). `FUN_141e570b0(x)` = "does x's record at +0x178
//!    have state 1" (`mov rax,[rcx+0x178]; test rax,rax; jz false; cmp [rax+0x10],1`). Three
//!    callers (exe+0x1d25dcc, +0x1d25e6c, +0x1d32114, all of the shape
//!    `x = lookup(id & 0x1ff); if (!f(x)) { x->vf[0x18]() ... }`) pass a lookup result that can be
//!    NULL and fault on `[rcx+0x178]` (the other 20 callers are untouched). The three calls are
//!    redirected to a stub that answers "true" for a NULL object, which is the branch that skips
//!    the `x->vf[0x18]()` work, i.e. "nothing to do for a missing entry". Any non-NULL object runs
//!    the original function unchanged.
//! 2. **Battle instance transform** (cfg `battle_xform_fast`). `FUN_1413778b0` builds a 0x74-byte
//!    instance record: it copies a 3x4 affine matrix, inverts it as a generic 4x4 (last row
//!    0,0,0,1) with loop-built cofactor minors, stores 8 further fields and three colour bytes.
//!    The detour computes the *same* floating-point operations in the same order, fully unrolled
//!    (no loops, no stack minors), so every output byte is identical for finite input; NaN inputs
//!    give NaNs, but which payload comes out can differ when both operands of one operation are
//!    NaN. The original is entered unchanged (through the trampoline) if the matrix argument
//!    partly overlaps the record. The function bytes are pinned by an FNV-1a hash of all 0x735
//!    bytes, so the patch only installs on the exact exe it was derived from.
//!
//! The differential test (`cargo test xform_matches_exe`) executes the exe's own function from a
//! copy of its bytes and compares it with `fill_fast` on millions of random inputs.

use crate::addrs::Table;
use crate::lua::{self, LuaState};
use crate::log;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use retour::GenericDetour;
use std::sync::OnceLock;

type P = *mut c_void;
/// FUN_1413778b0(this, matrix3x4, pair, f32 a, f32 b, vec3, flag) -> this
type Xform = unsafe extern "C" fn(*mut u8, *const u8, *const u8, f32, f32, *const f32, usize) -> *mut u8;

const CAI_FUNC_RVA: usize = 0x1e570b0;
/// mov rax,[rcx+0x178]; test rax,rax; jz +9; cmp dword [rax+0x10],1; jnz +3; mov al,1; ret; xor al,al; ret
const CAI_FUNC_BYTES: [u8; 24] = [
    0x48, 0x8b, 0x81, 0x78, 0x01, 0x00, 0x00, 0x48, 0x85, 0xc0, 0x74, 0x09, 0x83, 0x78, 0x10, 0x01, 0x75, 0x03, 0xb0, 0x01, 0xc3, 0x32, 0xc0, 0xc3,
];
/// the `call FUN_141e570b0` instructions (the e8 byte)
const CAI_SITES: [usize; 3] = [0x1d25dcc, 0x1d25e6c, 0x1d32114];

const XFORM_RVA: usize = 0x13778b0;
const XFORM_LEN: usize = 0x735;
const XFORM_FNV: u64 = 0x4e22_8b34_648b_c0df;
const XFORM_HEAD: [u8; 16] = [0x48, 0x8b, 0xc4, 0xf3, 0x0f, 0x11, 0x58, 0x20, 0x4c, 0x89, 0x40, 0x18, 0x55, 0x41, 0x55, 0x48];

static BASE: AtomicUsize = AtomicUsize::new(0);
static XFORM: OnceLock<GenericDetour<Xform>> = OnceLock::new();
static CAI_STUB: AtomicUsize = AtomicUsize::new(0);
static CAI_SITES_PATCHED: AtomicU32 = AtomicU32::new(0);
/// incremented by the CAI stub itself (`lock inc qword [r11]`)
pub static CAI_HITS: AtomicU64 = AtomicU64::new(0);
static XFORM_CALLS: AtomicU64 = AtomicU64::new(0);
static XFORM_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static XFORM_ON: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------------------------
// battle instance transform
// ---------------------------------------------------------------------------------------------

/// 3x3 determinant exactly as the engine computes it: expansion along row 0 of the minor, the
/// 2x2 sub-determinant `s3*s0 - s1*s2` of the other two rows (columns 1,2) times the (sign
/// alternated) leading element, accumulated onto +0.0.
#[inline(always)]
fn det3(m: &[f32; 9]) -> f32 {
    let mut acc = 0.0f32;
    // b = 0: rows 1,2; b = 1: rows 0,2 (leading element negated); b = 2: rows 0,1
    let rows: [(usize, usize); 3] = [(1, 2), (0, 2), (0, 1)];
    let mut b = 0;
    while b < 3 {
        let lead = if b & 1 == 1 { -m[b * 3] } else { m[b * 3] };
        let (r0, r1) = rows[b];
        let d2 = m[r1 * 3 + 2] * m[r0 * 3 + 1] - m[r0 * 3 + 2] * m[r1 * 3 + 1];
        acc += d2 * lead;
        b += 1;
    }
    acc
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn d3(a0: f32, a1: f32, a2: f32, a3: f32, a4: f32, a5: f32, a6: f32, a7: f32, a8: f32) -> f32 {
    det3(&[a0, a1, a2, a3, a4, a5, a6, a7, a8])
}

// inverse_rows_unrolled: generated from the same algorithm by sepatches/gen_inverse.py
include!("sepatches/inverse_unrolled.rs");

/// Loop form, kept as the readable reference (the tests compare it with the unrolled one and with
/// the exe's own function).
/// The engine's inverse of the 4x4 matrix `[m (3 rows of 4), 0 0 0 1]`, rows 0..2 of the result;
/// a determinant with |det| < FLT_MIN (or NaN) leaves the matrix itself in the output.
#[inline(always)]
#[allow(dead_code)]
pub fn inverse_rows(m: &[f32; 12]) -> [f32; 12] {
    let full: [f32; 16] = [m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8], m[9], m[10], m[11], 0.0, 0.0, 0.0, 1.0];
    // determinant: expansion along column 0
    let mut det = 0.0f32;
    let mut i = 0;
    while i < 4 {
        let lead = if i & 1 == 1 { -full[i * 4] } else { full[i * 4] };
        let mut minor = [0.0f32; 9];
        let (mut k, mut r) = (0, 0);
        while r < 4 {
            if r != i {
                minor[k * 3] = full[r * 4 + 1];
                minor[k * 3 + 1] = full[r * 4 + 2];
                minor[k * 3 + 2] = full[r * 4 + 3];
                k += 1;
            }
            r += 1;
        }
        det += lead * det3(&minor);
        i += 1;
    }
    if !(det.abs() >= f32::MIN_POSITIVE) {
        return *m;
    }
    let inv = 1.0f32 / det;
    let mut out = [0.0f32; 12];
    let mut i = 0;
    while i < 3 {
        let mut j = 0;
        while j < 4 {
            // cofactor of element (row j, column i), transposed into row i
            let mut minor = [0.0f32; 9];
            let (mut k, mut r) = (0, 0);
            while r < 4 {
                if r != j {
                    let mut c = 0;
                    let mut n = 0;
                    while c < 4 {
                        if c != i {
                            minor[k * 3 + n] = full[r * 4 + c];
                            n += 1;
                        }
                        c += 1;
                    }
                    k += 1;
                }
                r += 1;
            }
            let sign = if (i + j) & 1 == 1 { -1.0f32 } else { 1.0f32 };
            out[i * 4 + j] = (det3(&minor) * sign) * inv;
            j += 1;
        }
        i += 1;
    }
    out
}

/// colour channel the way the engine rounds it: x = v*255; x += 0.5 if x > 0 else x -= 0.5;
/// truncate (cvttss2si, out of range / NaN gives 0x80000000 -> low byte 0).
#[inline(always)]
fn colour_byte(v: f32) -> u8 {
    let x = v * 255.0f32;
    let x = if x > 0.0 { x + 0.5 } else { x - 0.5 };
    // SAFETY: SSE is part of the x86-64 baseline
    unsafe { core::arch::x86_64::_mm_cvttss_si32(core::arch::x86_64::_mm_set_ss(x)) as u8 }
}

/// Everything the engine function does, without its loops. Same stores in the same order.
///
/// # Safety
/// `this` must be writable for 0x74 bytes, `m` readable for 0x30, `pair` for 8, `vec` for 12.
#[inline(always)]
pub unsafe fn fill_fast(this: *mut u8, m: *const u8, pair: *const u8, a: f32, b: f32, vec: *const f32, flag: usize) -> *mut u8 {
    let src: [u32; 12] = core::ptr::read_unaligned(m as *const [u32; 12]);
    core::ptr::write_unaligned(this as *mut [u32; 12], src);
    let mf: [f32; 12] = [
        f32::from_bits(src[0]), f32::from_bits(src[1]), f32::from_bits(src[2]), f32::from_bits(src[3]),
        f32::from_bits(src[4]), f32::from_bits(src[5]), f32::from_bits(src[6]), f32::from_bits(src[7]),
        f32::from_bits(src[8]), f32::from_bits(src[9]), f32::from_bits(src[10]), f32::from_bits(src[11]),
    ];
    let inv = inverse_rows_unrolled(&mf);
    let mut bits = [0u32; 12];
    let mut n = 0;
    while n < 12 {
        bits[n] = inv[n].to_bits();
        n += 1;
    }
    core::ptr::write_unaligned(this.add(0x30) as *mut [u32; 12], bits);
    core::ptr::write_unaligned(this.add(0x60) as *mut u32, core::ptr::read_unaligned(pair as *const u32));
    core::ptr::write_unaligned(this.add(0x64) as *mut u32, core::ptr::read_unaligned(pair.add(4) as *const u32));
    core::ptr::write_unaligned(this.add(0x68) as *mut f32, a);
    core::ptr::write_unaligned(this.add(0x6c) as *mut f32, b);
    *this.add(0x73) = flag as u8;
    core::ptr::write_unaligned(this.add(0x71) as *mut u16, 0xffff);
    *this.add(0x70) = 0xff;
    *this.add(0x70) = colour_byte(core::ptr::read_unaligned(vec.add(2)));
    *this.add(0x71) = colour_byte(core::ptr::read_unaligned(vec.add(1)));
    *this.add(0x72) = colour_byte(core::ptr::read_unaligned(vec));
    this
}

unsafe extern "C" fn xform_detour(this: *mut u8, m: *const u8, pair: *const u8, a: f32, b: f32, vec: *const f32, flag: usize) -> *mut u8 {
    let Some(h) = XFORM.get() else { return this };
    XFORM_CALLS.fetch_add(1, Ordering::Relaxed);
    // the engine copies the matrix into the record before reading it again; if the two ranges
    // overlap other than exactly, keep the original's behaviour by running the original
    let (t, s) = (this as usize, m as usize);
    if t != s && t < s + 0x30 && s < t + 0x30 {
        XFORM_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return h.call(this, m, pair, a, b, vec, flag);
    }
    fill_fast(this, m, pair, a, b, vec, flag)
}

unsafe fn fnv1a(base: usize, len: usize) -> Option<u64> {
    if !crate::engineguard::readable(base, len) { return None; }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for i in 0..len {
        h ^= *((base + i) as *const u8) as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(h)
}

unsafe fn install_xform(base: usize) -> Result<(), String> {
    let target = base + XFORM_RVA;
    if crate::engineguard::bytes_at::<16>(target) != Some(XFORM_HEAD) { return Err(format!("bytes at {target:#x} differ")); }
    match fnv1a(target, XFORM_LEN) {
        Some(XFORM_FNV) => {}
        Some(h) => return Err(format!("function hash {h:#x} differs from {XFORM_FNV:#x} (another exe build?)")),
        None => return Err("function not readable".into()),
    }
    let f: Xform = core::mem::transmute(target);
    let d = GenericDetour::new(f, xform_detour).map_err(|e| e.to_string())?;
    let _ = XFORM.set(d);
    let d = XFORM.get().ok_or("detour not stored")?;
    crate::freeze::enable_detour("battle_xform_fast", "battle_xform_fast", target, d)?;
    XFORM_ON.store(true, Ordering::Relaxed);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// CAI null guard
// ---------------------------------------------------------------------------------------------

/// Entered by the `call` at each CAI site (rsp = 8 mod 16, rcx = object, rax/rdx/r8-r11 dead):
/// ```text
///   test rcx,rcx ; jz null
///   mov rax,func ; jmp rax          ; tail call: the original returns to the site
/// null:
///   mov r11,&CAI_HITS ; lock inc qword [r11]
///   mov al,1 ; ret
/// ```
fn cai_stub(func: usize, counter: usize) -> Vec<u8> {
    let mut s = Vec::with_capacity(48);
    s.extend_from_slice(&[0x48, 0x85, 0xc9]); // test rcx,rcx
    s.extend_from_slice(&[0x74, 0x0c]); // jz +12 (over the 12-byte tail call)
    s.extend_from_slice(&[0x48, 0xb8]);
    s.extend_from_slice(&(func as u64).to_le_bytes());
    s.extend_from_slice(&[0xff, 0xe0]);
    s.extend_from_slice(&[0x49, 0xbb]); // mov r11, imm64
    s.extend_from_slice(&(counter as u64).to_le_bytes());
    s.extend_from_slice(&[0xf0, 0x49, 0xff, 0x03]); // lock inc qword [r11]
    s.extend_from_slice(&[0xb0, 0x01, 0xc3]); // mov al,1 ; ret
    s
}

unsafe fn install_cai(base: usize) -> Result<(), String> {
    use crate::engineguard::{alloc_near, bytes_at, write_code_for};
    let func = base + CAI_FUNC_RVA;
    if bytes_at::<24>(func) != Some(CAI_FUNC_BYTES) { return Err(format!("function bytes at {func:#x} differ")); }
    // every site must be `call func` before anything is written
    let mut todo = Vec::new();
    for rva in CAI_SITES {
        let site = base + rva;
        match bytes_at::<5>(site) {
            Some(b) if b[0] == 0xe8 => {
                let rel = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
                if (site as isize + 5 + rel as isize) as usize == func { todo.push(site); } else { log!("cai null guard: call at {site:#x} does not target the function, skipped"); }
            }
            _ => log!("cai null guard: bytes at {site:#x} differ, skipped"),
        }
    }
    if todo.is_empty() { return Err("no call site matched".into()); }
    let stub = alloc_near(base + CAI_SITES[0]).ok_or("no free page within rel32 reach")?;
    let code = cai_stub(func, &CAI_HITS as *const AtomicU64 as usize);
    core::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
    crate::engineguard::make_executable(stub, code.len());
    let mut done = 0;
    for site in todo {
        let disp = stub as i64 - (site as i64 + 5);
        if disp < i32::MIN as i64 || disp > i32::MAX as i64 { log!("cai null guard: stub out of reach of {site:#x}"); continue; }
        let mut call = [0u8; 5];
        call[0] = 0xe8;
        call[1..].copy_from_slice(&(disp as i32).to_le_bytes());
        match write_code_for("cai_null_guard", "cai_null_guard", site, &call) {
            Ok(()) => done += 1,
            Err(e) => log!("cai null guard: write at {site:#x} failed: {e}"),
        }
    }
    if done == 0 { return Err("no site written".into()); }
    CAI_STUB.store(stub, Ordering::Relaxed);
    CAI_SITES_PATCHED.store(done, Ordering::Relaxed);
    Ok(())
}

pub fn install(t: &Table) {
    let base = t.base();
    BASE.store(base, Ordering::Relaxed);
    // SAFETY: each patch verifies its bytes first and writes only with the other threads frozen.
    unsafe {
        if crate::build::hook_enabled("cai_null_guard") {
            match install_cai(base) {
                Ok(()) => log!("cai null guard installed on {} call sites", CAI_SITES_PATCHED.load(Ordering::Relaxed)),
                Err(e) => log!("cai null guard not installed: {e}"),
            }
        } else {
            log!("cai null guard off (cai_null_guard=0 in script_extender.cfg)");
        }
        if crate::build::hook_enabled("battle_xform_fast") {
            match install_xform(base) {
                Ok(()) => log!("battle instance transform (unrolled, bit-exact) installed at exe+{XFORM_RVA:#x}"),
                Err(e) => log!("battle instance transform not installed: {e}"),
            }
        } else {
            log!("battle instance transform off (battle_xform_fast=0 in script_extender.cfg)");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Lua
// ---------------------------------------------------------------------------------------------

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_sepatches_info", se_sepatches_info);
    lua::set_global_fn(l, "se_sepatches_selftest", se_sepatches_selftest);
}

fn info() -> String {
    format!(
        "cai_null_guard={};cai_sites={};cai_hits={};battle_xform={};xform_calls={};xform_fallbacks={}",
        (CAI_STUB.load(Ordering::Relaxed) != 0) as u8, CAI_SITES_PATCHED.load(Ordering::Relaxed), CAI_HITS.load(Ordering::Relaxed),
        XFORM_ON.load(Ordering::Relaxed) as u8, XFORM_CALLS.load(Ordering::Relaxed), XFORM_FALLBACKS.load(Ordering::Relaxed),
    )
}

/// se_sepatches_info() -> "cai_null_guard=1;cai_sites=3;cai_hits=0;battle_xform=1;xform_calls=N;xform_fallbacks=0"
unsafe extern "C" fn se_sepatches_info(l: *mut LuaState) -> c_int {
    lua::push_str(l, &info());
    1
}

/// xorshift64*, seeded by the caller: tests are repeatable
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// a float from a mix of "interesting" distributions
    fn float(&mut self) -> f32 {
        let r = self.next();
        let v = (r >> 40) as u32;
        match r & 15 {
            0 => 0.0,
            1 => 1.0,
            2 => -1.0,
            3 => (v % 9) as f32 - 4.0,
            4 => f32::from_bits((r >> 8) as u32 & 0x007f_ffff), // subnormal
            5 => f32::from_bits((r >> 8) as u32), // anything, incl. inf / NaN
            6 => (v as f32 / 16_777_216.0 - 0.5) * 1e-20,
            7 => (v as f32 / 16_777_216.0 - 0.5) * 1e18,
            _ => (v as f32 / 16_777_216.0 - 0.5) * 8.0,
        }
    }
}

fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    // NaN payloads: compare 4-byte lanes, two NaNs count as equal (see the module note)
    a.len() == b.len() && a.chunks(4).zip(b.chunks(4)).all(|(x, y)| x == y || (x.len() == 4 && is_nan_bits(x) && is_nan_bits(y)))
}
fn is_nan_bits(x: &[u8]) -> bool {
    let v = u32::from_le_bytes([x[0], x[1], x[2], x[3]]);
    (v & 0x7f80_0000) == 0x7f80_0000 && (v & 0x007f_ffff) != 0
}

/// se_sepatches_selftest([n]) -> ok, message
///   Runs n random records (default 20000, max 2,000,000) through the engine's original
///   function (via the detour's trampoline) and through the unrolled one, and compares all 0x74
///   output bytes; every 16th matrix is made singular to reach the |det| < FLT_MIN path. Also
///   re-reads the three CAI call sites. Needs both patches installed; nothing of the game's own
///   state is touched (private buffers only).
unsafe extern "C" fn se_sepatches_selftest(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let n = {
        let v = (api.tonumber)(l, 1);
        if v >= 1.0 && v <= 2_000_000.0 { v as u32 } else { 20_000 }
    };
    let base = BASE.load(Ordering::Relaxed);
    let mut lines: Vec<String> = Vec::new();
    let mut ok = true;
    match XFORM.get() {
        Some(h) if XFORM_ON.load(Ordering::Relaxed) => {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ n as u64);
            let (mut bad, mut singular) = (0u32, 0u32);
            let mut first_bad = String::new();
            for t in 0..n {
                let mut m = [0f32; 12];
                for x in m.iter_mut() { *x = rng.float(); }
                if t % 16 == 0 {
                    // rank-deficient: row 2 = row 0 (+ row 1 every other time)
                    let k = if t % 32 == 0 { 1.0 } else { 0.0 };
                    for c in 0..4 { m[8 + c] = m[c] + k * m[4 + c]; }
                    singular += 1;
                }
                let pair = [rng.next() as u32, rng.next() as u32];
                let (a, b) = (rng.float(), rng.float());
                let vec = [rng.float(), rng.float(), rng.float()];
                let flag = (rng.next() & 0xff) as usize;
                let (mut r1, mut r2) = ([0xa5u8; 0x80], [0xa5u8; 0x80]);
                h.call(r1.as_mut_ptr(), m.as_ptr() as *const u8, pair.as_ptr() as *const u8, a, b, vec.as_ptr(), flag);
                fill_fast(r2.as_mut_ptr(), m.as_ptr() as *const u8, pair.as_ptr() as *const u8, a, b, vec.as_ptr(), flag);
                if !same_bytes(&r1, &r2) {
                    bad += 1;
                    if first_bad.is_empty() { first_bad = format!("{m:?}"); }
                }
            }
            ok &= bad == 0;
            lines.push(format!("xform: {n} records ({singular} singular), mismatches={bad}{}", if bad > 0 { format!(" first={first_bad}") } else { String::new() }));
        }
        _ => {
            ok = false;
            lines.push("xform: not installed".into());
        }
    }
    if CAI_STUB.load(Ordering::Relaxed) != 0 {
        let stub = CAI_STUB.load(Ordering::Relaxed);
        let mut redirected = 0;
        for rva in CAI_SITES {
            let site = base + rva;
            let rel = core::ptr::read_unaligned((site + 1) as *const i32);
            if *(site as *const u8) == 0xe8 && (site as isize + 5 + rel as isize) as usize == stub { redirected += 1; }
        }
        let pass = redirected == CAI_SITES_PATCHED.load(Ordering::Relaxed);
        // the stub itself, called like the engine would: NULL answers true and counts, a live
        // object reaches the original function (no record at +0x178 -> false)
        let f: unsafe extern "C" fn(usize) -> u8 = core::mem::transmute(stub);
        let h0 = CAI_HITS.load(Ordering::Relaxed);
        let null_answer = f(0);
        let obj = [0u8; 0x200];
        let live_answer = f(obj.as_ptr() as usize);
        let stub_ok = null_answer == 1 && live_answer == 0 && CAI_HITS.load(Ordering::Relaxed) == h0 + 1;
        ok &= pass && stub_ok;
        lines.push(format!("cai: sites redirected={redirected}/{} stub(null)={null_answer} stub(live empty)={live_answer} pass={}", CAI_SITES_PATCHED.load(Ordering::Relaxed), pass && stub_ok));
    } else {
        ok = false;
        lines.push("cai: not installed".into());
    }
    let msg = lines.join("; ");
    log!("se_sepatches_selftest: ok={ok}: {msg}");
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &msg);
    let _ = (P::default_marker(), );
    2
}

trait Marker { fn default_marker() -> u8 { 0 } }
impl Marker for P {}

#[cfg(test)]
mod tests;
