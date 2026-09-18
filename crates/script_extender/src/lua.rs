//! Lua 5.1 C API bindings resolved from the address table. Windows x64 has one calling
//! convention, so `extern "C"` fn pointers match the engine's functions directly.

use crate::addrs::Table;
use crate::log;
use core::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

pub type LuaState = c_void;
pub type CFunction = unsafe extern "C" fn(*mut LuaState) -> c_int;

pub const LUA_GLOBALSINDEX: c_int = -10002;
pub const LUA_TLIGHTUSERDATA: c_int = 2;
pub const LUA_TUSERDATA: c_int = 7;

#[allow(dead_code)]
pub struct Api {
    pub gettop: unsafe extern "C" fn(*mut LuaState) -> c_int,
    pub settop: unsafe extern "C" fn(*mut LuaState, c_int),
    pub pushcclosure: unsafe extern "C" fn(*mut LuaState, CFunction, c_int),
    pub setfield: unsafe extern "C" fn(*mut LuaState, c_int, *const c_char),
    pub getfield: unsafe extern "C" fn(*mut LuaState, c_int, *const c_char),
    pub pushstring: unsafe extern "C" fn(*mut LuaState, *const c_char),
    pub pushlstring: unsafe extern "C" fn(*mut LuaState, *const c_char, usize),
    pub pushboolean: unsafe extern "C" fn(*mut LuaState, c_int),
    pub pushinteger: unsafe extern "C" fn(*mut LuaState, isize),
    // The game's lua_Number is a 32-bit float (lua_pushnumber does `movss [rax], xmm1`).
    pub pushnumber: unsafe extern "C" fn(*mut LuaState, f32),
    pub pushnil: unsafe extern "C" fn(*mut LuaState),
    pub pushvalue: unsafe extern "C" fn(*mut LuaState, c_int),
    pub tointeger: unsafe extern "C" fn(*mut LuaState, c_int) -> isize,
    pub tonumber: unsafe extern "C" fn(*mut LuaState, c_int) -> f32,
    pub toboolean: unsafe extern "C" fn(*mut LuaState, c_int) -> c_int,
    pub touserdata: unsafe extern "C" fn(*mut LuaState, c_int) -> *mut c_void,
    pub topointer: unsafe extern "C" fn(*mut LuaState, c_int) -> *const c_void,
    pub tolstring: unsafe extern "C" fn(*mut LuaState, c_int, *mut usize) -> *const c_char,
    pub type_: unsafe extern "C" fn(*mut LuaState, c_int) -> c_int,
    pub createtable: unsafe extern "C" fn(*mut LuaState, c_int, c_int),
    pub rawseti: unsafe extern "C" fn(*mut LuaState, c_int, c_int),
    pub loadbuffer: unsafe extern "C" fn(*mut LuaState, *const c_char, usize, *const c_char) -> c_int,
    pub pcall: unsafe extern "C" fn(*mut LuaState, c_int, c_int, c_int) -> c_int,
}

static API: OnceLock<Api> = OnceLock::new();

pub fn api() -> Option<&'static Api> {
    API.get()
}

pub fn init(t: &Table) {
    // SAFETY: every address was verified against anchor bytes for this exact build.
    let api = unsafe {
        Api {
            gettop: core::mem::transmute(t.get("lua_gettop")),
            settop: core::mem::transmute(t.get("lua_settop")),
            pushcclosure: core::mem::transmute(t.get("lua_pushcclosure")),
            setfield: core::mem::transmute(t.get("lua_setfield")),
            getfield: core::mem::transmute(t.get("lua_getfield")),
            pushstring: core::mem::transmute(t.get("lua_pushstring")),
            pushlstring: core::mem::transmute(t.get("lua_pushlstring")),
            pushboolean: core::mem::transmute(t.get("lua_pushboolean")),
            pushinteger: core::mem::transmute(t.get("lua_pushinteger")),
            pushnumber: core::mem::transmute(t.get("lua_pushnumber")),
            pushnil: core::mem::transmute(t.get("lua_pushnil")),
            pushvalue: core::mem::transmute(t.get("lua_pushvalue")),
            tointeger: core::mem::transmute(t.get("lua_tointeger")),
            tonumber: core::mem::transmute(t.get("lua_tonumber")),
            toboolean: core::mem::transmute(t.get("lua_toboolean")),
            touserdata: core::mem::transmute(t.get("lua_touserdata")),
            topointer: core::mem::transmute(t.get("lua_topointer")),
            tolstring: core::mem::transmute(t.get("lua_tolstring")),
            type_: core::mem::transmute(t.get("lua_type")),
            createtable: core::mem::transmute(t.get("lua_createtable")),
            rawseti: core::mem::transmute(t.get("lua_rawseti")),
            loadbuffer: core::mem::transmute(t.get("luaL_loadbuffer")),
            pcall: core::mem::transmute(t.get("lua_pcall")),
        }
    };
    let _ = API.set(api);
}

/// Push a Rust string as a Lua string (length-delimited, no NUL needed).
pub unsafe fn push_str(l: *mut LuaState, s: &str) {
    if let Some(api) = api() {
        (api.pushlstring)(l, s.as_ptr() as *const c_char, s.len());
    }
}

/// Copy of the string at a stack index ("" when it is not a string).
pub unsafe fn to_str(l: *mut LuaState, idx: c_int) -> String {
    let Some(api) = api() else { return String::new() };
    let mut len: usize = 0;
    let p = (api.tolstring)(l, idx, &mut len);
    if p.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(core::slice::from_raw_parts(p as *const u8, len)).into_owned()
}

/// Register a global C function by name.
pub unsafe fn set_global_fn(l: *mut LuaState, name: &str, f: CFunction) {
    if let Some(api) = api() {
        let mut v = name.as_bytes().to_vec();
        v.push(0);
        (api.pushcclosure)(l, f, 0);
        (api.setfield)(l, LUA_GLOBALSINDEX, v.as_ptr() as *const c_char);
    }
}

/// Set a string field on the table at the top of the stack.
#[allow(dead_code)]
pub unsafe fn set_field_str(l: *mut LuaState, name: &str, value: &str) {
    if let Some(api) = api() {
        let mut v = name.as_bytes().to_vec();
        v.push(0);
        push_str(l, value);
        (api.setfield)(l, -2, v.as_ptr() as *const c_char);
    }
}

/// Set a number field on the table at the top of the stack.
#[allow(dead_code)]
pub unsafe fn set_field_num(l: *mut LuaState, name: &str, value: f64) {
    if let Some(api) = api() {
        let mut v = name.as_bytes().to_vec();
        v.push(0);
        (api.pushnumber)(l, value as f32);
        (api.setfield)(l, -2, v.as_ptr() as *const c_char);
    }
}

/// Compile and run a Lua chunk in the given state, leaving the stack as it was.
/// Errors (syntax or runtime) are returned as text, never raised into the engine.
pub unsafe fn run_chunk(l: *mut LuaState, name: &str, src: &str) -> Result<(), String> {
    let api = api().ok_or("lua api missing")?;
    let top = (api.gettop)(l);
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    let st = (api.loadbuffer)(l, src.as_ptr() as *const c_char, src.len(), cname.as_ptr() as *const c_char);
    if st != 0 {
        let msg = to_str(l, -1);
        (api.settop)(l, top);
        return Err(format!("load failed ({st}): {msg}"));
    }
    let st = (api.pcall)(l, 0, 0, 0);
    if st != 0 {
        let msg = to_str(l, -1);
        (api.settop)(l, top);
        return Err(format!("run failed ({st}): {msg}"));
    }
    (api.settop)(l, top);
    log!("ran chunk '{name}' ({} bytes)", src.len());
    Ok(())
}
