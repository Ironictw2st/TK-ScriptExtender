//! Character skill trees (build 1.7.2.0), from the CCQ_ADD_CAMPAIGN_SKILL_POINT executor
//! FUN_141b45e40 and CCQ_COMMIT_CAMPAIGN_SKILL_CHOICES FUN_141b48310 (notes/AI_CAMPAIGN.md):
//!   DETAILS = **(CHARACTER + 0x260); TREE T = DETAILS + 0x98
//!   T + 0x04 row count, T + 0x08 rows (0x10 each: +4 node count, +8 nodes)
//!   node (0x40): +0 node record, +0x08 level-details override vector (else (*node + 0x78) + 0xa0),
//!     +0x10 u8 points in the node, +0x11 u8 locked, +0x28 back pointer
//!     level-details vector: +4 count (level 0 included, so max points = count - 1)
//!   T + 0x28 (= DETAILS + 0xc0) unspent skill points; T + 0x58/+0x5c/+0x60 pending choices
//!   FUN_141a4e5f0(node) = "can add a point" (pure check: level + 1 < count, not locked, the
//!   node-level unlock requirements met); it does not look at the unspent points.
//!
//! Natives:
//!   se_skill_nodes(q, cqi) -> { points, rows, nodes = { {row, col, level, max, can_add, locked,
//!                                node, skill}, ... } } | nil, msg       (read-only)

use crate::addrs::Table;
use crate::buildings::record_key;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

struct Engine {
    can_add: unsafe extern "C" fn(*mut c_void) -> u8,
    // FUN_141a38e90(details, node): what CCQ_ADD_CAMPAIGN_SKILL_POINT runs (pending point + recompute + event)
    add: unsafe extern "C" fn(*mut c_void, *mut c_void),
    // FUN_141a54020(tree, details): CCQ_COMMIT_CAMPAIGN_SKILL_CHOICES step 1 (apply pending, clear)
    commit: unsafe extern "C" fn(*mut c_void, *mut c_void),
    // FUN_141422050(force legacy object): commit step 2, only when the character has one
    commit_legacy: unsafe extern "C" fn(*mut c_void),
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe {
        Engine {
            can_add: core::mem::transmute(t.get("skill_node_can_add")),
            add: core::mem::transmute(t.get("skill_add_point")),
            commit: core::mem::transmute(t.get("skill_commit")),
            commit_legacy: core::mem::transmute(t.get("skill_commit_legacy")),
        }
    };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_skill_nodes", se_skill_nodes);
    lua::set_global_fn(l, "se_skill_add", se_skill_add);
}

/// Force legacy object of a DETAILS, as both CCQ executors reach it:
/// details+0xbd0 -> * -> +0x1b0 -> * (0 when the character has none).
unsafe fn legacy_of(d: usize) -> usize {
    let p = rq(d + 0xbd0);
    if p == 0 { return 0; }
    let a = rq(p);
    if a == 0 { return 0; }
    let b = rq(a + 0x1b0);
    if b == 0 { return 0; }
    rq(b)
}

/// se_skill_add(q, cqi, row, col) -> ok, msg
/// Spends one unspent point on the node at (row, col) of the character tree and commits it,
/// i.e. CCQ_ADD_CAMPAIGN_SKILL_POINT (FUN_141b45e40, legacy flag 0) followed by
/// CCQ_COMMIT_CAMPAIGN_SKILL_CHOICES (FUN_141b48310), run directly instead of queued so every
/// machine of a multiplayer game executes it once inside the same model callback.
/// Refused when there is no unspent point or the engine's own check says the node can't take one.
unsafe extern "C" fn se_skill_add(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let row = (api.tointeger)(l, 3);
    let col = (api.tointeger)(l, 4);
    let r = (|| -> Result<String, String> {
        let e = ENGINE.get().ok_or("engine table missing")?;
        let (d, t) = tree_of(l, cqi)?;
        if row < 0 || col < 0 { return Err(format!("bad node ({row}, {col})")); }
        let list = nodes(t)?;
        let n = list.iter().find(|n| n.row as isize == row && n.col as isize == col)
            .ok_or_else(|| format!("no node at ({row}, {col})"))?;
        let points = rd(t + 0x28);
        if points == 0 { return Err("no unspent skill points".into()); }
        if rd(t + 0x5c) != 0 { return Err(format!("{} uncommitted choices pending; refusing to commit someone else's choices", rd(t + 0x5c))); }
        if !can_add(n.addr) { return Err(format!("node ({row}, {col}) '{}' cannot take a point", record_key(rq(rq(n.addr) + 0x78)))); }
        let before = rb(n.addr + 0x10);
        (e.add)(d as *mut c_void, n.addr as *mut c_void);
        let after = rb(n.addr + 0x10);
        let left = rd(t + 0x28);
        if after != before.wrapping_add(1) || left + 1 != points {
            // nothing was queued (or something unexpected was): still commit what is pending so
            // the tree is never left with half-applied choices, and report it.
            (e.commit)(t as *mut c_void, d as *mut c_void);
            return Err(format!("add did not take: level {before} -> {after}, points {points} -> {left}"));
        }
        (e.commit)(t as *mut c_void, d as *mut c_void);
        let leg = legacy_of(d);
        if leg != 0 { (e.commit_legacy)(leg as *mut c_void); }
        // notification exactly as FUN_141b48310: model = **(details+0x68); x = *(*(model+0x250)+0x78);
        // (x+0x1310)->vtable[2](x+0x1310, &details)
        let m = rq(rq(d + 0x68));
        let x = if m != 0 { rq(rq(m + 0x250) + 0x78) } else { 0 };
        if x != 0 && readable(x + 0x1310, 8) {
            let obj = x + 0x1310;
            let f = rq(rq(obj) + 0x10);
            if f != 0 {
                let notify: unsafe extern "C" fn(*mut c_void, *mut usize) = core::mem::transmute(f);
                let mut dd = d;
                notify(obj as *mut c_void, &mut dd);
            }
        }
        Ok(format!("cqi {cqi}: '{}' level {before} -> {after}, points {points} -> {left}{}",
            record_key(rq(rq(n.addr) + 0x78)), if leg != 0 { " (legacy committed)" } else { "" }))
    })();
    match r {
        Ok(m) => { log!("se_skill_add: {m}"); (api.pushboolean)(l, 1); lua::push_str(l, &m); 2 }
        Err(m) => { log!("se_skill_add: cqi {cqi}: {m}"); (api.pushboolean)(l, 0); lua::push_str(l, &m); 2 }
    }
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
unsafe fn rb(p: usize) -> u8 {
    if readable(p, 1) { core::ptr::read_unaligned(p as *const u8) } else { 0 }
}

const MAX_ROWS: u32 = 64;
const MAX_COLS: u32 = 64;

pub struct Node {
    pub row: u32,
    pub col: u32,
    pub addr: usize,
}

/// DETAILS of the character at (q, cqi) and its tree.
pub unsafe fn tree_of(l: *mut LuaState, cqi: u32) -> Result<(usize, usize), String> {
    let ch = crate::pool::character_from_arg(l, 1, cqi)?;
    let d = rq(rq(ch + 0x260));
    if d == 0 || !readable(d, 0xd00) {
        return Err("character has no details object".into());
    }
    Ok((d, d + 0x98))
}

/// Every node of a tree, in row/column order (bounds-checked against the engine's own counts).
pub unsafe fn nodes(t: usize) -> Result<Vec<Node>, String> {
    let rows = rd(t + 4);
    let data = rq(t + 8);
    if rows > MAX_ROWS { return Err(format!("tree row count {rows} out of range")); }
    if rows > 0 && !readable(data, rows as usize * 0x10) { return Err("tree rows not readable".into()); }
    let mut out = Vec::new();
    for r in 0..rows {
        let row = data + r as usize * 0x10;
        let n = rd(row + 4);
        let nd = rq(row + 8);
        if n > MAX_COLS || (n > 0 && !readable(nd, n as usize * 0x40)) {
            return Err(format!("tree row {r} bad (count {n})"));
        }
        for c in 0..n {
            out.push(Node { row: r, col: c, addr: nd + c as usize * 0x40 });
        }
    }
    Ok(out)
}

/// Number of level entries of a node (level 0 included), as FUN_141a4e5f0 reads it.
pub unsafe fn level_count(node: usize) -> u32 {
    let mut v = rq(node + 8);
    if v == 0 {
        let rec = rq(node);
        let srec = rq(rec + 0x78);
        if srec == 0 { return 0; }
        v = srec + 0xa0;
    }
    rd(v + 4)
}

pub unsafe fn can_add(node: usize) -> bool {
    match ENGINE.get() {
        Some(e) => rq(node) != 0 && (e.can_add)(node as *mut c_void) != 0,
        None => false,
    }
}

unsafe fn set_field_int(l: *mut LuaState, name: &str, v: isize) {
    let Some(api) = lua::api() else { return };
    let mut k = name.as_bytes().to_vec();
    k.push(0);
    (api.pushinteger)(l, v);
    (api.setfield)(l, -2, k.as_ptr() as *const c_char);
}

unsafe fn set_field_bool(l: *mut LuaState, name: &str, v: bool) {
    let Some(api) = lua::api() else { return };
    let mut k = name.as_bytes().to_vec();
    k.push(0);
    (api.pushboolean)(l, v as c_int);
    (api.setfield)(l, -2, k.as_ptr() as *const c_char);
}

unsafe extern "C" fn se_skill_nodes(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let cqi = (api.tointeger)(l, 2) as u32;
    let r = (|| -> Result<(usize, Vec<Node>), String> {
        let (_, t) = tree_of(l, cqi)?;
        Ok((t, nodes(t)?))
    })();
    match r {
        Ok((t, list)) => {
            (api.createtable)(l, 0, 3);
            set_field_int(l, "points", rd(t + 0x28) as isize);
            set_field_int(l, "rows", rd(t + 4) as isize);
            (api.createtable)(l, list.len() as c_int, 0);
            for (i, n) in list.iter().enumerate() {
                let rec = rq(n.addr);
                let count = level_count(n.addr);
                (api.createtable)(l, 0, 8);
                set_field_int(l, "row", n.row as isize);
                set_field_int(l, "col", n.col as isize);
                set_field_int(l, "level", rb(n.addr + 0x10) as isize);
                set_field_int(l, "max", count.saturating_sub(1) as isize);
                set_field_bool(l, "can_add", can_add(n.addr));
                set_field_bool(l, "locked", rb(n.addr + 0x11) != 0);
                lua::set_field_str(l, "node", &record_key(rec));
                lua::set_field_str(l, "skill", &record_key(rq(rec + 0x78)));
                (api.rawseti)(l, -2, (i + 1) as c_int);
            }
            let k = b"nodes\0";
            (api.setfield)(l, -2, k.as_ptr() as *const c_char);
            1
        }
        Err(e) => {
            log!("se_skill_nodes: cqi {cqi}: {e}");
            (api.pushnil)(l);
            lua::push_str(l, &e);
            2
        }
    }
}
