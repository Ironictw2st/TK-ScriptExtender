//! Auto-resolve: tunables, prediction read-out and the script plan (build 1.7.2.0).
//!
//! Campaign variables (DB `campaign_variables`, 774 keys):
//!   descriptors = array of I_TWEAKER-derived objects in .data, base RVA 0x3e33520, stride 0x78,
//!   end RVA 0x3e49ff0 (vtable RVA 0x325dac0, +0x60 f32 tweak value, +0x68 CA::String name);
//!   FUN_140958a60(_, name) = linear name -> index lookup over that array.
//!   live values = `*(world + 0x3b58)`: f32[774] indexed by descriptor index, followed at +0xc18
//!   by the per-round override vector (FUN_14147b0e0 zeroes 0xc18 bytes, FUN_1414d53a0 fills the
//!   array from the DB defaults and `campaigns_campaign_variables_junctions`; it is re-run by
//!   FUN_1419e8a00 when the applicable overrides change, which would undo script values: the
//!   Lua module re-applies its values at turn start).
//!
//! Pending battle PB = `*(world + 0x3b80)` (the `pending_battle` native FUN_1415ff330):
//!   +0x11c night byte, +0xe8 u32 index of the autoresolver result in use,
//!   results vectors per day/night at PB + 0xc8 + night*0x10 {cap, count @+0xcc, data @+0xd0} of
//!   result pointers; result R (0xa8 bytes): **attacker block R+0x64, defender block R+0x7c**
//!   (verified live: the CCO's +0xc0 byte is "is attacker"), block = {+0 f32 strength share %,
//!   +4 f32 same, +8 f32 predicted casualties %, +0xc u32 prediction enum, +0x10 u32};
//!   R+0x18 vector of two 0x68-byte alliance summaries {+0 vector of army records, +0x10 men
//!   before, +0x18 men after, +0x20 men lost, ...} (CcoPendingBattleAlliance getters
//!   FUN_142f460a0 / FUN_142f89f70):
//!   0 close_victory 1 decisive_victory 2 heroic_victory 3 pyrrhic_victory 4 draw
//!   5 close_defeat 6 decisive_defeat 7 crushing_defeat 8 valiant_defeat
//!   PB+0x140 / +0x148 attacker / defender alliance, PB+0x170 fought battle result.
//!
//! Natives (q_faction only provides the model):
//!   se_ar_variable_get(q_faction, key) -> number | nil, msg
//!   se_ar_variable_set(q_faction, key, value) -> ok, msg        keys starting "autoresolver_" only
//!   se_ar_variables_reset(q_faction) -> ok, msg
//!   se_ar_variable_list(q_faction) -> "key=value;..." of every autoresolver_* key
//!   se_ar_prediction(q_faction) -> ok, "k=v;..." | false, msg
//!   se_ar_plan_set(q_faction, spec) / se_ar_plan_get() / se_ar_plan_clear(): the plan is keyed to
//!     the pending battle object that exists when it is set; the hook ignores every other battle
//!   se_ar_recompute(q_faction) -> ok, msg   re-run the engine's compute routine for the current
//!     pending battle so that the panel shows the planned outcome
//!
//! Result layout used by the hook (mapped live, 2026-09-18): alliance summary (0x68) = {+0 vector
//! of INLINE army records, +0x10 men before, +0x18 men after, +0x20 men lost, +0x28 kills,
//! +0x64 u32 prediction enum}; army record: +0x20 vector of unit records, stride 0x188:
//! +0x20 UniString name, +0x50 men initial, +0x54 men at start, +0x58 men after, +0x64 hp initial,
//! +0x68 hp at start, +0x6c hp after (characters: men 1/1/1, only hp moves). R+0x98 / +0x9c
//! attacker strength before / after, R+0xa0 / +0xa4 defender.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use retour::GenericDetour;
use core::ffi::{c_int, c_void};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const RVA_VAR_DESCRIPTORS: usize = 0x3e33520;
const RVA_VAR_DESCRIPTORS_END: usize = 0x3e49ff0;
const VAR_STRIDE: usize = 0x78;
const RVA_VAR_VTABLE: usize = 0x325dac0;
const OFF_VAR_NAME: usize = 0x68;
const OFF_WORLD_VARS: usize = 0x3b58;
const OFF_WORLD_PENDING_BATTLE: usize = 0x3b80;
const KEY_PREFIX: &str = "autoresolver_";

const PREDICTIONS: [&str; 9] = [
    "close_victory", "decisive_victory", "heroic_victory", "pyrrhic_victory", "draw",
    "close_defeat", "decisive_defeat", "crushing_defeat", "valiant_defeat",
];

static INDEX: OnceLock<Result<HashMap<String, usize>, String>> = OnceLock::new();
/// idx -> value before the first script write (for reset)
static ORIGINAL: Mutex<Option<HashMap<usize, f32>>> = Mutex::new(None);
static PLAN: Mutex<Option<String>> = Mutex::new(None);
/// pending battle object the plan belongs to
static PLAN_PB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static COMPUTE_TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Engine {
    string_from_cstr: unsafe extern "C" fn(*mut c_void, *const core::ffi::c_char) -> *mut c_void,
    string_dtor: unsafe extern "C" fn(*mut c_void),
    alloc: unsafe extern "C" fn(usize, u32) -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
}
static ENGINE: OnceLock<Engine> = OnceLock::new();

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_ar_variable_get", se_ar_variable_get);
    lua::set_global_fn(l, "se_ar_variable_set", se_ar_variable_set);
    lua::set_global_fn(l, "se_ar_variables_reset", se_ar_variables_reset);
    lua::set_global_fn(l, "se_ar_variable_list", se_ar_variable_list);
    lua::set_global_fn(l, "se_ar_prediction", se_ar_prediction);
    lua::set_global_fn(l, "se_ar_plan_set", se_ar_plan_set);
    lua::set_global_fn(l, "se_ar_plan_get", se_ar_plan_get);
    lua::set_global_fn(l, "se_ar_plan_clear", se_ar_plan_clear);
    lua::set_global_fn(l, "se_ar_recompute", se_ar_recompute);
}

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool {
    p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0
}
unsafe fn rq(p: usize) -> usize {
    if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 }
}
unsafe fn rd(p: usize) -> u32 {
    if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 }
}

unsafe fn ca_string(sp: usize) -> String {
    if !readable(sp, 16) {
        return String::new();
    }
    if rq(sp + 8) >> 60 == 8 {
        let raw = core::slice::from_raw_parts(sp as *const u8, 15);
        let n = raw.iter().position(|&c| c == 0).unwrap_or(15);
        let ok = n > 0 && raw[..n].iter().all(|c| c.is_ascii_graphic());
        return if ok { String::from_utf8_lossy(&raw[..n]).into_owned() } else { String::new() };
    }
    let len = rd(sp) as usize;
    let ptr = rq(sp + 8);
    if len == 0 || len > 160 || !readable(ptr, len) {
        return String::new();
    }
    String::from_utf8_lossy(core::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
}

/// name -> index, built once from the descriptor array; refuses the whole feature unless every
/// descriptor has the expected vtable and a readable name.
unsafe fn index() -> Result<&'static HashMap<String, usize>, String> {
    INDEX
        .get_or_init(|| {
            let (base, _) = crate::process::main_module();
            let count = (RVA_VAR_DESCRIPTORS_END - RVA_VAR_DESCRIPTORS) / VAR_STRIDE;
            let mut map = HashMap::with_capacity(count);
            for i in 0..count {
                let d = base + RVA_VAR_DESCRIPTORS + i * VAR_STRIDE;
                if rq(d) != base + RVA_VAR_VTABLE {
                    return Err(format!("campaign variable descriptor {i} has vtable rva {:#x}, expected {:#x}", rq(d).wrapping_sub(base), RVA_VAR_VTABLE));
                }
                let name = ca_string(d + OFF_VAR_NAME);
                if name.is_empty() {
                    return Err(format!("campaign variable descriptor {i} has no readable name"));
                }
                map.insert(name, i);
            }
            if !map.contains_key("autoresolver_duel_base_chance") {
                return Err("descriptor array does not contain autoresolver_duel_base_chance".into());
            }
            log!("autoresolve: {} campaign variables indexed, {} of them autoresolver_*", map.len(), map.keys().filter(|k| k.starts_with(KEY_PREFIX)).count());
            Ok(map)
        })
        .as_ref()
        .map_err(|e| e.clone())
}

unsafe fn world_of(l: *mut LuaState, idx: c_int) -> Result<usize, String> {
    let faction = crate::progression::faction_from_arg(l, idx)?;
    let world = rq(rq(faction + 0x288) + 0x78);
    if world == 0 || !readable(world + OFF_WORLD_PENDING_BATTLE, 8) {
        return Err("could not derive the model from the faction".into());
    }
    Ok(world)
}

unsafe fn vars_of(world: usize) -> Result<usize, String> {
    let vars = rq(world + OFF_WORLD_VARS);
    let count = (RVA_VAR_DESCRIPTORS_END - RVA_VAR_DESCRIPTORS) / VAR_STRIDE;
    if vars == 0 || !readable(vars, count * 4 + 0x10) {
        return Err(format!("campaign variable array missing (world+{:#x} = {:#x})", OFF_WORLD_VARS, vars));
    }
    Ok(vars)
}

unsafe fn slot(l: *mut LuaState, key: &str) -> Result<(usize, usize), String> {
    if !key.starts_with(KEY_PREFIX) {
        return Err(format!("only keys starting with '{KEY_PREFIX}' are exposed (got '{key}')"));
    }
    let idx = *index()?.get(key).ok_or_else(|| format!("no campaign variable named '{key}'"))?;
    let vars = vars_of(world_of(l, 1)?)?;
    Ok((idx, vars + idx * 4))
}

unsafe fn bool_result(l: *mut LuaState, r: Result<String, String>, who: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match r {
        Ok(m) => { log!("{who}: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); }
        Err(e) => { log!("{who}: {e}"); (api.pushboolean)(l, 0); lua::push_str(l, &e); }
    }
    2
}

unsafe extern "C" fn se_ar_variable_get(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    match slot(l, &key) {
        Ok((_, p)) => {
            let v = core::ptr::read_unaligned(p as *const f32);
            (api.pushnumber)(l, v);
            1
        }
        Err(e) => {
            log!("se_ar_variable_get: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_ar_variable_set(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let key = lua::to_str(l, 2);
    let value = (api.tonumber)(l, 3);
    let r: Result<String, String> = (|| {
        if !value.is_finite() || value.abs() > 1.0e6 { return Err(format!("value {value} is out of range")); }
        let (idx, p) = slot(l, &key)?;
        let old = core::ptr::read_unaligned(p as *const f32);
        if !old.is_finite() { return Err(format!("current value of '{key}' is not a finite number; layout mismatch, nothing written")); }
        ORIGINAL.lock().map_err(|_| "state lock poisoned")?.get_or_insert_with(HashMap::new).entry(idx).or_insert(old);
        core::ptr::write_unaligned(p as *mut f32, value);
        Ok(format!("{key} [{idx}]: {old} -> {value}"))
    })();
    bool_result(l, r, "se_ar_variable_set")
}

unsafe extern "C" fn se_ar_variables_reset(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let vars = vars_of(world_of(l, 1)?)?;
        let mut guard = ORIGINAL.lock().map_err(|_| "state lock poisoned")?;
        let map = guard.take().unwrap_or_default();
        for (idx, v) in &map {
            core::ptr::write_unaligned((vars + idx * 4) as *mut f32, *v);
        }
        Ok(format!("{} autoresolver variables restored", map.len()))
    })();
    bool_result(l, r, "se_ar_variables_reset")
}

unsafe extern "C" fn se_ar_variable_list(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let vars = vars_of(world_of(l, 1)?)?;
        let mut rows: Vec<(&String, &usize)> = index()?.iter().filter(|(k, _)| k.starts_with(KEY_PREFIX)).collect();
        rows.sort();
        Ok(rows.iter().map(|(k, i)| format!("{k}={}", core::ptr::read_unaligned((vars + **i * 4) as *const f32))).collect::<Vec<_>>().join(";"))
    })();
    match r {
        Ok(s) => { lua::push_str(l, &s); 1 }
        Err(e) => {
            log!("se_ar_variable_list: {e}");
            if let Some(api) = lua::api() { (api.pushnil)(l); }
            lua::push_str(l, &e);
            2
        }
    }
}

unsafe extern "C" fn se_ar_prediction(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let world = world_of(l, 1)?;
        let pb = rq(world + OFF_WORLD_PENDING_BATTLE);
        if pb == 0 || !readable(pb, 0x180) { return Err("no pending battle object".into()); }
        let night = (rd(pb + 0x11c) & 0xff) as usize;
        if night > 1 { return Err(format!("night flag {night} is not a boolean; layout mismatch")); }
        let which = rd(pb + 0xe8) as usize;
        let count = rd(pb + 0xcc + night * 0x10) as usize;
        let data = rq(pb + 0xd0 + night * 0x10);
        let mut out = format!("pending_battle={:#x};night={night};result_index={which};results={count}", pb);
        if count == 0 || count > 16 || which >= count || !readable(data, count * 8) {
            return Ok(out + ";available=0");
        }
        let res = rq(data + which * 8);
        if !readable(res, 0x100) { return Ok(out + ";available=0"); }
        let words: Vec<String> = (0..0x20).map(|k| format!("{:08x}", rd(res + k * 4))).collect();
        log!("se_ar_prediction: result {:#x}: {}", res, words.join(" "));
        let sums = rq(res + 0x20);
        for (i, (side, off)) in [("attacker", 0x64_usize), ("defender", 0x7c)].into_iter().enumerate() {
            let share = core::ptr::read_unaligned((res + off) as *const f32);
            let casualties = core::ptr::read_unaligned((res + off + 8) as *const f32);
            let e = rd(res + off + 0xc) as usize;
            let name = PREDICTIONS.get(e).copied().unwrap_or("unknown");
            out += &format!(";{side}_prediction={name};{side}_prediction_id={e};{side}_casualties_percent={casualties};{side}_strength_share={share}");
            if rd(res + 0x1c) == 2 && readable(sums, 0xd0) {
                let s = sums + i * 0x68;
                out += &format!(";{side}_men_before={};{side}_men_after={};{side}_men_lost={}", rd(s + 0x10), rd(s + 0x18), rd(s + 0x20));
            }
        }
        Ok(out + ";available=1")
    })();
    bool_result(l, r, "se_ar_prediction")
}

unsafe extern "C" fn se_ar_plan_set(l: *mut LuaState) -> c_int {
    let spec = lua::to_str(l, 2);
    let r: Result<String, String> = (|| {
        if spec.len() > 8192 { return Err("plan is too long".into()); }
        let world = world_of(l, 1)?;
        let pb = rq(world + OFF_WORLD_PENDING_BATTLE);
        if pb == 0 || !readable(pb, 0x180) { return Err("no pending battle object".into()); }
        *PLAN.lock().map_err(|_| "state lock poisoned")? = Some(spec.clone());
        PLAN_PB.store(pb, std::sync::atomic::Ordering::SeqCst);
        Ok(format!("plan stored for pending battle {:#x}: {spec}", pb))
    })();
    bool_result(l, r, "se_ar_plan_set")
}

unsafe extern "C" fn se_ar_recompute(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let target = COMPUTE_TARGET.load(std::sync::atomic::Ordering::SeqCst);
        if target == 0 { return Err("the auto-resolve hook is not installed".into()); }
        let world = world_of(l, 1)?;
        let pb = rq(world + OFF_WORLD_PENDING_BATTLE);
        if pb == 0 || !readable(pb, 0x180) { return Err("no pending battle object".into()); }
        let night = (rd(pb + 0x11c) & 0xff) as usize;
        if night > 1 { return Err("night flag is not a boolean; layout mismatch".into()); }
        let count = rd(pb + 0xcc + night * 0x10);
        if count == 0 || count > 16 { return Err(format!("the pending battle has {count} results; nothing to recompute")); }
        // Through the hooked entry, exactly as the engine calls it at the click.
        let f: ComputeResults = core::mem::transmute(target);
        f(pb as *mut c_void, night as u8);
        Ok(format!("results recomputed for pending battle {:#x}", pb))
    })();
    bool_result(l, r, "se_ar_recompute")
}

unsafe extern "C" fn se_ar_plan_get(l: *mut LuaState) -> c_int {
    let s = PLAN.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
    lua::push_str(l, &s);
    1
}

unsafe extern "C" fn se_ar_plan_clear(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let had = PLAN.lock().map_err(|_| "state lock poisoned")?.take().is_some();
        PLAN_PB.store(0, std::sync::atomic::Ordering::SeqCst);
        Ok(if had { "plan cleared".into() } else { "no plan was stored".into() })
    })();
    bool_result(l, r, "se_ar_plan_clear")
}

// ---------------------------------------------------------------------------------------------
// Hook: FUN_14185e030(PB, night). 0.25 only observes: every run (the prediction when the panel
// opens, the real resolve at the click, AI battles) is logged with a deep dump of the new result,
// so the per-unit layout can be mapped before anything is rewritten.
// ---------------------------------------------------------------------------------------------

type ComputeResults = unsafe extern "C" fn(*mut c_void, u8);
static COMPUTE_HOOK: OnceLock<GenericDetour<ComputeResults>> = OnceLock::new();
static DUMPS_LEFT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(12);

unsafe fn hex_words(p: usize, bytes: usize) -> String {
    (0..bytes / 8).map(|k| format!("{:016x}", rq(p + k * 8))).collect::<Vec<_>>().join(" ")
}

unsafe fn dump_result(res: usize) {
    if !readable(res, 0xa8) { return; }
    log!("  result {:#x}: {}", res, hex_words(res, 0xa8));
    let (duels, ddata) = (rd(res + 0x2c) as usize, rq(res + 0x30));
    for k in 0..duels.min(8) {
        if readable(ddata + k * 0x38, 0x38) { log!("  duel[{k}] {:#x}: {}", ddata + k * 0x38, hex_words(ddata + k * 0x38, 0x38)); }
    }
    let (count, sums) = (rd(res + 0x1c) as usize, rq(res + 0x20));
    for i in 0..count.min(2) {
        let s = sums + i * 0x68;
        if !readable(s, 0x68) { break; }
        log!("  alliance[{i}] {:#x}: {}", s, hex_words(s, 0x68));
        let (armies, adata) = (rd(s + 4) as usize, rq(s + 8));
        for a in 0..armies.min(1) {
            let army = adata; // army records are stored inline
            if !readable(army, 0x90) { break; }
            log!("    army[{a}] {:#x}: {}", army, hex_words(army, 0x90));
            for off in [0usize, 0x20, 0x40, 0x50, 0x70] {
                let (n, data) = (rd(army + off + 4) as usize, rq(army + off + 8));
                if n == 0 || n > 64 || !readable(data, 0x40) { continue; }
                log!("      vec@{:#x} count {n} data {:#x}: {}", off, data, hex_words(data, 0x100.min(n * 0x40).max(0x40)));
                let first = rq(data);
                if readable(first, 0x80) && first > 0x10000 && first >> 44 != 0 {
                    log!("        [0] -> {:#x}: {}", first, hex_words(first, 0x80));
                    let second = rq(data + 8);
                    if n > 1 && readable(second, 0x80) { log!("        [1] -> {:#x}: {}", second, hex_words(second, 0x80)); }
                }
            }
        }
    }
}

unsafe extern "C" fn compute_detour(pb: *mut c_void, night: u8) {
    let Some(hook) = COMPUTE_HOOK.get() else { return };
    let p = pb as usize;
    let slot = p + 0xc8 + (night as usize & 1) * 0x10;
    let before = if readable(slot, 0x10) { rd(slot + 4) } else { 0 };
    hook.call(pb, night);
    if !readable(slot, 0x10) { return; }
    let after = rd(slot + 4) as usize;
    let plan = PLAN.lock().ok().and_then(|g| g.clone());
    log!("ar_compute_results(pb={:#x}, night={night}): results {before} -> {after}, plan {}", p, plan.as_deref().unwrap_or("none"));
    if after == 0 || after > 16 { return; }
    if let Some(spec) = plan.as_deref() {
        if PLAN_PB.load(std::sync::atomic::Ordering::SeqCst) == p {
            let data = rq(slot + 8);
            if readable(data, after * 8) {
                let res = rq(data + (after - 1) * 8);
                match apply_plan(res, spec) {
                    Ok(m) => log!("  plan applied: {m}"),
                    Err(e) => log!("  plan NOT applied: {e}"),
                }
                match apply_duels(p, res, spec) {
                    Ok(m) if m.is_empty() => {}
                    Ok(m) => log!("  {m}"),
                    Err(e) => log!("  duel rules NOT applied: {e}"),
                }
            }
        } else {
            log!("  plan belongs to another pending battle; left alone");
        }
    }
    if DUMPS_LEFT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) > 0 {
        let data = rq(slot + 8);
        if readable(data, after * 8) { dump_result(rq(data + (after - 1) * 8)); }
    }
}

struct SidePlan { scale: f32, max: f32 }

fn plan_value(spec: &str, key: &str) -> Option<String> {
    spec.split(';').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
}

const UNIT_STRIDE: usize = 0x188;

/// (units data, count) of a side when it has exactly one army record, else an error text.
unsafe fn side_units(sum: usize) -> Result<(usize, usize), String> {
    let (armies, adata) = (rd(sum + 4) as usize, rq(sum + 8));
    if armies != 1 { return Err(format!("{armies} army records (only single-record sides are handled)")); }
    if !readable(adata, 0x30) { return Err("army record not readable".into()); }
    let (n, data) = (rd(adata + 0x24) as usize, rq(adata + 0x28));
    if n == 0 || n > 80 || !readable(data, n * UNIT_STRIDE) { return Err(format!("unit vector not plausible (count {n})")); }
    for u in 0..n {
        let r = data + u * UNIT_STRIDE;
        let (start, after, hp_start, hp_after) = (rd(r + 0x54), rd(r + 0x58), rd(r + 0x68), rd(r + 0x6c));
        if start > 100_000 || after > start || hp_start > 100_000_000 || hp_after > hp_start {
            return Err(format!("unit {u} fields not plausible (men {start}->{after}, hp {hp_start}->{hp_after})"));
        }
    }
    Ok((data, n))
}

unsafe fn side_loss(data: usize, n: usize) -> f32 {
    let (mut hp_start, mut hp_after) = (0u64, 0u64);
    for u in 0..n {
        hp_start += rd(data + u * UNIT_STRIDE + 0x68) as u64;
        hp_after += rd(data + u * UNIT_STRIDE + 0x6c) as u64;
    }
    if hp_start == 0 { 0.0 } else { 1.0 - hp_after as f32 / hp_start as f32 }
}

/// Rewrites the freshly computed result according to the plan: per-unit survivors and hit points,
/// the side summaries, the prediction blocks and (when the winner is forced) the result enums.
unsafe fn apply_plan(res: usize, spec: &str) -> Result<String, String> {
    if !readable(res, 0xa8) || rd(res + 0x1c) != 2 { return Err("result does not have two alliance summaries".into()); }
    let sums = rq(res + 0x20);
    if !readable(sums, 0xd0) { return Err("alliance summaries not readable".into()); }
    let num = |k: &str| plan_value(spec, k).and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite());
    let plans = [
        SidePlan { scale: num("cas_att_scale").unwrap_or(1.0).clamp(0.0, 10.0), max: num("cas_att_max").unwrap_or(1.0).clamp(0.0, 1.0) },
        SidePlan { scale: num("cas_def_scale").unwrap_or(1.0).clamp(0.0, 10.0), max: num("cas_def_max").unwrap_or(1.0).clamp(0.0, 1.0) },
    ];
    let winner = plan_value(spec, "winner");
    let att_enum = rd(res + 0x64 + 0xc);
    let def_enum = rd(res + 0x7c + 0xc);
    if att_enum > 8 || def_enum > 8 { return Err(format!("prediction enums not plausible ({att_enum}, {def_enum})")); }
    let attacker_wins = att_enum < 4;
    let swap = match winner.as_deref() {
        Some("attacker") => !attacker_wins && att_enum != 4,
        Some("defender") => attacker_wins,
        _ => false,
    };
    let sides = [side_units(sums), side_units(sums + 0x68)];
    let losses = [sides[0].as_ref().map(|(d, n)| side_loss(*d, *n)).unwrap_or(0.0), sides[1].as_ref().map(|(d, n)| side_loss(*d, *n)).unwrap_or(0.0)];
    if swap && (sides[0].is_err() || sides[1].is_err()) {
        return Err(format!("cannot force the winner: attacker {:?}, defender {:?}", sides[0].as_ref().err(), sides[1].as_ref().err()));
    }
    let mut report = Vec::new();
    let mut lost_after = [0u32; 2];
    for i in 0..2 {
        let sum = sums + i * 0x68;
        let (data, n) = match &sides[i] {
            Ok(v) => *v,
            Err(e) => { report.push(format!("side {i} skipped: {e}")); lost_after[i] = rd(sum + 0x20); continue; }
        };
        if !swap && plans[i].scale == 1.0 && plans[i].max >= 1.0 {
            report.push(format!("side {i}: untouched ({n} unit records)"));
            lost_after[i] = rd(sum + 0x20);
            continue;
        }
        let mut detail = Vec::new();
        // forced winner: this side takes the other side's loss level
        let level = if swap && losses[i] > 0.0001 { losses[1 - i] / losses[i] } else { 1.0 };
        let (mut men_before, mut men_after, mut men_after_old, mut hp_b, mut hp_a) = (0u32, 0u32, 0u32, 0u64, 0u64);
        for u in 0..n {
            let r = data + u * UNIT_STRIDE;
            let (start, after, hp_start, hp_after) = (rd(r + 0x54), rd(r + 0x58), rd(r + 0x68), rd(r + 0x6c));
            if hp_start > 0 {
                let loss = 1.0 - hp_after as f32 / hp_start as f32;
                let mut new_loss = (loss * level * plans[i].scale).clamp(0.0, 1.0).min(plans[i].max);
                // Characters (single-man records) are never made worse by casualty or winner
                // rules: their fate belongs to the duel / fate controls.
                if start <= 1 { new_loss = new_loss.min(loss); }
                let new_hp = ((hp_start as f32) * (1.0 - new_loss)).round() as u32;
                let mut new_men = ((start as f32) * (1.0 - new_loss)).round() as u32;
                if new_hp > 0 && new_men == 0 && start > 0 { new_men = 1; }
                if new_hp == 0 { new_men = 0; }
                core::ptr::write_unaligned((r + 0x6c) as *mut u32, new_hp.min(hp_start));
                core::ptr::write_unaligned((r + 0x58) as *mut u32, new_men.min(start));
            }
            men_before += start; men_after += rd(r + 0x58); hp_b += hp_start as u64; hp_a += rd(r + 0x6c) as u64;
            men_after_old += after;
            detail.push(format!("{start}:{after}>{}", rd(r + 0x58)));
        }
        let old_lost = rd(sum + 0x20);
        let (sum_before, sum_after) = (rd(sum + 0x10), rd(sum + 0x18));
        let new_after = (sum_after as i64 + men_after as i64 - men_after_old as i64).clamp(0, sum_before as i64) as u32;
        core::ptr::write_unaligned((sum + 0x18) as *mut u64, new_after as u64);
        core::ptr::write_unaligned((sum + 0x20) as *mut u64, sum_before.saturating_sub(new_after) as u64);
        lost_after[i] = sum_before.saturating_sub(new_after);
        if men_before != sum_before {
            report.push(format!("side {i}: unit records cover {men_before} of {sum_before} men"));
        }
        log!("  side {i} units (start:after>new): {}", detail.join(" "));
        // prediction block casualties % and the strength-after float follow the hit point loss
        let block = res + if i == 0 { 0x64 } else { 0x7c };
        let new_frac = if hp_b == 0 { 0.0 } else { 1.0 - hp_a as f32 / hp_b as f32 };
        core::ptr::write_unaligned((block + 8) as *mut f32, new_frac * 100.0);
        let (sb, sa) = (res + 0x98 + i * 8, res + 0x9c + i * 8);
        let before_strength = core::ptr::read_unaligned(sb as *const f32);
        if before_strength.is_finite() && before_strength > 0.0 {
            core::ptr::write_unaligned(sa as *mut f32, before_strength * (1.0 - new_frac));
        }
        report.push(format!("side {i}: men {men_before} -> {men_after} (lost {old_lost} -> {}), loss {:.1}% -> {:.1}%", lost_after[i], losses[i] * 100.0, new_frac * 100.0));
    }
    // kills of each side = what the other side lost
    core::ptr::write_unaligned((sums + 0x28) as *mut u64, lost_after[1] as u64);
    core::ptr::write_unaligned((sums + 0x68 + 0x28) as *mut u64, lost_after[0] as u64);
    if swap {
        // BATTLE_RESULTS header (the first 0x60 bytes are what FUN_141850df0 copies into the
        // fought result): +4 i32 winning alliance index (-1 none, 0 attacker, 1 defender);
        // alliance summary +0x60 = that side's battle_result_types id (what
        // attacker_battle_result() reports). The prediction blocks carry the same ids.
        let old_winner = rd(res + 4);
        if old_winner > 1 { return Err(format!("winner index {old_winner:#x} is not 0/1; winner not forced")); }
        let (a, d) = (rd(sums + 0x60), rd(sums + 0x68 + 0x60));
        if a > 9 || d > 9 { return Err(format!("alliance result ids not plausible ({a}, {d}); winner not forced")); }
        core::ptr::write_unaligned((res + 4) as *mut u32, 1 - old_winner);
        core::ptr::write_unaligned((sums + 0x60) as *mut u32, d);
        core::ptr::write_unaligned((sums + 0x68 + 0x60) as *mut u32, a);
        core::ptr::write_unaligned((res + 0x64 + 0xc) as *mut u32, def_enum);
        core::ptr::write_unaligned((res + 0x7c + 0xc) as *mut u32, att_enum);
        report.push(format!("winner forced to {}: winner index {old_winner} -> {}, result ids {a}/{d} -> {d}/{a}", winner.unwrap_or_default(), 1 - old_winner));
    }
    Ok(report.join("; "))
}

// ---------------------------------------------------------------------------------------------
// Duels. FUN_14226f320(sim) (first call of the simulator's post-processing FUN_142269d90) rolls
// them: candidates per side = FUN_142264ad0 (16-byte entries {unit, i32 power, i32}); while
// fewer than autoresolver_duel_max_limit: chance = base + min(n_att, n_def) * character_mod -
// done * additional, clamped to [chance_min, chance_max], one roll; both duelists are picked at
// random (FUN_14225b9d0); then it is deterministic: diff = power_a - power_b, |diff| >
// autoresolver_duel_refuse_variable -> refused (states 3/3, +0x28/+0x2c = 2/2), else the
// stronger one wins (states 0/5) and is stored FIRST. Record (0x38, vector at R+0x28, pushed
// by FUN_141ff2c00): +0 CA::String unit key of the first duelist, +0x10 second, +0x20 u32 cqi
// first, +0x24 cqi second, +0x28/+0x2c u32 (2/2 refused; a duration float appears at +0x28
// for fought duels), +0x30 u32 state first (0 won, 3 refused), +0x34 state second (5 lost).
// ---------------------------------------------------------------------------------------------

const DUEL_STRIDE: usize = 0x38;

struct DuelRule { a: u32, b: u32, happen: bool, win_chance: f32, winner: i64, a_key: String, b_key: String }

fn parse_duel_rules(spec: &str) -> Vec<DuelRule> {
    let Some(v) = plan_value(spec, "duels") else { return Vec::new() };
    v.split('|').filter_map(|row| {
        let f: Vec<&str> = row.split(',').collect();
        if f.len() < 5 { return None; }
        Some(DuelRule {
            a: f[0].parse::<f32>().ok()? as u32,
            b: f[1].parse::<f32>().ok()? as u32,
            happen: f[2] != "0",
            win_chance: f[3].parse().unwrap_or(-1.0),
            winner: f[4].parse::<f32>().map(|x| x as i64).unwrap_or(-1),
            a_key: f.get(6).map(|s| s.to_string()).unwrap_or_default(),
            b_key: f.get(7).map(|s| s.to_string()).unwrap_or_default(),
        })
    }).collect()
}

/// Deterministic 0..1 value for a pair in one pending battle (the prediction run and the run at
/// the click must agree).
fn pair_roll(pb: usize, a: u32, b: u32) -> f32 {
    let mut x = (pb as u64) ^ ((a.min(b) as u64) << 32 | a.max(b) as u64) ^ 0x9e37_79b9_7f4a_7c15;
    x ^= x >> 33; x = x.wrapping_mul(0xff51_afd7_ed55_8ccd); x ^= x >> 33; x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53); x ^= x >> 33;
    (x >> 40) as f32 / (1u64 << 24) as f32
}

unsafe fn duel_remove(e: &Engine, res: usize, i: usize) {
    let (count, data) = (rd(res + 0x2c) as usize, rq(res + 0x30));
    let rec = data + i * DUEL_STRIDE;
    (e.string_dtor)(rec as *mut c_void);
    (e.string_dtor)((rec + 0x10) as *mut c_void);
    if i + 1 < count {
        core::ptr::copy_nonoverlapping((data + (count - 1) * DUEL_STRIDE) as *const u8, rec as *mut u8, DUEL_STRIDE);
    }
    core::ptr::write_unaligned((res + 0x2c) as *mut u32, (count - 1) as u32);
}

unsafe fn duel_set_winner(rec: usize, winner: u32) {
    if rd(rec + 0x20) != winner && rd(rec + 0x24) == winner {
        let mut tmp = [0u8; 16];
        core::ptr::copy_nonoverlapping(rec as *const u8, tmp.as_mut_ptr(), 16);
        core::ptr::copy_nonoverlapping((rec + 0x10) as *const u8, rec as *mut u8, 16);
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), (rec + 0x10) as *mut u8, 16);
        let (first, second) = (rd(rec + 0x20), rd(rec + 0x24));
        core::ptr::write_unaligned((rec + 0x20) as *mut u32, second);
        core::ptr::write_unaligned((rec + 0x24) as *mut u32, first);
    }
    // a fought duel: first duelist won (0), second lost (5)
    if rd(rec + 0x30) == 3 || rd(rec + 0x34) == 3 {
        core::ptr::write_unaligned((rec + 0x28) as *mut u64, 0);
    }
    core::ptr::write_unaligned((rec + 0x30) as *mut u32, 0);
    core::ptr::write_unaligned((rec + 0x34) as *mut u32, 5);
}

unsafe fn duel_append(e: &Engine, res: usize, first: (u32, &str), second: (u32, &str)) -> Result<(), String> {
    let (cap, count, data) = (rd(res + 0x28) as usize, rd(res + 0x2c) as usize, rq(res + 0x30));
    let mut data = data;
    if count >= cap {
        let new_cap = (cap * 2).max(2);
        let fresh = (e.alloc)(new_cap * DUEL_STRIDE, 0) as usize;
        if fresh == 0 { return Err("engine allocator returned null".into()); }
        core::ptr::write_bytes(fresh as *mut u8, 0, new_cap * DUEL_STRIDE);
        if count > 0 { core::ptr::copy_nonoverlapping(data as *const u8, fresh as *mut u8, count * DUEL_STRIDE); }
        if data != 0 { (e.free)(data as *mut c_void); }
        core::ptr::write_unaligned((res + 0x28) as *mut u32, new_cap as u32);
        core::ptr::write_unaligned((res + 0x30) as *mut usize, fresh);
        data = fresh;
    }
    let rec = data + count * DUEL_STRIDE;
    core::ptr::write_bytes(rec as *mut u8, 0, DUEL_STRIDE);
    for (off, key) in [(0usize, first.1), (0x10, second.1)] {
        let mut c = key.as_bytes().to_vec();
        c.push(0);
        (e.string_from_cstr)((rec + off) as *mut c_void, c.as_ptr() as *const core::ffi::c_char);
    }
    core::ptr::write_unaligned((rec + 0x20) as *mut u32, first.0);
    core::ptr::write_unaligned((rec + 0x24) as *mut u32, second.0);
    core::ptr::write_unaligned((rec + 0x30) as *mut u32, 0);
    core::ptr::write_unaligned((rec + 0x34) as *mut u32, 5);
    core::ptr::write_unaligned((res + 0x2c) as *mut u32, (count + 1) as u32);
    Ok(())
}

unsafe fn apply_duels(pb: usize, res: usize, spec: &str) -> Result<String, String> {
    let rules = parse_duel_rules(spec);
    let default_none = plan_value(spec, "duel_default").as_deref() == Some("none");
    let max = plan_value(spec, "duel_max").and_then(|v| v.parse::<f32>().ok()).map(|v| v as usize);
    if rules.is_empty() && !default_none && max.is_none() { return Ok(String::new()); }
    let e = ENGINE.get().ok_or("engine table missing")?;
    let (cap, count, data) = (rd(res + 0x28) as usize, rd(res + 0x2c) as usize, rq(res + 0x30));
    if count > cap || cap > 64 || (count > 0 && !readable(data, count * DUEL_STRIDE)) {
        return Err(format!("duel vector not plausible (cap {cap}, count {count}, data {:#x})", data));
    }
    let before = count;
    // The roll is seeded from the plan (turn + force cqis), never from an address: both machines
    // of a multiplayer game have to pick the same winner.
    let _ = pb;
    let seed = plan_value(spec, "seed").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) as usize;
    let rule_for = |x: u32, y: u32| rules.iter().find(|r| (r.a == x && r.b == y) || (r.a == y && r.b == x));
    let mut report = Vec::new();
    // 1. removals: forbidden pairs, and everything without a rule when the default is "none"
    let mut i = 0;
    while i < rd(res + 0x2c) as usize {
        let rec = rq(res + 0x30) + i * DUEL_STRIDE;
        let (x, y) = (rd(rec + 0x20), rd(rec + 0x24));
        let drop = match rule_for(x, y) { Some(r) => !r.happen, None => default_none };
        if drop { report.push(format!("removed {x} vs {y}")); duel_remove(e, res, i); } else { i += 1; }
    }
    // 2. winners of the duels that stay, 3. forced duels the engine did not roll
    for r in rules.iter().filter(|r| r.happen) {
        let desired = if r.winner == r.a as i64 || r.winner == r.b as i64 { r.winner as u32 }
            else if r.win_chance >= 0.0 { if pair_roll(seed, r.a, r.b) < r.win_chance { r.a } else { r.b } }
            else { 0 };
        let n = rd(res + 0x2c) as usize;
        let found = (0..n).map(|k| rq(res + 0x30) + k * DUEL_STRIDE).find(|rec| {
            let (x, y) = (rd(rec + 0x20), rd(rec + 0x24));
            (x == r.a && y == r.b) || (x == r.b && y == r.a)
        });
        match found {
            Some(rec) => {
                if desired != 0 { duel_set_winner(rec, desired); report.push(format!("{} vs {}: winner {desired}", r.a, r.b)); }
            }
            None => {
                if r.a_key.is_empty() || r.b_key.is_empty() {
                    report.push(format!("{} vs {}: not rolled by the engine and no unit keys given, cannot force it", r.a, r.b));
                    continue;
                }
                let w = if desired != 0 { desired } else { r.a };
                let (first, second) = if w == r.a { ((r.a, r.a_key.as_str()), (r.b, r.b_key.as_str())) } else { ((r.b, r.b_key.as_str()), (r.a, r.a_key.as_str())) };
                duel_append(e, res, first, second)?;
                report.push(format!("{} vs {}: duel created, winner {w}", r.a, r.b));
            }
        }
    }
    // 4. cap
    if let Some(m) = max {
        while rd(res + 0x2c) as usize > m {
            let last = rd(res + 0x2c) as usize - 1;
            duel_remove(e, res, last);
            report.push("trimmed to max".into());
        }
    }
    Ok(format!("duels {before} -> {}: {}", rd(res + 0x2c), report.join(", ")))
}

pub fn install_hooks(t: &Table) {
    let _ = ENGINE.set(unsafe {
        Engine {
            string_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
            alloc: core::mem::transmute(t.get("engine_alloc")),
            free: core::mem::transmute(t.get("engine_free")),
        }
    });
    if crate::build::config_value("autoresolve_hooks").as_deref() == Some("0") {
        log!("autoresolve hooks disabled by script_extender.cfg");
        return;
    }
    let target: ComputeResults = unsafe { core::mem::transmute(t.get("ar_compute_results")) };
    // SAFETY: anchor-verified prologue (pushes, lea rbp,[rsp-0x40], sub rsp): no RIP-relative
    // instruction in the relocated bytes.
    unsafe {
        match GenericDetour::new(target, compute_detour) {
            Ok(d) => {
                if let Err(e) = crate::freeze::with_threads_frozen(target as usize, 16, || d.enable()) {
                    log!("failed to enable the auto-resolve hook: {e}");
                    return;
                }
                let _ = COMPUTE_HOOK.set(d);
                COMPUTE_TARGET.store(target as usize, std::sync::atomic::Ordering::SeqCst);
                log!("auto-resolve hook installed");
            }
            Err(e) => log!("failed to create the auto-resolve hook: {e}"),
        }
    }
}
