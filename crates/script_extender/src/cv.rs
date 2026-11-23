//! Context-expression evaluator: the engine's own compiler + interpreter behind the dev Context
//! Viewer, exposed as one read-only native (notes/context_viewer.md, tools/secli).
//!
//! se_cv_eval(expr, max_rows, want_names) -> table
//!   { ok, kind = "value"|"context"|"list"|"command"|"void"|"error", value, type, id, name,
//!     count, rows = { {type, id, name}, ... }, truncated, errors = { "...", ... }, tag }
//!
//! The sequence is FUN_142d455b0's (the viewer's expression box), evaluated against a fresh
//! CcoCampaignRoot (func 0x142fedc00 = an 8-byte object holding vtable 0x143846ae8):
//!   errors   {vtbl 0x1437e7cc8, u32 cap, u32 count, data, 0}; entries 0x20 bytes (CA::String first)
//!   FUN_140544de0(parse_state, ctx->vfn60(), &text, &errors)    parse state <= 0x1b0 bytes
//!   FUN_1405610a0(&compiled, parse_state)                         owning pointer, vfn0(obj, 1) frees
//!   FUN_140548e10(evalctx, ctx, &errors)                          evalctx ~0x5e28 bytes
//!   compiled->vfn8(compiled, &ret, evalctx, &variant)             variant 0xc0 bytes, tag at +0xb0
//!   tag 1 i32 / 2 f32 / 1..7 to text FUN_140519dc0; 8 context at +0x10; 9 list at +0x10
//!   ({?, u32 count +4, ptr data +8} of context pointers); 10 = command list (never executed)
//!   cleanup: FUN_1405baec0(variant + 0x10), FUN_142d26090(evalctx), FUN_14054b4e0(parse_state)
//!
//! Safety rules: opt-in (cfg `dev_cv_eval=1`), campaign only (model global 0x1443cfa50 non-null),
//! read-only: expressions the root reports as commands (vfn 0x88, the viewer's own test) are
//! refused before evaluation and a command-typed result (tag 10) is reported, never run. Not in
//! the sync tag: it changes nothing. Results may differ per machine (UI state), so mod code must
//! not branch on them in multiplayer; it is a developer/diagnostic tool.
//! Never point it at the contexts search (FUN_142d62d10): that path crashed the game (22 Nov).

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::sync::OnceLock;

type P = *mut c_void;

struct Fns {
    base: usize,
    parse_init: unsafe extern "C" fn(P, P, P, P) -> P,
    compile: unsafe extern "C" fn(P, P) -> P,
    parse_free: unsafe extern "C" fn(P),
    evalctx_init: unsafe extern "C" fn(P, P, P) -> P,
    evalctx_free: unsafe extern "C" fn(P),
    to_string: unsafe extern "C" fn(P, P) -> P,
    variant_free: unsafe extern "C" fn(P),
    error_free: unsafe extern "C" fn(P),
    str_new: unsafe extern "C" fn(P, P) -> P,
    free: unsafe extern "C" fn(P),
    root_vtable: usize,
    errors_vtable: usize,
    variant_vtable: usize,
}
unsafe impl Send for Fns {}
unsafe impl Sync for Fns {}

static FNS: OnceLock<Fns> = OnceLock::new();

/// Campaign model global (the viewer's "Campaign Root" shortcut is only offered while it is set).
const MODEL_GLOBAL_RVA: usize = 0x43cfa50;
/// CA::String empty sentinels (narrow / wide), see the viewer's string clean-ups.
const EMPTY_NARROW_RVA: usize = 0x3c5b3ee;
const EMPTY_WIDE_RVA: usize = 0x3c5b3f0;
const MAX_TEXT: usize = 8192;

fn enabled() -> bool {
    crate::build::config_value("dev_cv_eval").as_deref().map(str::trim) == Some("1")
}

pub fn install(t: &Table) {
    unsafe {
        let _ = FNS.set(Fns {
            base: t.base(),
            parse_init: core::mem::transmute(t.get("cv_parse_init")),
            compile: core::mem::transmute(t.get("cv_compile")),
            parse_free: core::mem::transmute(t.get("cv_parse_free")),
            evalctx_init: core::mem::transmute(t.get("cv_evalctx_init")),
            evalctx_free: core::mem::transmute(t.get("cv_evalctx_free")),
            to_string: core::mem::transmute(t.get("cv_variant_to_string")),
            variant_free: core::mem::transmute(t.get("cv_variant_free")),
            error_free: core::mem::transmute(t.get("cv_error_free")),
            str_new: core::mem::transmute(t.get("cv_str_new")),
            free: core::mem::transmute(t.get("engine_free")),
            root_vtable: t.get("cv_root_vtable"),
            errors_vtable: t.get("cv_errors_vtable"),
            variant_vtable: t.get("cv_variant_vtable"),
        });
    }
    if enabled() {
        log!("context expression evaluator enabled (dev_cv_eval=1): se.cv.eval");
    }
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_cv_eval", se_cv_eval);
}

extern "system" {
    fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
}
unsafe fn readable(p: usize, n: usize) -> bool { p >= 0x10000 && IsBadReadPtr(p as *const c_void, n) == 0 }
unsafe fn rq(p: usize) -> usize { if readable(p, 8) { core::ptr::read_unaligned(p as *const usize) } else { 0 } }
unsafe fn rd(p: usize) -> u32 { if readable(p, 4) { core::ptr::read_unaligned(p as *const u32) } else { 0 } }
unsafe fn vfn(obj: usize, off: usize) -> usize { rq(rq(obj) + off) }

/// Zeroed, 16-byte aligned scratch for an engine object of up to `n` bytes.
struct Buf(Vec<u128>);
impl Buf {
    fn new(n: usize) -> Buf { Buf(vec![0u128; n.div_ceil(16)]) }
    fn p(&mut self) -> P { self.0.as_mut_ptr() as P }
    fn a(&self) -> usize { self.0.as_ptr() as usize }
}

/// CA::String {u32 len, .., data ptr at +8}. Short strings are stored INLINE in the 16 bytes
/// (top nibble of +8 = 8): then +0 is character data, not a length, and the text is
/// NUL-terminated (verified live 22 Nov: "83", "Runtime"). Narrow or UTF-16 is detected from
/// the data (a wide ASCII string has a zero second byte).
unsafe fn ca_string(sp: usize) -> String {
    if !readable(sp, 16) { return String::new(); }
    let raw_ptr = rq(sp + 8);
    if raw_ptr >> 60 == 8 {
        let b = core::slice::from_raw_parts(sp as *const u8, 15);
        if b.len() >= 2 && b[0] != 0 && b[1] == 0 {
            let w = core::slice::from_raw_parts(sp as *const u16, 7);
            let n = w.iter().position(|&c| c == 0).unwrap_or(7);
            return String::from_utf16_lossy(&w[..n]);
        }
        let n = b.iter().position(|&c| c == 0).unwrap_or(15);
        return String::from_utf8_lossy(&b[..n]).into_owned();
    }
    let len = (rd(sp) as usize).min(MAX_TEXT);
    let data = raw_ptr;
    if len == 0 || !readable(data, len) { return String::new(); }
    let bytes = core::slice::from_raw_parts(data as *const u8, len);
    let wide = len >= 2 && bytes[1] == 0 || len == 1 && readable(data, 2) && *(data as *const u8).add(1) == 0;
    if wide && readable(data, len * 2) {
        let w = core::slice::from_raw_parts(data as *const u16, len);
        let n = w.iter().position(|&c| c == 0).unwrap_or(len);
        return String::from_utf16_lossy(&w[..n]);
    }
    let n = bytes.iter().position(|&c| c == 0).unwrap_or(len);
    String::from_utf8_lossy(&bytes[..n]).into_owned()
}

/// Frees a CA::String's heap buffer the way the engine's inlined destructors do.
unsafe fn ca_string_free(f: &Fns, sp: usize) {
    let p = rq(sp + 8);
    if p == 0 || p >> 60 == 8 || p == f.base + EMPTY_NARROW_RVA || p == f.base + EMPTY_WIDE_RVA { return; }
    (f.free)(p as P);
}

/// A CA::String built by the engine's own narrow constructor (FUN_140662df0(dst, const char*),
/// the one the viewer code uses for literals); freed like the engine's inlined destructors.
struct CaStr { buf: Buf, _c: std::ffi::CString }
impl CaStr {
    unsafe fn new(f: &Fns, s: &str) -> CaStr {
        let c = std::ffi::CString::new(s.replace('\0', " ")).unwrap_or_default();
        let mut buf = Buf::new(0x20);
        (f.str_new)(buf.p(), c.as_ptr() as P);
        CaStr { buf, _c: c }
    }
    fn p(&mut self) -> P { self.buf.p() }
    unsafe fn free(&mut self, f: &Fns) { ca_string_free(f, self.buf.a()); }
}

struct Errors { buf: Buf }
impl Errors {
    unsafe fn new(f: &Fns) -> Errors {
        let mut buf = Buf::new(0x20);
        *(buf.p() as *mut usize) = f.errors_vtable;
        Errors { buf }
    }
    fn p(&mut self) -> P { self.buf.p() }
    unsafe fn texts(&self) -> Vec<String> {
        let a = self.buf.a();
        let (n, data) = (rd(a + 0xc) as usize, rq(a + 0x10));
        let mut v = Vec::new();
        if data == 0 { return v; }
        // entry = { CA::String category ("Runtime", ...), CA::String message } (0x20 bytes)
        for i in 0..n.min(64) {
            let cat = ca_string(data + i * 0x20);
            let msg = ca_string(data + i * 0x20 + 0x10);
            let s = match (cat.is_empty(), msg.is_empty()) {
                (false, false) => format!("{cat}: {msg}"),
                (true, false) => msg,
                _ => cat,
            };
            if !s.is_empty() { v.push(s); }
        }
        v
    }
    unsafe fn free(&mut self, f: &Fns) {
        let a = self.buf.a();
        let (n, data) = (rd(a + 0xc) as usize, rq(a + 0x10));
        if data != 0 {
            for i in 0..n { (f.error_free)((data + i * 0x20) as P); }
            (f.free)(data as P);
        }
        core::ptr::write_bytes((a + 8) as *mut u8, 0, 0x18);
    }
}

enum Outcome {
    Value(String, u8),
    Context(usize),
    List(Vec<usize>, usize),
    Command,
    Void(u8),
}

/// Compile + evaluate `text` against `ctx` (an engine context object). Errors go to `errs`.
/// `visit` sees the outcome while the result variant is still alive: context pointers in it are
/// owned by the variant (list elements can be temporaries), so they are only valid inside `visit`.
/// Returns false when nothing was evaluated (compile error).
unsafe fn eval_on(f: &Fns, ctx: usize, text: &str, errs: &mut Vec<String>, max_rows: usize,
                  visit: &mut dyn FnMut(Outcome, &[String])) -> bool {
    let mut errors = Errors::new(f);
    let mut src = CaStr::new(f, text);
    let mut ps = Buf::new(0x400);
    let typeinfo = vfn(ctx, 0x60);
    if typeinfo == 0 { errs.push("context has no type".into()); src.free(f); return false; }
    let ti: unsafe extern "C" fn(P) -> P = core::mem::transmute(typeinfo);
    let ti = ti(ctx as P);
    (f.parse_init)(ps.p(), ti, src.p(), errors.p());
    let mut compiled: usize = 0;
    (f.compile)(&mut compiled as *mut usize as P, ps.p());
    let out = if compiled == 0 {
        errs.extend(errors.texts());
        if errs.is_empty() { errs.push("compile failed".into()); }
        false
    } else {
        let mut ec = Buf::new(0x6000);
        (f.evalctx_init)(ec.p(), ctx as P, errors.p());
        let mut var = Buf::new(0xc0);
        *(var.p() as *mut usize) = f.variant_vtable;
        let mut ret = Buf::new(0x40);
        let ev: unsafe extern "C" fn(P, P, P, P) -> P = core::mem::transmute(vfn(compiled, 8));
        ev(compiled as P, ret.p(), ec.p(), var.p());
        let va = var.a();
        let tag = *((va + 0xb0) as *const u8);
        let o = match tag {
            1..=7 => {
                let mut s = Buf::new(0x20);
                (f.to_string)(var.p(), s.p());
                let text = ca_string(s.a());
                ca_string_free(f, s.a());
                Outcome::Value(text, tag)
            }
            8 => Outcome::Context(rq(va + 0x10)),
            9 => {
                let list = va + 0x10;
                let (n, data) = (rd(list + 4) as usize, rq(list + 8));
                let mut v = Vec::new();
                if data != 0 && readable(data, n.min(max_rows) * 8) {
                    for i in 0..n.min(max_rows) { v.push(rq(data + i * 8)); }
                }
                Outcome::List(v, n)
            }
            10 => Outcome::Command,
            t => Outcome::Void(t),
        };
        errs.extend(errors.texts());
        visit(o, errs);
        (f.variant_free)((va + 0x10) as P);
        (f.evalctx_free)(ec.p());
        let del: unsafe extern "C" fn(P, u32) -> P = core::mem::transmute(vfn(compiled, 0));
        del(compiled as P, 1);
        true
    };
    (f.parse_free)(ps.p());
    errors.free(f);
    src.free(f);
    out
}

/// Type name and id of a context object (vfn28()+0x10 = const char* type name, vfn18 = id).
unsafe fn ctx_type_id(f: &Fns, ctx: usize) -> (String, String) {
    if !readable(ctx, 8) || !readable(rq(ctx), 0x68) { return (String::new(), String::new()); }
    let desc: unsafe extern "C" fn(P) -> P = core::mem::transmute(vfn(ctx, 0x28));
    let d = desc(ctx as P) as usize;
    let name_p = rq(d + 0x10);
    let ty = if readable(name_p, 1) {
        let mut n = 0;
        while n < 128 && readable(name_p + n, 1) && *((name_p + n) as *const u8) != 0 { n += 1; }
        String::from_utf8_lossy(core::slice::from_raw_parts(name_p as *const u8, n)).into_owned()
    } else { String::new() };
    let mut s = Buf::new(0x20);
    let idf: unsafe extern "C" fn(P, P) -> P = core::mem::transmute(vfn(ctx, 0x18));
    idf(ctx as P, s.p());
    let id = ca_string(s.a());
    ca_string_free(f, s.a());
    (ty, id)
}

unsafe fn ctx_name(f: &Fns, ctx: usize) -> Option<String> {
    let mut e = Vec::new();
    let mut name = None;
    eval_on(f, ctx, "Name", &mut e, 1, &mut |o, _| { if let Outcome::Value(s, _) = o { name = Some(s); } });
    if e.is_empty() { name } else { None }
}

unsafe fn model_ready(f: &Fns) -> bool { rq(f.base + MODEL_GLOBAL_RVA) != 0 }

// ------------------------------------------------------------------------------- Lua result
unsafe fn set_str(l: *mut LuaState, k: &str, v: &str) { lua::set_field_str(l, k, v); }
unsafe fn set_bool(l: *mut LuaState, k: &str, v: bool) {
    let Some(api) = lua::api() else { return };
    let key = std::ffi::CString::new(k).unwrap_or_default();
    (api.pushboolean)(l, v as c_int);
    (api.setfield)(l, -2, key.as_ptr() as *const c_char);
}
unsafe fn set_int(l: *mut LuaState, k: &str, v: i64) {
    let Some(api) = lua::api() else { return };
    let key = std::ffi::CString::new(k).unwrap_or_default();
    (api.pushinteger)(l, v as isize);
    (api.setfield)(l, -2, key.as_ptr() as *const c_char);
}
unsafe fn set_list(l: *mut LuaState, k: &str, items: &[String]) {
    let Some(api) = lua::api() else { return };
    (api.createtable)(l, items.len() as c_int, 0);
    for (i, s) in items.iter().enumerate() {
        lua::push_str(l, s);
        (api.rawseti)(l, -2, (i + 1) as c_int);
    }
    let key = std::ffi::CString::new(k).unwrap_or_default();
    (api.setfield)(l, -2, key.as_ptr() as *const c_char);
}

unsafe fn push_error(l: *mut LuaState, msg: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    (api.createtable)(l, 0, 3);
    set_bool(l, "ok", false);
    set_str(l, "kind", "error");
    set_list(l, "errors", &[msg.to_string()]);
    1
}

/// Fills the result table at the top of the stack (called while the variant is alive).
unsafe fn push_outcome(f: &Fns, l: *mut LuaState, out: Outcome, errs: &[String], want_names: bool) {
    let Some(api) = lua::api() else { return };
    match out {
        Outcome::Value(v, tag) => {
            set_bool(l, "ok", errs.is_empty());
            set_str(l, "kind", if errs.is_empty() { "value" } else { "error" });
            set_str(l, "value", &v);
            set_int(l, "tag", tag as i64);
        }
        Outcome::Context(c) => {
            set_bool(l, "ok", c != 0);
            set_str(l, "kind", "context");
            if c != 0 {
                let (ty, id) = ctx_type_id(f, c);
                set_str(l, "type", &ty);
                set_str(l, "id", &id);
                if want_names { if let Some(n) = ctx_name(f, c) { set_str(l, "name", &n); } }
            }
        }
        Outcome::List(items, count) => {
            set_bool(l, "ok", true);
            set_str(l, "kind", "list");
            set_int(l, "count", count as i64);
            set_bool(l, "truncated", items.len() < count);
            // one compiled "Name" per element; the names are what the viewer shows on each row
            let mut name_cache: HashMap<usize, bool> = HashMap::new();
            (api.createtable)(l, items.len() as c_int, 0);
            for (i, c) in items.iter().enumerate() {
                (api.createtable)(l, 0, 3);
                let (ty, id) = ctx_type_id(f, *c);
                set_str(l, "type", &ty);
                set_str(l, "id", &id);
                let vt = rq(*c);
                if want_names && *name_cache.get(&vt).unwrap_or(&true) {
                    match ctx_name(f, *c) {
                        Some(n) => set_str(l, "name", &n),
                        None => { name_cache.insert(vt, false); }
                    }
                }
                (api.rawseti)(l, -2, (i + 1) as c_int);
            }
            (api.setfield)(l, -2, b"rows\0".as_ptr() as *const c_char);
        }
        Outcome::Command => {
            set_bool(l, "ok", true);
            set_str(l, "kind", "command");
            set_str(l, "note", "the expression is a command; it was not run");
        }
        Outcome::Void(tag) => {
            set_bool(l, "ok", errs.is_empty());
            set_str(l, "kind", if errs.is_empty() { "void" } else { "error" });
            set_int(l, "tag", tag as i64);
        }
    }
}

/// se_cv_eval(expr, max_rows, want_names) -> table (see the module comment)
unsafe extern "C" fn se_cv_eval(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let Some(f) = FNS.get() else { return push_error(l, "not installed") };
    if !enabled() {
        return push_error(l, "disabled: set dev_cv_eval=1 in script_extender.cfg (developer tool)");
    }
    if !model_ready(f) { return push_error(l, "no campaign model (campaign only)"); }
    let expr = lua::to_str(l, 1);
    let expr = expr.trim();
    if expr.is_empty() || expr.len() > 4000 { return push_error(l, "expression empty or longer than 4000 bytes"); }
    if expr.starts_with('~') { return push_error(l, "console commands (~) are not evaluated"); }
    let max_rows = { let n = (api.tointeger)(l, 2); if n <= 0 { 500 } else { (n as usize).min(5000) } };
    let want_names = (api.type_)(l, 3) <= 0 || (api.toboolean)(l, 3) != 0;

    // fresh CcoCampaignRoot, as the viewer's "Campaign Root" shortcut builds it
    let mut root: usize = f.root_vtable;
    let root_p = &mut root as *mut usize as usize;
    // the viewer's own command test (root vfn 0x88 with the source text): never evaluate those
    let mut src = CaStr::new(f, expr);
    let is_cmd: unsafe extern "C" fn(P, P) -> u8 = core::mem::transmute(vfn(root_p, 0x88));
    let cmd = is_cmd(root_p as P, src.p()) != 0;
    src.free(f);
    if cmd {
        return push_error(l, "command expressions are not evaluated (read-only)");
    }
    let mut errs = Vec::new();
    (api.createtable)(l, 0, 8);
    set_str(l, "expr", expr);
    let evaluated = eval_on(f, root_p, expr, &mut errs, max_rows, &mut |out, errs| {
        push_outcome(f, l, out, errs, want_names);
    });
    if !evaluated {
        set_bool(l, "ok", false);
        set_str(l, "kind", "error");
    }
    if !errs.is_empty() { set_list(l, "errors", &errs); }
    1
}
