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
//!   result pointers; result R: attacker block R+0x7c, defender block R+0x64, block+8 u32
//!   predicted casualties, block+0xc u32 prediction enum (CcoPendingBattleAlliance getters
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
//!   se_ar_plan_set(spec) / se_ar_plan_get() / se_ar_plan_clear()  plan storage (consumed by the
//!     hooks of later versions; 0.24 only stores it)

use crate::log;
use crate::lua::{self, LuaState};
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

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_ar_variable_get", se_ar_variable_get);
    lua::set_global_fn(l, "se_ar_variable_set", se_ar_variable_set);
    lua::set_global_fn(l, "se_ar_variables_reset", se_ar_variables_reset);
    lua::set_global_fn(l, "se_ar_variable_list", se_ar_variable_list);
    lua::set_global_fn(l, "se_ar_prediction", se_ar_prediction);
    lua::set_global_fn(l, "se_ar_plan_set", se_ar_plan_set);
    lua::set_global_fn(l, "se_ar_plan_get", se_ar_plan_get);
    lua::set_global_fn(l, "se_ar_plan_clear", se_ar_plan_clear);
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
        for (side, off) in [("attacker", 0x7c_usize), ("defender", 0x64)] {
            let casualties = rd(res + off + 8);
            let e = rd(res + off + 0xc) as usize;
            let name = PREDICTIONS.get(e).copied().unwrap_or("unknown");
            out += &format!(";{side}_prediction={name};{side}_prediction_id={e};{side}_casualties={casualties}");
        }
        Ok(out + ";available=1")
    })();
    bool_result(l, r, "se_ar_prediction")
}

unsafe extern "C" fn se_ar_plan_set(l: *mut LuaState) -> c_int {
    let spec = lua::to_str(l, 1);
    let r: Result<String, String> = (|| {
        if spec.len() > 8192 { return Err("plan is too long".into()); }
        *PLAN.lock().map_err(|_| "state lock poisoned")? = Some(spec.clone());
        Ok(format!("plan stored ({} bytes): {spec}", spec.len()))
    })();
    bool_result(l, r, "se_ar_plan_set")
}

unsafe extern "C" fn se_ar_plan_get(l: *mut LuaState) -> c_int {
    let s = PLAN.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
    lua::push_str(l, &s);
    1
}

unsafe extern "C" fn se_ar_plan_clear(l: *mut LuaState) -> c_int {
    let r: Result<String, String> = (|| {
        let had = PLAN.lock().map_err(|_| "state lock poisoned")?.take().is_some();
        Ok(if had { "plan cleared".into() } else { "no plan was stored".into() })
    })();
    bool_result(l, r, "se_ar_plan_clear")
}
