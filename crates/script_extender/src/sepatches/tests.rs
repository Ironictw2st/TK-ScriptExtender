use super::*;
use core::ffi::c_void;

extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
}

#[test]
fn cai_stub_layout() {
    let s = cai_stub(0x1111_2222_3333_4444, 0x5555_6666_7777_8888);
    assert_eq!(&s[..3], &[0x48, 0x85, 0xc9]);
    // jz skips exactly "mov rax,imm64 ; jmp rax" (12 bytes) and lands on "mov r11,imm64"
    assert_eq!(&s[3..5], &[0x74, 0x0c]);
    assert_eq!(&s[5..7], &[0x48, 0xb8]);
    assert_eq!(&s[7..15], &0x1111_2222_3333_4444u64.to_le_bytes());
    assert_eq!(&s[15..17], &[0xff, 0xe0]);
    assert_eq!(&s[17..19], &[0x49, 0xbb]);
    assert_eq!(&s[19..27], &0x5555_6666_7777_8888u64.to_le_bytes());
    assert_eq!(&s[27..31], &[0xf0, 0x49, 0xff, 0x03]);
    assert_eq!(&s[31..], &[0xb0, 0x01, 0xc3]);
    assert_eq!(5 + 0x0c, 17);
}

#[test]
fn cai_sites_are_calls_to_the_function_in_the_notes() {
    // the three sites, as seen in the exe on 5 Nov 2026 (rel32 of each call)
    let rels: [(usize, i32); 3] = [(0x1d25dcc, 0x1312df), (0x1d25e6c, 0x13123f), (0x1d32114, 0x124f97)];
    for (site, rel) in rels {
        assert!(CAI_SITES.contains(&site));
        assert_eq!((site as isize + 5 + rel as isize) as usize, CAI_FUNC_RVA);
    }
}

#[test]
fn identity_and_translation() {
    let id = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    assert_eq!(inverse_rows(&id), id);
    // diag(2,4,8) with translation (1,2,3) in column 3
    let m = [2.0, 0.0, 0.0, 1.0, 0.0, 4.0, 0.0, 2.0, 0.0, 0.0, 8.0, 3.0];
    let inv = inverse_rows(&m);
    let mul = |a: &[f32; 12], b: &[f32; 12]| -> [f32; 12] {
        let mut o = [0f32; 12];
        for r in 0..3 {
            for c in 0..4 {
                let mut s = 0.0;
                for k in 0..3 {
                    s += a[r * 4 + k] * b[k * 4 + c];
                }
                if c == 3 {
                    s += a[r * 4 + 3];
                }
                o[r * 4 + c] = s;
            }
        }
        o
    };
    let p = mul(&m, &inv);
    for (x, y) in p.iter().zip(id.iter()) {
        assert!((x - y).abs() < 1e-6, "{p:?}");
    }
    // singular -> the matrix itself
    let sing = [1.0, 2.0, 3.0, 4.0, 2.0, 4.0, 6.0, 8.0, 0.0, 0.0, 0.0, 0.0];
    assert_eq!(inverse_rows(&sing), sing);
}

#[test]
fn unrolled_matches_loop_reference() {
    let mut rng = Rng(42);
    for t in 0..400_000u32 {
        let (m, ..) = random_case(&mut rng, t);
        let (a, b) = (inverse_rows(&m), inverse_rows_unrolled(&m));
        let (ab, bb): (Vec<u32>, Vec<u32>) = (a.iter().map(|x| x.to_bits()).collect(), b.iter().map(|x| x.to_bits()).collect());
        assert!(ab == bb || a.iter().zip(b.iter()).all(|(x, y)| x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())), "{m:?}");
    }
}

#[test]
fn colour_rounding() {
    assert_eq!(colour_byte(0.0), 0); // x = -0.5 -> trunc 0
    assert_eq!(colour_byte(1.0), 255);
    assert_eq!(colour_byte(0.5), 128); // 127.5 + 0.5 = 128
    assert_eq!(colour_byte(-1.0), 1); // -255.5 -> -255 -> low byte 0x01
    assert_eq!(colour_byte(f32::NAN), 0); // 0x80000000 -> low byte 0
    assert_eq!(colour_byte(1e30), 0);
}

// ---- differential test against the exe's own function ------------------------------------------

const EXE: &str = r"C:\Program Files (x86)\Steam\steamapps\common\Total War THREE KINGDOMS\Three_Kingdoms.exe";
/// (rva of the instruction, its length, rva of the constant it reads); the disp32 is the last 4 bytes
const RIP_SITES: [(usize, usize, usize); 7] = [
    (0x13778fa, 8, 0x3275760),
    (0x1377959, 9, 0x3263464),
    (0x1377b08, 7, 0x32634d0),
    (0x1377b0f, 7, 0x32b2188),
    (0x1377bc3, 8, 0x3263498),
    (0x1377ed6, 8, 0x32714e8),
    (0x1377eff, 8, 0x3263450),
];

fn rva_to_off(exe: &[u8], rva: usize) -> Option<usize> {
    let rd32 = |o: usize| u32::from_le_bytes(exe[o..o + 4].try_into().unwrap()) as usize;
    let nt = rd32(0x3c);
    let nsec = u16::from_le_bytes([exe[nt + 6], exe[nt + 7]]) as usize;
    let soh = u16::from_le_bytes([exe[nt + 20], exe[nt + 21]]) as usize;
    for i in 0..nsec {
        let s = nt + 24 + soh + i * 40;
        let (va, raw_size, raw_ptr) = (rd32(s + 12), rd32(s + 16), rd32(s + 20));
        if rva >= va && rva < va + raw_size {
            return Some(rva - va + raw_ptr);
        }
    }
    None
}

/// The engine function copied into a private buffer, its seven rip-relative constants re-pointed
/// at a private constants block.
unsafe fn load_original() -> Option<Xform> {
    let exe = std::fs::read(EXE).ok()?;
    let nt = u32::from_le_bytes(exe[0x3c..0x40].try_into().unwrap()) as usize;
    let stamp = u32::from_le_bytes(exe[nt + 8..nt + 12].try_into().unwrap());
    if stamp != 0x69ce_4c84 {
        return None;
    }
    let off = rva_to_off(&exe, XFORM_RVA)?;
    // the hashed 0x735 bytes end at `pop rbp`; the function's `ret` is the byte after them
    let func = &exe[off..off + XFORM_LEN + 1];
    assert_eq!(func[XFORM_LEN], 0xc3);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &func[..XFORM_LEN] {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    assert_eq!(h, XFORM_FNV, "function bytes differ from the pinned hash");
    assert_eq!(&func[..16], &XFORM_HEAD);
    let buf = VirtualAlloc(core::ptr::null_mut(), 0x4000, 0x3000, 0x40) as *mut u8;
    assert!(!buf.is_null());
    core::ptr::copy_nonoverlapping(func.as_ptr(), buf, XFORM_LEN + 1);
    for (k, (rva, len, target)) in RIP_SITES.iter().enumerate() {
        let at = rva - XFORM_RVA;
        let old = i32::from_le_bytes(func[at + len - 4..at + len].try_into().unwrap());
        assert_eq!((*rva + len) as isize + old as isize, *target as isize, "rip table entry {k}");
        let toff = rva_to_off(&exe, *target)?;
        let slot = buf.add(0x1000 + k * 16);
        core::ptr::copy_nonoverlapping(exe[toff..toff + 16].as_ptr(), slot, 16);
        let disp = slot as isize - (buf.add(at + len) as isize);
        assert!(disp.abs() < 0x7fff_0000);
        core::ptr::write_unaligned(buf.add(at + len - 4) as *mut i32, disp as i32);
    }
    Some(core::mem::transmute::<*mut u8, Xform>(buf))
}

fn run_original(f: Xform, m: &[f32; 12], pair: &[u32; 2], a: f32, b: f32, vec: &[f32; 3], flag: usize) -> [u8; 0x80] {
    let mut r = [0xa5u8; 0x80];
    unsafe {
        f(r.as_mut_ptr(), m.as_ptr() as *const u8, pair.as_ptr() as *const u8, a, b, vec.as_ptr(), flag);
    }
    r
}
fn run_fast(m: &[f32; 12], pair: &[u32; 2], a: f32, b: f32, vec: &[f32; 3], flag: usize) -> [u8; 0x80] {
    let mut r = [0xa5u8; 0x80];
    unsafe {
        fill_fast(r.as_mut_ptr(), m.as_ptr() as *const u8, pair.as_ptr() as *const u8, a, b, vec.as_ptr(), flag);
    }
    r
}

type Case = ([f32; 12], [u32; 2], f32, f32, [f32; 3], usize);

fn random_case(rng: &mut Rng, t: u32) -> Case {
    let mut m = [0f32; 12];
    for x in m.iter_mut() {
        *x = rng.float();
    }
    match t % 16 {
        0 => {
            let k = if t % 32 == 0 { 1.0 } else { 0.0 };
            for c in 0..4 {
                m[8 + c] = m[c] + k * m[4 + c];
            }
        }
        // a plausible instance matrix: positive scale on the diagonal, translation random
        1..=4 => {
            for r in 0..3 {
                for c in 0..3 {
                    m[r * 4 + c] = if r == c { 0.2 + (rng.next() % 1000) as f32 / 250.0 } else { 0.0 };
                }
            }
        }
        _ => {}
    }
    let pair = [rng.next() as u32, rng.next() as u32];
    (m, pair, rng.float(), rng.float(), [rng.float(), rng.float(), rng.float()], (rng.next() & 0xff) as usize)
}

#[test]
fn xform_matches_exe() {
    let Some(orig) = (unsafe { load_original() }) else {
        eprintln!("exe 1.7.2.0 not found at {EXE}: differential test skipped");
        return;
    };
    let n: u32 = std::env::var("SEPATCH_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let (mut exact, mut nan_only, mut singular) = (0u64, 0u64, 0u64);
    for t in 0..n {
        let (m, pair, a, b, vec, flag) = random_case(&mut rng, t);
        let r1 = run_original(orig, &m, &pair, a, b, &vec, flag);
        let r2 = run_fast(&m, &pair, a, b, &vec, flag);
        if r1[..] == r2[..] {
            exact += 1;
        } else {
            assert!(
                same_bytes(&r1, &r2),
                "mismatch for {m:?} pair {pair:?} a {a} b {b} vec {vec:?}\n orig {:02x?}\n fast {:02x?}",
                &r1[..0x74],
                &r2[..0x74]
            );
            nan_only += 1;
        }
        let mb: [u8; 0x30] = unsafe { core::mem::transmute(m) };
        if r1[0x30..0x60] == mb[..] {
            singular += 1;
        }
    }
    eprintln!("xform differential: {n} records, {exact} byte-identical, {nan_only} identical up to NaN payload, {singular} took the singular path");
    assert!(singular > 0 && exact > (n as u64) * 9 / 10);
}

#[test]
fn xform_matches_exe_edge_cases() {
    let Some(orig) = (unsafe { load_original() }) else { return };
    let specials = [
        0.0f32,
        -0.0,
        1.0,
        -1.0,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        1e-30,
        3e38,
        -3e38,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        0.5,
        255.0,
        1.0 / 255.0,
    ];
    let mut rng = Rng(77);
    for t in 0..200_000u32 {
        let mut m = [0f32; 12];
        for x in m.iter_mut() {
            *x = specials[(rng.next() % specials.len() as u64) as usize];
        }
        let vec = [
            specials[(rng.next() % 15) as usize],
            specials[(rng.next() % 15) as usize],
            specials[(rng.next() % 15) as usize],
        ];
        let pair = [t, !t];
        let r1 = run_original(orig, &m, &pair, 1.5, -2.5, &vec, 7);
        let r2 = run_fast(&m, &pair, 1.5, -2.5, &vec, 7);
        assert!(
            same_bytes(&r1, &r2),
            "mismatch for {m:?} vec {vec:?}\n orig {:02x?}\n fast {:02x?}",
            &r1[..0x74],
            &r2[..0x74]
        );
    }
}

#[test]
#[ignore]
fn xform_speed() {
    let Some(orig) = (unsafe { load_original() }) else { return };
    let mut rng = Rng(5);
    let cases: Vec<Case> = (0..4096u32).map(|t| random_case(&mut rng, t * 16 + 1)).collect();
    let rounds = 400;
    let mut sink = 0u8;
    let t0 = std::time::Instant::now();
    for _ in 0..rounds {
        for (m, pair, a, b, vec, flag) in &cases {
            sink ^= run_original(orig, m, pair, *a, *b, vec, *flag)[0x10];
        }
    }
    let t_orig = t0.elapsed();
    let t0 = std::time::Instant::now();
    for _ in 0..rounds {
        for (m, pair, a, b, vec, flag) in &cases {
            sink ^= run_fast(m, pair, *a, *b, vec, *flag)[0x10];
        }
    }
    let t_fast = t0.elapsed();
    let calls = (rounds * cases.len()) as f64;
    eprintln!(
        "original {:.1} ns/call, unrolled {:.1} ns/call, x{:.1} (sink {sink})",
        t_orig.as_nanos() as f64 / calls,
        t_fast.as_nanos() as f64 / calls,
        t_orig.as_secs_f64() / t_fast.as_secs_f64()
    );
}

