//! Sampling profiler for performance work (UI hitches, AI turn times). Diagnostic only: it reads
//! thread contexts and stacks, changes nothing in the game.
//!
//! `se_profile_start(seconds, delay_seconds, label)`: a sampler thread waits `delay`, then every
//! millisecond suspends each of the busiest game threads in turn (re-picked every second by
//! GetThreadTimes), takes the register context (GetThreadContext), copies the top 8 KB of the
//! stack (ReadProcessMemory on ourselves: a fault cannot crash us) and resumes the thread. While
//! a thread is suspended the sampler neither allocates nor logs (the target may hold the heap or
//! log lock). After the resume, the copied stack is unwound with the exe's own unwind tables
//! (.pdata / UNWIND_INFO), so the call chains are real, not guessed from stack contents. Frames
//! outside the exe (system DLLs, drivers) are skipped by scanning up to the first return address
//! into the exe that follows a call instruction.
//!
//! Output in `<dll folder>\..\profiles\` (kept when the mod manager removes old version folders):
//!   profile_<label>.txt         per thread: self % and inclusive % per function (Ghidra addresses)
//!   profile_<label>.folded.txt  folded call stacks `thread;root;...;leaf count` (flame-graph input,
//!                               read by tools/profile_tree.py)

use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

const TH32CS_SNAPTHREAD: u32 = 0x4;
const THREAD_ACCESS: u32 = 0x2 | 0x8 | 0x40; // SUSPEND_RESUME | GET_CONTEXT | QUERY_INFORMATION
const CONTEXT_FULL: u32 = 0x0010_0003; // CONTROL | INTEGER
const STACK_BYTES: usize = 8192;
const MAX_THREADS: usize = 4;
const MAX_FRAMES: usize = 40;

#[repr(C)]
struct ThreadEntry32 { dw_size: u32, cnt_usage: u32, tid: u32, owner: u32, base_pri: i32, delta_pri: i32, flags: u32 }

#[repr(C, align(16))]
struct Ctx([u8; 1232]);

#[repr(C)]
#[derive(Clone, Copy)]
struct FileTime { lo: u32, hi: u32 }

extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
    fn Thread32First(snap: *mut c_void, e: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snap: *mut c_void, e: *mut ThreadEntry32) -> i32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
    fn SuspendThread(h: *mut c_void) -> u32;
    fn ResumeThread(h: *mut c_void) -> u32;
    fn GetThreadContext(h: *mut c_void, ctx: *mut Ctx) -> i32;
    fn GetThreadTimes(h: *mut c_void, c: *mut FileTime, e: *mut FileTime, k: *mut FileTime, u: *mut FileTime) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcess() -> *mut c_void;
    fn ReadProcessMemory(p: *mut c_void, addr: *const c_void, buf: *mut c_void, n: usize, got: *mut usize) -> i32;
    fn Sleep(ms: u32);
}

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(ms: u32) -> u32;
    fn timeEndPeriod(ms: u32) -> u32;
}

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_profile_start", se_profile_start);
    lua::set_global_fn(l, "se_profile_stop", se_profile_stop);
}

/// se_profile_stop() -> ok, msg : end the running profile now; its reports are still written.
unsafe extern "C" fn se_profile_stop(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let running = RUNNING.load(Ordering::SeqCst);
    if running { STOP.store(true, Ordering::SeqCst); }
    (api.pushboolean)(l, running as c_int);
    lua::push_str(l, if running { "stopping; the reports are written in a moment" } else { "no profile is running" });
    2
}

/// Unwound sample: frames[0] = the function RIP was in (0 when outside the exe), then callers.
struct Sample { thread: u16, n: u8, frames: [u32; MAX_FRAMES] }

unsafe fn cpu_100ns(h: *mut c_void) -> u64 {
    let z = FileTime { lo: 0, hi: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 { return 0; }
    (((k.hi as u64) << 32) | k.lo as u64) + (((u.hi as u64) << 32) | u.lo as u64)
}

unsafe fn threads() -> Vec<(u32, *mut c_void)> {
    let (pid, me) = (GetCurrentProcessId(), GetCurrentThreadId());
    let mut out = Vec::new();
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if snap.is_null() || snap as usize == usize::MAX { return out; }
    let mut e = ThreadEntry32 { dw_size: core::mem::size_of::<ThreadEntry32>() as u32, cnt_usage: 0, tid: 0, owner: 0, base_pri: 0, delta_pri: 0, flags: 0 };
    if Thread32First(snap, &mut e) != 0 {
        loop {
            if e.owner == pid && e.tid != me {
                let h = OpenThread(THREAD_ACCESS, 0, e.tid);
                if !h.is_null() { out.push((e.tid, h)); }
            }
            if Thread32Next(snap, &mut e) == 0 { break; }
        }
    }
    CloseHandle(snap);
    out
}

/// The exe's RUNTIME_FUNCTION table, sorted by begin: (begin, end, unwind info rva).
unsafe fn runtime_functions(base: usize) -> Vec<(u32, u32, u32)> {
    let nt = base + *((base + 0x3c) as *const i32) as usize;
    let (rva, size) = (*((nt + 0x18 + 0x70 + 3 * 8) as *const u32) as usize, *((nt + 0x18 + 0x70 + 3 * 8 + 4) as *const u32) as usize);
    let mut v: Vec<(u32, u32, u32)> = (0..size / 12)
        .map(|i| { let p = (base + rva + i * 12) as *const u32; (*p, *p.add(1), *p.add(2)) })
        .filter(|f| f.0 != 0)
        .collect();
    v.sort_unstable();
    v
}

fn lookup(rfs: &[(u32, u32, u32)], rva: u32) -> Option<(u32, u32, u32)> {
    let i = match rfs.binary_search_by(|f| f.0.cmp(&rva)) { Ok(i) => i, Err(0) => return None, Err(i) => i - 1 };
    if rva < rfs[i].1 { Some(rfs[i]) } else { None }
}

/// Follows chained unwind infos to the entry that starts the function (its begin is the
/// function start Ghidra knows).
unsafe fn function_start(base: usize, mut rf: (u32, u32, u32)) -> u32 {
    for _ in 0..8 {
        let info = base + rf.2 as usize;
        let flags = *(info as *const u8) >> 3;
        if flags & 4 == 0 { break; }
        let count = *((info + 2) as *const u8) as usize;
        let chained = (info + 4 + ((count + 1) & !1) * 2) as *const u32;
        rf = (*chained, *chained.add(1), *chained.add(2));
    }
    rf.0
}

struct Snapshot<'a> { base: usize, bytes: &'a [u8] }
impl Snapshot<'_> {
    fn read(&self, addr: usize) -> Option<usize> {
        let off = addr.checked_sub(self.base)?;
        if off + 8 > self.bytes.len() { return None; }
        Some(usize::from_le_bytes(self.bytes[off..off + 8].try_into().ok()?))
    }
}

/// One unwind step for a frame inside the exe. `regs` = rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi,
/// r8..r15. Returns the caller's rip (rsp / nonvolatile registers are updated in place).
unsafe fn unwind_step(base: usize, rfs: &[(u32, u32, u32)], rip: usize, regs: &mut [usize; 16], st: &Snapshot) -> Option<usize> {
    let rva = (rip - base) as u32;
    let Some(mut rf) = lookup(rfs, rva) else {
        // leaf function: the return address is at [rsp]
        let ret = st.read(regs[4])?;
        regs[4] += 8;
        return Some(ret);
    };
    let mut in_prolog_of_first = true;
    for _ in 0..8 {
        let info = base + rf.2 as usize;
        let flags = *(info as *const u8) >> 3;
        let prolog = *((info + 1) as *const u8) as u32;
        let count = *((info + 2) as *const u8) as usize;
        let frame = *((info + 3) as *const u8);
        let (frame_reg, frame_off) = ((frame & 0xf) as usize, ((frame >> 4) as usize) * 16);
        let offset_in_fn = if in_prolog_of_first { rva - rf.0 } else { u32::MAX };
        let in_prolog = offset_in_fn < prolog;
        // Body of a frame-pointer function: everything is relative to the frame register.
        if frame_reg != 0 && !in_prolog {
            regs[4] = regs[frame_reg].wrapping_sub(frame_off);
        }
        let codes = (info + 4) as *const u8;
        let mut i = 0;
        while i < count {
            let code_offset = *codes.add(i * 2) as u32;
            let op = *codes.add(i * 2 + 1) & 0xf;
            let opinfo = (*codes.add(i * 2 + 1) >> 4) as usize;
            let slots = match op { 1 => if opinfo == 0 { 2 } else { 3 }, 4 | 8 => 2, 5 | 9 => 3, _ => 1 };
            let executed = !in_prolog || code_offset <= offset_in_fn;
            if executed {
                match op {
                    0 => { regs[opinfo] = st.read(regs[4])?; regs[4] += 8; }
                    1 => {
                        let size = if opinfo == 0 {
                            (*(codes.add((i + 1) * 2) as *const u16)) as usize * 8
                        } else {
                            *(codes.add((i + 1) * 2) as *const u32) as usize
                        };
                        regs[4] += size;
                    }
                    2 => regs[4] += opinfo * 8 + 8,
                    3 => {} // handled above (body) / not yet established (prolog)
                    4 => { let off = (*(codes.add((i + 1) * 2) as *const u16)) as usize * 8; if let Some(v) = st.read(regs[4] + off) { regs[opinfo] = v; } }
                    5 => { let off = *(codes.add((i + 1) * 2) as *const u32) as usize; if let Some(v) = st.read(regs[4] + off) { regs[opinfo] = v; } }
                    8 | 9 => {}
                    10 => return None, // machine frame: stop
                    _ => return None,
                }
            }
            i += slots;
        }
        if flags & 4 == 0 { break; }
        let chained = (info + 4 + ((count + 1) & !1) * 2) as *const u32;
        rf = (*chained, *chained.add(1), *chained.add(2));
        in_prolog_of_first = false;
    }
    let ret = st.read(regs[4])?;
    regs[4] += 8;
    Some(ret)
}

/// True when the bytes before `ret` look like a call instruction (so `ret` is a return address).
unsafe fn follows_call(ret: usize) -> bool {
    let p = ret as *const u8;
    *p.sub(5) == 0xe8                                   // call rel32
        || (*p.sub(2) == 0xff && (*p.sub(1) & 0x38) == 0x10)   // call reg / [reg]
        || (*p.sub(3) == 0xff && (*p.sub(2) & 0x38) == 0x10)   // call [reg+disp8]
        || (*p.sub(6) == 0xff && (*p.sub(5) & 0x38) == 0x10)   // call [rip+disp32] / [reg+disp32]
        || (*p.sub(7) == 0xff && (*p.sub(6) & 0x38) == 0x10)
}

unsafe fn unwind(base: usize, size: usize, rfs: &[(u32, u32, u32)], ctx: &Ctx, stack: &[u8], out: &mut Sample) {
    let mut regs = [0usize; 16];
    for (i, r) in regs.iter_mut().enumerate() { *r = core::ptr::read_unaligned(ctx.0.as_ptr().add(0x78 + i * 8) as *const usize); }
    let mut rip = core::ptr::read_unaligned(ctx.0.as_ptr().add(0xf8) as *const usize);
    let st = Snapshot { base: regs[4], bytes: stack };
    let in_exe = |a: usize| a >= base + 0x1000 && a < base + size;
    let func = |rip: usize| lookup(rfs, (rip - base) as u32).map(|rf| function_start(base, rf)).unwrap_or((rip - base) as u32);
    out.n = 0;
    if !in_exe(rip) {
        // RIP outside the exe (system call, driver, wait): mark it with a 0 frame, then continue
        // from the first return address into the exe that follows a call instruction.
        out.frames[0] = 0;
        out.n = 1;
        // A stale return address can sit near the top of the stack (0.31.x blamed a tiny faction
        // check for 19% of an end turn that way): a candidate is only accepted when unwinding
        // from it yields at least three more frames inside the exe.
        let mut found = None;
        let mut a = regs[4];
        while let Some(v) = st.read(a) {
            if in_exe(v) && follows_call(v) {
                let mut trial = regs;
                trial[4] = a + 8;
                let (mut r, mut depth) = (v, 0);
                while depth < 3 {
                    match unwind_step(base, rfs, r, &mut trial, &st) { Some(n) if in_exe(n) => { r = n; depth += 1; } _ => break }
                }
                if depth >= 3 { found = Some((v, a + 8)); break; }
            }
            a += 8;
        }
        let Some((v, sp)) = found else { return };
        rip = v;
        regs[4] = sp;
    }
    while (out.n as usize) < MAX_FRAMES && in_exe(rip) {
        out.frames[out.n as usize] = func(rip);
        out.n += 1;
        let Some(next) = unwind_step(base, rfs, rip, &mut regs, &st) else { break };
        rip = next;
    }
}

unsafe fn run(seconds: u32, delay: u32, label: String) {
    Sleep(delay * 1000);
    let (base, size) = crate::process::main_module();
    let rfs = runtime_functions(base);
    let all = threads();
    // The busiest threads are re-picked every second (a turn's model thread only becomes busy
    // after the player ends the turn); re-picking happens while nothing is suspended.
    let first: Vec<u64> = all.iter().map(|(_, h)| cpu_100ns(*h)).collect();
    let mut last = first.clone();
    Sleep(400);
    let mut picked: Vec<usize> = Vec::with_capacity(MAX_THREADS);
    let repick = |last: &mut Vec<u64>, picked: &mut Vec<usize>| {
        let mut busy: Vec<(u64, usize)> = all.iter().enumerate().map(|(i, (_, h))| { let c = cpu_100ns(*h); let d = c.saturating_sub(last[i]); last[i] = c; (d, i) }).collect();
        busy.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        picked.clear();
        picked.extend(busy.iter().take(MAX_THREADS).filter(|(d, _)| *d > 0).map(|(_, i)| *i));
    };
    repick(&mut last, &mut picked);
    log!("profiler '{label}': {} threads, {} unwind entries, sampling the {MAX_THREADS} busiest (re-picked every second) for {seconds} s", all.len(), rfs.len());

    // The run is bounded by wall-clock time, not by a tick count: Sleep(1) lasts ~15 ms unless
    // the timer resolution is raised, which is done for the duration of the run.
    let ticks = seconds as usize * 1000;
    let mut samples: Vec<Sample> = Vec::with_capacity(ticks * MAX_THREADS + 16);
    timeBeginPeriod(1);
    let deadline = std::time::Duration::from_secs(seconds as u64);
    let mut next_repick = std::time::Duration::from_secs(1);
    let mut ctx = Ctx([0u8; 1232]);
    let mut stack = vec![0u8; STACK_BYTES];
    let me = GetCurrentProcess();
    let started = std::time::Instant::now();
    loop {
        let elapsed = started.elapsed();
        if elapsed >= deadline || STOP.load(Ordering::SeqCst) { break; }
        if elapsed >= next_repick { repick(&mut last, &mut picked); next_repick += std::time::Duration::from_secs(1); }
        for ti in picked.iter() {
            let h = &all[*ti].1;
            if samples.len() == samples.capacity() { break; }
            let mut got = 0usize;
            let mut ok = false;
            // ---- nothing between suspend and resume may allocate, lock or log ----
            if SuspendThread(*h) == u32::MAX { continue; }
            core::ptr::write_unaligned(ctx.0.as_mut_ptr().add(0x30) as *mut u32, CONTEXT_FULL);
            if GetThreadContext(*h, &mut ctx) != 0 {
                let rsp = core::ptr::read_unaligned(ctx.0.as_ptr().add(0x98) as *const usize);
                // the stack may end before 8 KB: fall back to smaller reads
                for len in [STACK_BYTES, 2048, 512] {
                    if ReadProcessMemory(me, rsp as *const c_void, stack.as_mut_ptr() as *mut c_void, len, &mut got) != 0 { break; }
                    got = 0;
                }
                ok = true;
            }
            ResumeThread(*h);
            // -----------------------------------------------------------------------
            if !ok { continue; }
            let mut s = Sample { thread: *ti as u16, n: 0, frames: [0; MAX_FRAMES] };
            unwind(base, size, &rfs, &ctx, &stack[..got], &mut s);
            samples.push(s);
        }
        Sleep(1);
    }
    timeEndPeriod(1);
    STOP.store(false, Ordering::SeqCst);
    let wall = started.elapsed().as_secs_f32();

    // aggregate
    let ghidra = |f: u32| 0x1_4000_0000u64 + f as u64;
    let mut report = format!("profile '{label}': {seconds} s nominal, {:.1} s wall, {} samples, exe base {:#x}\naddresses are Ghidra function starts; 'inclusive' = the function was somewhere on the unwound call stack\n", wall, samples.len(), base);
    let mut folded: HashMap<(u32, Vec<u32>), u32> = HashMap::new();
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_unstable_by_key(|i| std::cmp::Reverse(samples.iter().filter(|s| s.thread as usize == *i).count()));
    for ti in order {
        let (tid, cpu) = (all[ti].0, cpu_100ns(all[ti].1).saturating_sub(first[ti]));
        let mine: Vec<&Sample> = samples.iter().filter(|s| s.thread as usize == ti).collect();
        if mine.len() < 50 { continue; }
        let (mut selfc, mut incl): (HashMap<u32, u32>, HashMap<u32, u32>) = (HashMap::new(), HashMap::new());
        let (mut outside, mut depth) = (0u32, 0u64);
        for s in &mine {
            let frames = &s.frames[..s.n as usize];
            if frames[0] != 0 { *selfc.entry(frames[0]).or_insert(0) += 1; } else { outside += 1; }
            let mut seen: [u32; MAX_FRAMES] = [0; MAX_FRAMES];
            let mut k = 0;
            for f in frames.iter().filter(|f| **f != 0) {
                if !seen[..k].contains(f) { seen[k] = *f; k += 1; *incl.entry(*f).or_insert(0) += 1; }
            }
            depth += k as u64;
            let mut chain: Vec<u32> = frames.iter().rev().copied().filter(|f| *f != 0).collect();
            if frames[0] == 0 { chain.push(0); }
            *folded.entry((tid, chain)).or_insert(0) += 1;
        }
        let n = mine.len() as f32;
        report += &format!("\n=== thread {tid}: {} samples, {} ms cpu during the run, {:.0}% of samples with RIP outside the exe, average unwound depth {:.1}\n", mine.len(), cpu / 10_000, outside as f32 * 100.0 / n, depth as f32 / n);
        for (title, map) in [("self (RIP inside the function)", &selfc), ("inclusive (function on the call stack)", &incl)] {
            let mut rows: Vec<(&u32, &u32)> = map.iter().collect();
            rows.sort_unstable_by(|a, b| b.1.cmp(a.1));
            report += &format!("  -- {title}\n");
            for (f, c) in rows.iter().take(60) {
                report += &format!("     {:5.1}%  {:6}  0x{:x}\n", **c as f32 * 100.0 / n, c, ghidra(**f));
            }
        }
    }
    for (_, h) in all { CloseHandle(h); }
    let mut lines: Vec<String> = folded.iter().map(|((tid, chain), c)| {
        let names: Vec<String> = chain.iter().map(|f| if *f == 0 { "outside".to_string() } else { format!("{:x}", ghidra(*f)) }).collect();
        format!("t{};{} {}", tid, names.join(";"), c)
    }).collect();
    lines.sort();
    // The mod manager deletes old version folders (the 0.31.2 reports were lost that way), so
    // reports go to <dll folder>\..\profiles when the DLL sits in a version folder.
    let dir = crate::process::self_dir().map(|d| {
        let target = d.parent().map(|p| p.join("profiles")).unwrap_or_else(|| d.clone());
        if std::fs::create_dir_all(&target).is_ok() { target } else { d }
    });
    for (name, text) in [(format!("profile_{label}.txt"), report), (format!("profile_{label}.folded.txt"), lines.join("\n"))] {
        match dir.as_ref().map(|d| d.join(&name)) {
            Some(p) => match std::fs::write(&p, text) {
                Ok(()) => log!("profiler '{label}': wrote {}", p.display()),
                Err(e) => log!("profiler '{label}': could not write {}: {e}", p.display()),
            },
            None => log!("profiler '{label}': no DLL folder for {name}"),
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
}

/// se_profile_start(seconds, delay_seconds, label) -> ok, msg
unsafe extern "C" fn se_profile_start(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let seconds = ((api.tointeger)(l, 1) as i64).clamp(1, 120) as u32;
    let delay = ((api.tointeger)(l, 2) as i64).clamp(0, 60) as u32;
    let raw = lua::to_str(l, 3);
    let label: String = raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(40).collect();
    let label = if label.is_empty() { "run".to_string() } else { label };
    if RUNNING.swap(true, Ordering::SeqCst) {
        (api.pushboolean)(l, 0);
        lua::push_str(l, "a profile is already running");
        return 2;
    }
    let msg = format!("profiling for {seconds} s after a {delay} s delay; report: profile_{label}.txt next to the DLL");
    std::thread::spawn(move || run(seconds, delay, label));
    (api.pushboolean)(l, 1);
    lua::push_str(l, &msg);
    2
}
