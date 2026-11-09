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
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn init(t: &Table) {
    let e = unsafe { Engine { can_add: core::mem::transmute(t.get("skill_node_can_add")) } };
    let _ = ENGINE.set(e);
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_skill_nodes", se_skill_nodes);
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
