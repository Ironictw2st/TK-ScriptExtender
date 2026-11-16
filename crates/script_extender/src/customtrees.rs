//! Creator S8: player-made skill trees (notes/skill_trees.md).
//!
//! creatormech ships 64 custom node sets `190e_custom_skillset_<nn>` (nn 01..64) with 18 nodes each,
//! `190e_custom_skillset_<nn>_<ii>` (ii 01..18), defaulting to the vanilla fire general's skills.
//! A player tree overrides some nodes' skills. The SE owns slot allocation (portrait rules):
//!   - same definition (hash over the sorted ii=skill pairs) -> its slot; new -> a never-used slot;
//!   - a used slot is never redefined (an older save's leader may use it); refused when all 64 used.
//! Manifest: %APPDATA%\The Creative Assembly\ThreeKingdoms\190e_custom_skilltrees.txt, one line per
//! slot: `nn<TAB>hash<TAB>class<TAB>ii=skill,ii=skill,...`.
//!
//! Applying: the tree is built from the node records when a campaign loads, BEFORE any first-tick
//! Lua. So a detour on the engine's tree builder FUN_141a65390 (cfg `creator_skill_trees`, default
//! on) applies the manifest to the DB once per database (and again when the manifest changes)
//! before the first build: node record (character_skill_nodes) +0x78 := character_skills record.

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

type P = *mut c_void;
type Build = unsafe extern "C" fn(P, P, P, P, P, P, usize, u8, u8, P) -> usize;

const SLOTS: u32 = 64;
const NODES: u32 = 18;
const NODE_SKILL: usize = 0x78;

struct Fns {
    db_get: unsafe extern "C" fn(P) -> P,
    nodes_table: unsafe extern "C" fn(P) -> P,
    skills_table: unsafe extern "C" fn(P) -> P,
    record_base: unsafe extern "C" fn(P, *const c_void) -> P,
    str_from_cstr: unsafe extern "C" fn(P, *const u8) -> P,
    string_dtor: unsafe extern "C" fn(P),
}
static FNS: OnceLock<Fns> = OnceLock::new();
static BUILD: OnceLock<GenericDetour<Build>> = OnceLock::new();
/// bumped whenever the manifest file changes through define (this process)
static GEN: AtomicU64 = AtomicU64::new(1);
static APPLIED_DB: AtomicUsize = AtomicUsize::new(0);
static APPLIED_GEN: AtomicU64 = AtomicU64::new(0);

#[link(name = "kernel32")]
extern "system" { fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32; }
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }

pub fn init(t: &Table) {
    let _ = FNS.set(unsafe {
        Fns {
            db_get: core::mem::transmute(t.get("db_get")),
            nodes_table: core::mem::transmute(t.get("skill_nodes_table")),
            skills_table: core::mem::transmute(t.get("skills_table")),
            record_base: core::mem::transmute(t.get("record_base")),
            str_from_cstr: core::mem::transmute(t.get("string_from_cstr")),
            string_dtor: core::mem::transmute(t.get("string_dtor")),
        }
    });
}

pub fn install(t: &Table) {
    if !crate::build::hook_enabled("creator_skill_trees") {
        log!("creator skill tree hook off (creator_skill_trees=0): custom skill trees are not applied");
        return;
    }
    // SAFETY: anchor-verified prologue (spills of rcx/rdx/r8/r9, pushes): no RIP-relative operand.
    unsafe {
        let target: Build = core::mem::transmute(t.get("skill_tree_build"));
        let Ok(d) = GenericDetour::new(target, build_detour) else { log!("creator skill tree hook: could not create the detour"); return };
        let _ = BUILD.set(d);
        let Some(d) = BUILD.get() else { return };
        if let Err(e) = crate::freeze::enable_detour("skill_tree_build", "creator_skill_trees", target as usize, d) {
            log!("creator skill tree hook: could not enable the detour ({e}); off");
            return;
        }
    }
    log!("creator skill tree hook installed: custom skill tree slots are applied before the engine builds a tree");
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_custom_skill_tree_define", se_custom_skill_tree_define);
}

unsafe extern "C" fn build_detour(t: P, fac: P, set: P, a: P, b: P, c: P, d: usize, e: u8, f: u8, g: P) -> usize {
    let Some(h) = BUILD.get() else { return 0 };
    {
        let _g = crate::crash::enter_quiet("skill_tree_build");
        let fu = fac as usize;
        if fu != 0 && readable(fu + 0x288, 8) {
            let model = rq(rq(fu + 0x288) + 0x78);
            if model != 0 && readable(model + 0x3b38, 8) {
                if let Some(fns) = FNS.get() {
                    let db = (fns.db_get)((model + 0x3b38) as P) as usize;
                    let gen = GEN.load(Ordering::Relaxed);
                    if db != 0 && (APPLIED_DB.load(Ordering::Relaxed) != db || APPLIED_GEN.load(Ordering::Relaxed) != gen) {
                        let n = apply_manifest(fns, db);
                        APPLIED_DB.store(db, Ordering::Relaxed);
                        APPLIED_GEN.store(gen, Ordering::Relaxed);
                        log!("custom skill trees: manifest applied to db {db:#x} ({n} nodes rewritten)");
                    }
                }
            }
        }
    }
    h.call(t, fac, set, a, b, c, d, e, f, g)
}

fn tw_root() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("The Creative Assembly").join("ThreeKingdoms"))
}
fn manifest_path() -> Option<PathBuf> { tw_root().map(|r| r.join("190e_custom_skilltrees.txt")) }

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 120 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// spec "ii=skill,ii=skill" -> sorted, validated pairs
fn parse_spec(spec: &str) -> Result<Vec<(u32, String)>, String> {
    let mut v: Vec<(u32, String)> = Vec::new();
    for part in spec.split(',').filter(|s| !s.is_empty()) {
        let (i, s) = part.split_once('=').ok_or_else(|| format!("bad cell '{part}'"))?;
        let i: u32 = i.trim().parse().map_err(|_| format!("bad index in '{part}'"))?;
        if !(1..=NODES).contains(&i) { return Err(format!("layout index {i} out of 1..{NODES}")); }
        let s = s.trim();
        if !valid_key(s) { return Err(format!("bad skill key '{s}'")); }
        if v.iter().any(|(j, _)| *j == i) { return Err(format!("layout index {i} given twice")); }
        v.push((i, s.to_string()));
    }
    if v.is_empty() { return Err("no cells".into()); }
    v.sort();
    Ok(v)
}

fn spec_string(v: &[(u32, String)]) -> String {
    v.iter().map(|(i, s)| format!("{i:02}={s}")).collect::<Vec<_>>().join(",")
}

fn fnv64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b { h ^= *x as u64; h = h.wrapping_mul(0x100000001b3); }
    h
}

/// slot -> (hash, class, cells)
fn load_manifest() -> std::collections::BTreeMap<u32, (u64, String, Vec<(u32, String)>)> {
    let mut m = std::collections::BTreeMap::new();
    let Some(p) = manifest_path() else { return m };
    let Ok(t) = std::fs::read_to_string(&p) else { return m };
    for line in t.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 4 { continue; }
        let Ok(slot) = f[0].trim().parse::<u32>() else { continue };
        if !(1..=SLOTS).contains(&slot) { continue; }
        let hash = u64::from_str_radix(f[1].trim(), 16).unwrap_or(0);
        let class = if valid_key(f[2].trim()) { f[2].trim().to_string() } else { String::new() };
        // a line that does not parse still marks the slot as used (never reuse an unknown slot)
        let cells = parse_spec(f[3].trim()).unwrap_or_default();
        m.insert(slot, (hash, class, cells));
    }
    m
}

fn save_manifest(m: &std::collections::BTreeMap<u32, (u64, String, Vec<(u32, String)>)>) -> Result<(), String> {
    let p = manifest_path().ok_or("APPDATA not set")?;
    let mut t = String::new();
    for (slot, (hash, class, cells)) in m {
        t.push_str(&format!("{slot:02}\t{hash:016x}\t{class}\t{}\n", spec_string(cells)));
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, t).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

unsafe fn lookup(fns: &Fns, table: P, key: &str) -> usize {
    let mut c = key.as_bytes().to_vec();
    c.push(0);
    let mut s = [0u8; 64];
    (fns.str_from_cstr)(s.as_mut_ptr() as P, c.as_ptr());
    let r = (fns.record_base)(table, s.as_ptr() as *const c_void) as usize;
    (fns.string_dtor)(s.as_mut_ptr() as P);
    r
}

/// Rewrite node -> skill for every manifest slot. Returns the number of nodes rewritten.
unsafe fn apply_manifest(fns: &Fns, db: usize) -> usize {
    let man = load_manifest();
    if man.is_empty() { return 0; }
    let nodes = (fns.nodes_table)(db as P);
    let skills = (fns.skills_table)(db as P);
    if nodes.is_null() || skills.is_null() { log!("custom skill trees: node / skill table not loaded"); return 0; }
    let (mb, ms) = crate::process::main_module();
    let mut n = 0;
    for (slot, (_, _, cells)) in &man {
        for (i, skill_key) in cells {
            let node_key = format!("190e_custom_skillset_{slot:02}_{i:02}");
            let node = lookup(fns, nodes, &node_key);
            let skill = lookup(fns, skills, skill_key);
            if node == 0 || skill == 0 || !readable(node, NODE_SKILL + 8) || !readable(skill, 0x10) {
                log!("custom skill trees: slot {slot} cell {i}: node '{node_key}' or skill '{skill_key}' not found; skipped");
                continue;
            }
            let old = rq(node + NODE_SKILL);
            // the slot must point at a skill record of the same type, inside the exe's vtables
            if old == 0 || !readable(old, 8) || rq(old) != rq(skill) || rq(skill) < mb || rq(skill) >= mb + ms {
                log!("custom skill trees: slot {slot} cell {i}: unexpected node layout; skipped");
                continue;
            }
            core::ptr::write_unaligned((node + NODE_SKILL) as *mut usize, skill);
            n += 1;
        }
    }
    n
}

/// se_custom_skill_tree_define(spec "ii=skill,...", class_key) -> "190e_custom_skillset_<nn>", nn | nil, msg
unsafe extern "C" fn se_custom_skill_tree_define(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let spec = lua::to_str(l, 1);
    let class = lua::to_str(l, 2);
    let out = (|| -> Result<(String, u32, bool), String> {
        let cells = parse_spec(&spec)?;
        let class = if valid_key(&class) { class.clone() } else { String::new() };
        let hash = fnv64(spec_string(&cells).as_bytes());
        let mut man = load_manifest();
        if let Some((slot, _)) = man.iter().find(|(_, (h, _, c))| *h == hash && *c == cells) {
            return Ok((format!("190e_custom_skillset_{slot:02}"), *slot, true));
        }
        let Some(slot) = (1..=SLOTS).find(|s| !man.contains_key(s)) else {
            return Err(format!("all {SLOTS} custom skill tree slots are in use (older campaigns may still use them); none was redefined"));
        };
        man.insert(slot, (hash, class, cells));
        save_manifest(&man)?;
        GEN.fetch_add(1, Ordering::Relaxed);
        Ok((format!("190e_custom_skillset_{slot:02}"), slot, false))
    })();
    match out {
        Ok((k, slot, reused)) => {
            log!("se_custom_skill_tree_define: -> {k}{}", if reused { " (same definition, reused)" } else { "" });
            lua::push_str(l, &k);
            (api.pushinteger)(l, slot as _);
            2
        }
        Err(e) => { log!("se_custom_skill_tree_define: {e}"); (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}
