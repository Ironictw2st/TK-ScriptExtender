//! Horde income: `gdp_abs` effects that land on the FACTION (scopes `force_to_faction_own*`)
//! count as faction income (build 1.7.2.0, RE 2026-09-18).
//!
//! Finance object = FACTION+0x2a8, `*(finance+0x888)` = faction, ring of ten 0xd8-byte snapshots,
//! current index at +0x87c (the live one is index-1, wrapped). Per snapshot: income A = 4 ints at
//! +0x5c, income B = 5 ints at +0x6c, expenses = 11 ints at +0xb4;
//! projected_net_income (FUN_14144eec0) = sum(A) + sum(B) - sum(expenses).
//! **FUN_141451510(finance, category)** recomputes income A[category]: 0 = sum of
//! FUN_141944360(region) over the faction's regions (tax rate x region GDP `region+0xbc`),
//! 2 = FUN_141434d00, 3 = FUN_141775690. A faction without regions gets 0 for category 0.
//!
//! `gdp_abs` effects = bonus value kind "region gdp type", id 0 `region_gdp` (1 = region_gdp_mod,
//! 2 = max_provided_region_gdp_mod; name table 0x143e61600), one record per
//! campaign_region_gdp_types row. An effect holder's values (FUN_141415530(holder+0x18)) are a
//! sorted vector {+4 count, +8 data} of 0x18-byte entries {u16 id, u8 kind @+2, value @+4,
//! record @+0x10} (generic getter FUN_140ac9a20(ctx, kind, record, id)). With a force-to-faction
//! scope the entries sit on the faction, where the region income code never looks.
//!
//! Hook (cfg `horde_income=1`, off by default): after the original ran for category 0, the sum
//! of the faction's `region_gdp` entries whose record is a GDP type is added to that slot, 1:1.
//!
//! se_faction_gdp_bonus(q_faction) -> sum, dump:string | nil, msg   (works without the hook)

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::sync::OnceLock;

const GDP_TYPES: [&str; 22] = [
    "animal_husbandry", "banditry", "commerce", "culture", "entertainment", "farming", "fertility",
    "fertility_agriculture", "fertility_livestock", "industry", "land_trade", "learning",
    "local_trade", "manufacture", "mining", "other", "peasantry", "population", "sea_trade", "silk",
    "spice", "subsistence",
];
const ID_REGION_GDP: u32 = 0;

type UpdateIncome = unsafe extern "C" fn(*mut c_void, u32);
type EffectCtx = unsafe extern "C" fn(*mut c_void) -> *mut c_void;

static HOOK: OnceLock<GenericDetour<UpdateIncome>> = OnceLock::new();
static EFFECT_CTX: OnceLock<usize> = OnceLock::new();
static LOGGED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(8);

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
    if !readable(sp, 16) { return String::new(); }
    if rq(sp + 8) >> 60 == 8 {
        let raw = core::slice::from_raw_parts(sp as *const u8, 15);
        let n = raw.iter().position(|&c| c == 0).unwrap_or(15);
        let ok = n > 0 && raw[..n].iter().all(|c| c.is_ascii_graphic());
        return if ok { String::from_utf8_lossy(&raw[..n]).into_owned() } else { String::new() };
    }
    let (len, ptr) = (rd(sp) as usize, rq(sp + 8));
    if len == 0 || len > 96 || !readable(ptr, len) { return String::new(); }
    String::from_utf8_lossy(core::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
}

unsafe fn record_key(rec: usize) -> String {
    if rec == 0 || !readable(rec, 0x20) { return String::new(); }
    let s = ca_string(rq(rec + 8));
    if !s.is_empty() { return s; }
    ca_string(rec + 8)
}

/// Value stored in an entry: the engine has float and integer getters over the same field; take
/// the float when the bits are a sane float, else the integer.
fn entry_value(bits: u32) -> f32 {
    let f = f32::from_bits(bits);
    if f.is_finite() && (f == 0.0 || (f.abs() > 1.0e-3 && f.abs() < 1.0e8)) { f } else {
        let i = bits as i32;
        if i.abs() < 100_000_000 { i as f32 } else { 0.0 }
    }
}

/// (sum of the faction's region_gdp values on GDP type records, per-entry description)
unsafe fn faction_gdp_bonus(faction: usize) -> Result<(f32, Vec<String>), String> {
    let f: EffectCtx = core::mem::transmute(*EFFECT_CTX.get().ok_or("engine table missing")?);
    let ctx = f((faction + 0x18) as *mut c_void) as usize;
    if ctx == 0 || !readable(ctx, 0x10) { return Err("faction effect values not available".into()); }
    let (count, data) = (rd(ctx + 4) as usize, rq(ctx + 8));
    if count > 20_000 || (count > 0 && !readable(data, count * 0x18)) {
        return Err(format!("effect value vector not plausible (count {count}, data {:#x})", data));
    }
    let (mut sum, mut rows) = (0.0f32, Vec::new());
    for i in 0..count {
        let en = data + i * 0x18;
        let rec = rq(en + 0x10);
        if rec == 0 { continue; }
        let key = record_key(rec);
        if !GDP_TYPES.contains(&key.as_str()) { continue; }
        let (id, kind, v) = (rd(en) & 0xffff, (rd(en) >> 16) & 0xff, entry_value(rd(en + 4)));
        rows.push(format!("kind {kind} id {id} {key} = {v} (raw {:#010x})", rd(en + 4)));
        if id == ID_REGION_GDP { sum += v; }
    }
    Ok((sum, rows))
}

unsafe extern "C" fn update_income_detour(finance: *mut c_void, category: u32) {
    let Some(hook) = HOOK.get() else { return };
    hook.call(finance, category);
    if category != 0 { return; }
    let fin = finance as usize;
    if !readable(fin, 0x890) { return; }
    let faction = rq(fin + 0x888);
    if faction == 0 || !readable(faction, 0x2b0) || faction + 0x2a8 != fin { return; }
    let Ok((sum, rows)) = faction_gdp_bonus(faction) else { return };
    let extra = sum.round() as i32;
    if extra == 0 { return; }
    let cur = rd(fin + 0x87c) as i32;
    if !(0..=10).contains(&cur) { return; }
    let idx = if cur - 1 < 0 { cur + 9 } else { cur - 1 } as usize;
    let slot = fin + 0x5c + idx * 0xd8;
    let before = rd(slot) as i32;
    core::ptr::write_unaligned(slot as *mut i32, before.saturating_add(extra));
    if LOGGED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) > 0 {
        log!("horde income: faction {:#x} region income {before} + force-scoped gdp {extra} ({})", faction, rows.join("; "));
    }
}

pub fn install(t: &Table) {
    let _ = EFFECT_CTX.set(t.get("effect_ctx"));
    if crate::build::config_value("horde_income").as_deref() != Some("1") {
        log!("horde income hook off (set horde_income=1 in script_extender.cfg to enable)");
        return;
    }
    let target: UpdateIncome = unsafe { core::mem::transmute(t.get("finance_update_income")) };
    // SAFETY: anchor-verified prologue (mov [rsp+8],rbx; push rdi; sub rsp,0x20; xor r8d,r8d;
    // mov edi,edx; mov rbx,rcx; mov eax,edx): no RIP-relative instruction in the relocated bytes.
    unsafe {
        match GenericDetour::new(target, update_income_detour) {
            Ok(d) => {
                if let Err(e) = crate::freeze::with_threads_frozen(target as usize, 16, || d.enable()) {
                    log!("failed to enable the horde income hook: {e}");
                    return;
                }
                let _ = HOOK.set(d);
                log!("horde income hook installed");
            }
            Err(e) => log!("failed to create the horde income hook: {e}"),
        }
    }
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_faction_gdp_bonus", se_faction_gdp_bonus);
}

unsafe extern "C" fn se_faction_gdp_bonus(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let r = crate::progression::faction_from_arg(l, 1).and_then(|f| faction_gdp_bonus(f));
    match r {
        Ok((sum, rows)) => {
            log!("se_faction_gdp_bonus: sum {sum}; hook {}; {}", if HOOK.get().is_some() { "on" } else { "off" }, rows.join("; "));
            (api.pushnumber)(l, sum);
            lua::push_str(l, &rows.join("\n"));
            2
        }
        Err(e) => {
            log!("se_faction_gdp_bonus: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}
