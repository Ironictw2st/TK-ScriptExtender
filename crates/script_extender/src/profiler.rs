//! Sampling profiler for performance work (UI hitches, AI turn times). Diagnostic only: it reads
//! thread contexts and stacks, changes nothing in the game.
//!
//! `se_profile_start(seconds, delay_seconds, label)`: a sampler thread waits `delay`, picks the
//! busiest game threads (GetThreadTimes over a short pre-window), then every millisecond suspends
//! each of them in turn, takes RIP/RSP (GetThreadContext), copies the top of the stack
//! (ReadProcessMemory on ourselves: a fault cannot crash us) and resumes it. While a thread is
//! suspended the sampler neither allocates nor logs (the target may hold the heap or log lock);
//! samples go into a preallocated buffer and are aggregated afterwards.
//!
//! Report `<dll dir>\profile_<label>.txt` per thread: samples, "self" functions (where RIP was)
//! and "on stack" functions (any return address into the exe; a function counted once per
//! sample), as preferred-base addresses (0x140000000 + RVA) for Ghidra. Function starts come from
//! the exe's .pdata.

use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

const TH32CS_SNAPTHREAD: u32 = 0x4;
const THREAD_ACCESS: u32 = 0x2 | 0x8 | 0x40; // SUSPEND_RESUME | GET_CONTEXT | QUERY_INFORMATION
const CONTEXT_FULL: u32 = 0x0010_0003; // CONTROL | INTEGER
const STACK_WORDS: usize = 96;
const MAX_THREADS: usize = 6;

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

static RUNNING: AtomicBool = AtomicBool::new(false);

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_profile_start", se_profile_start);
}

const MAX_FRAMES: usize = 24;
/// Compact sample: function starts (RVAs) only. `funcs[0]` is the function RIP was in, or 0 when
/// RIP was outside the exe; the rest are distinct functions found on the stack.
struct Sample { thread: u16, n: u8, funcs: [u32; MAX_FRAMES] }

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

/// Sorted function start RVAs from the exe's exception directory.
unsafe fn function_starts(base: usize) -> Vec<u32> {
    let nt = base + *((base + 0x3c) as *const i32) as usize;
    let (rva, size) = (*((nt + 0x18 + 0x70 + 3 * 8) as *const u32) as usize, *((nt + 0x18 + 0x70 + 3 * 8 + 4) as *const u32) as usize);
    let mut v: Vec<u32> = (0..size / 12).map(|i| *((base + rva + i * 12) as *const u32)).filter(|s| *s != 0).collect();
    v.sort_unstable();
    v
}

fn func_of(starts: &[u32], rva: u32) -> u32 {
    match starts.binary_search(&rva) { Ok(i) => starts[i], Err(0) => 0, Err(i) => starts[i - 1] }
}

unsafe fn run(seconds: u32, delay: u32, label: String) {
    Sleep(delay * 1000);
    let (base, size) = crate::process::main_module();
    let starts = function_starts(base);
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
    log!("profiler '{label}': {} threads, sampling the {MAX_THREADS} busiest (re-picked every second) for {seconds} s", all.len());

    let ticks = seconds as usize * 1000;
    let mut samples: Vec<Sample> = Vec::with_capacity(ticks * MAX_THREADS + 16);
    let mut ctx = Ctx([0u8; 1232]);
    let me = GetCurrentProcess();
    for tick in 0..ticks {
        if tick % 1000 == 999 { repick(&mut last, &mut picked); }
        for ti in picked.iter() {
            let h = &all[*ti].1;
            if samples.len() == samples.capacity() { break; }
            let (mut rip, mut stack) = (0usize, [0usize; STACK_WORDS]);
            // ---- nothing between suspend and resume may allocate, lock or log ----
            if SuspendThread(*h) == u32::MAX { continue; }
            core::ptr::write_unaligned(ctx.0.as_mut_ptr().add(0x30) as *mut u32, CONTEXT_FULL);
            if GetThreadContext(*h, &mut ctx) != 0 {
                rip = core::ptr::read_unaligned(ctx.0.as_ptr().add(0xf8) as *const usize);
                let rsp = core::ptr::read_unaligned(ctx.0.as_ptr().add(0x98) as *const usize);
                let mut got = 0usize;
                ReadProcessMemory(me, rsp as *const c_void, stack.as_mut_ptr() as *mut c_void, STACK_WORDS * 8, &mut got);
            }
            ResumeThread(*h);
            // -----------------------------------------------------------------------
            if rip == 0 { continue; }
            let in_exe = |a: usize| a >= base && a < base + size;
            let mut s = Sample { thread: *ti as u16, n: 1, funcs: [0; MAX_FRAMES] };
            if in_exe(rip) { s.funcs[0] = func_of(&starts, (rip - base) as u32); }
            for w in stack.iter() {
                if (s.n as usize) < MAX_FRAMES && in_exe(*w) {
                    let f = func_of(&starts, (*w - base) as u32);
                    if !s.funcs[..s.n as usize].contains(&f) { s.funcs[s.n as usize] = f; s.n += 1; }
                }
            }
            samples.push(s);
        }
        Sleep(1);
    }

    // aggregate
    let mut report = format!("profile '{label}': {seconds} s, {} samples, exe base {:#x}\naddresses are preferred-base (Ghidra) function starts\n", samples.len(), base);
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_unstable_by_key(|i| std::cmp::Reverse(samples.iter().filter(|s| s.thread as usize == *i).count()));
    for ti in order {
        let (tid, cpu) = (all[ti].0, cpu_100ns(all[ti].1).saturating_sub(first[ti]));
        let mine: Vec<&Sample> = samples.iter().filter(|s| s.thread as usize == ti).collect();
        if mine.len() < 20 { continue; }
        let (mut selfc, mut stackc): (HashMap<u32, u32>, HashMap<u32, u32>) = (HashMap::new(), HashMap::new());
        let mut outside = 0u32;
        for s in &mine {
            if s.funcs[0] != 0 { *selfc.entry(s.funcs[0]).or_insert(0) += 1; } else { outside += 1; }
            for f in s.funcs[..s.n as usize].iter().filter(|f| **f != 0) { *stackc.entry(*f).or_insert(0) += 1; }
        }
        let n = mine.len() as f32;
        report += &format!("\n=== thread {tid}: {} samples, {} ms cpu during the run, {:.0}% of samples outside the exe (system / driver / waiting)\n", mine.len(), cpu / 10_000, outside as f32 * 100.0 / n);
        for (title, map) in [("self (RIP inside the function)", &selfc), ("on stack (function or its callees were running)", &stackc)] {
            let mut rows: Vec<(&u32, &u32)> = map.iter().collect();
            rows.sort_unstable_by(|a, b| b.1.cmp(a.1));
            report += &format!("  -- {title}\n");
            for (f, c) in rows.iter().take(45) {
                report += &format!("     {:5.1}%  {:6}  0x{:x}\n", **c as f32 * 100.0 / n, c, 0x1_4000_0000u64 + **f as u64);
            }
        }
    }
    for (_, h) in all { CloseHandle(h); }
    let path = crate::process::self_dir().map(|d| d.join(format!("profile_{label}.txt")));
    match path {
        Some(p) => match std::fs::write(&p, report) {
            Ok(()) => log!("profiler '{label}': report written to {}", p.display()),
            Err(e) => log!("profiler '{label}': could not write {}: {e}", p.display()),
        },
        None => log!("profiler '{label}': no DLL folder for the report"),
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
