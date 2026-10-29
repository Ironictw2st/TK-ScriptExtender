//! Audio: post Wwise events on our own game object, for a Lua-driven jukebox (190E "Soundtrack").
//!
//! The Wwise SDK (2018.1.x, bank v129) is statically linked; notes/audio_wwise.md has the RE and
//! the live verification of every call used here. The module never writes engine memory except
//! through two detours, and it checks its own 16-byte anchors: a mismatch switches only audio off
//! (natives answer "unavailable"), the rest of the DLL is unaffected. cfg `audio=0` turns it off.
//!
//! - Events are posted on game object `OBJ` (registered on first use; default listeners apply).
//! - EndOfEvent is delivered on Wwise's audio thread to `end_callback`, which only queues
//!   (playing id, event id); Lua drains the queue through `se_audio_poll` on its own thread.
//! - `vanilla_music(false)` pauses Global_Music_Play on the engine's global emitter. The game's
//!   music controller (FUN_140794900, the only poster of Global_Music_Play / _Pause / _Resume /
//!   _Stop) is detoured: when a call resumed the music (Global_Music_Resume is a master resume
//!   that clears every pause level) or started a new Global_Music_Play, the pause is applied
//!   again. Our pause is kept at exactly one Wwise pause level.
//! - Wwise SetState is detoured to see Game_Mode -> loading_screen_start: every jukebox track is
//!   stopped (fade), the vanilla pause is released, and a "transition" entry is queued for Lua.
//!
//! Natives (internal; the public face is se.audio in se_api.lua). Event and state arguments are
//! names or "0x"-hex strings: lua_Number is a 32-bit float, so 32-bit ids do not survive as
//! numbers. Playing ids are small sequential integers and are passed as numbers.
//!   se_audio_info() -> available, reason, vanilla_paused, active_count
//!   se_audio_post(event, position) -> playing_id | nil, message
//!   se_audio_action(target, action, fade_ms) -> ok, message   target = playing id or event;
//!                                                              action 0 stop, 1 pause, 2 resume
//!   se_audio_set_state(group, value) -> ok, message            Game_Mode refused
//!   se_audio_position(playing_id) -> ms | nil, message
//!   se_audio_vanilla(on, fade_ms) -> ok, message
//!   se_audio_poll() -> { {id=, event="0x..", reason="end"|"transition"}, ... }
//!                      id 0 = the loading-screen marker itself; a track stopped by a loading
//!                      screen ends with reason "transition"

use crate::addrs::Table;
use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::{c_int, c_void};
use retour::GenericDetour;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Our AkGameObjectID. Engine emitters use their heap address, which is never this low.
const OBJ: u64 = 0x190E_5E00;

const EV_MUSIC_PLAY: u32 = 0xd80b6c61; // Global_Music_Play
const GRP_GAME_MODE: u32 = 0x3fdabd27; // Game_Mode
const ST_LOADING_START: u32 = 0xca3b0143; // loading_screen_start

const AK_END_OF_EVENT: u32 = 0x1;
const AK_ENABLE_GET_SOURCE_PLAY_POSITION: u32 = 0x100000;
const CURVE_LINEAR: u32 = 4;
const AK_SUCCESS: u32 = 1;

// (name, rva, first 16 bytes); bytes read from the 1.7.2.0 exe (Steam and Epic share .text).
const ANCHORS: &[(&str, usize, [u8; 16])] = &[
    ("ak_post_event", 0x2c40f80, [0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x4c]),
    ("ak_execute_action_on_event", 0x2c38f90, [0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x48]),
    ("ak_set_state", 0x2c4a3d0, [0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83, 0xec, 0x20, 0x8b, 0xfa, 0x8b, 0xd9, 0x48, 0x8b]),
    ("ak_register_game_obj", 0x2c44480, [0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xd9, 0x48, 0x83, 0xf9, 0xe0, 0x72, 0x0b, 0xb8]),
    ("ak_get_source_play_position", 0x2c3d5e0, [0x40, 0x53, 0x48, 0x83, 0xec, 0x40, 0x48, 0x8b, 0xda, 0x48, 0x85, 0xd2, 0x75, 0x0b, 0xb8, 0x1f]),
    ("music_controller", 0x794900, [0x48, 0x89, 0x5c, 0x24, 0x18, 0x48, 0x89, 0x6c, 0x24, 0x20, 0x56, 0x57, 0x41, 0x56, 0x48, 0x81]),
];

const RVA_SOUND_PTR: usize = 0x3e31f20; // static pointer to the sound system
const RVA_SOUND_VT: usize = 0x32dbde8; // sound system vtable
const RVA_EMITTER_VT: usize = 0x32e2088; // engine emitter vtable
const OFF_GLOBAL_HANDLE: usize = 0x388; // sound -> handle; handle+0x10 -> global emitter
const OFF_GAME_PAUSED: usize = 0x518; // sound: byte set while the game itself paused the music
const OFF_MUSIC_HANDLE: usize = 0x3c0; // sound: handle of the playing Global_Music_Play

type PostEvent = unsafe extern "C" fn(u32, u64, u32, Option<AkCallback>, *mut c_void, u32, *const c_void, u32) -> u32;
type ExecAction = unsafe extern "C" fn(u32, u32, u64, u32, u32, u32) -> u32;
type SetState = unsafe extern "C" fn(u32, u32) -> u32;
type RegisterObj = unsafe extern "C" fn(u64) -> u32;
type GetPosition = unsafe extern "C" fn(u32, *mut u32, u8) -> u32;
type MusicController = unsafe extern "C" fn(*mut c_void, u8, u8);
type AkCallback = unsafe extern "C" fn(u32, *const AkEventCallbackInfo);

#[repr(C)]
struct AkEventCallbackInfo {
    cookie: *mut c_void,
    game_obj: u64,
    playing_id: u32,
    event_id: u32,
}

struct Fns {
    base: usize,
    post: PostEvent,
    action: ExecAction,
    set_state: SetState,
    register: RegisterObj,
    position: GetPosition,
}

static FNS: OnceLock<Fns> = OnceLock::new();
static REASON: Mutex<String> = Mutex::new(String::new());
static REGISTERED: AtomicBool = AtomicBool::new(false);
static VANILLA_PAUSED: AtomicBool = AtomicBool::new(false);
static TRANSITIONS: AtomicUsize = AtomicUsize::new(0);
/// Jukebox tracks posted and not yet ended: (playing id, event id).
static ACTIVE: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
/// Finished tracks waiting for Lua: (playing id, event id, reason); playing id 0 = transition marker.
static ENDED: Mutex<VecDeque<(u32, u32, u8)>> = Mutex::new(VecDeque::new());
/// Tracks stopped by a loading screen: their EndOfEvent is reported as "transition", not "end",
/// so a panel's auto-next does not start a track during the load.
static TRANSITION_STOPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
const REASON_END: u8 = 0;
const REASON_TRANSITION: u8 = 1;
static MUSIC_HOOK: OnceLock<GenericDetour<MusicController>> = OnceLock::new();
static STATE_HOOK: OnceLock<GenericDetour<SetState>> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn set_reason(s: &str) {
    *lock(&REASON) = s.to_string();
}

extern "system" {
    fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
}

unsafe fn rd_u64(addr: usize) -> Option<u64> {
    if addr < 0x10000 || IsBadReadPtr(addr as *const c_void, 8) != 0 { return None; }
    Some(core::ptr::read_unaligned(addr as *const u64))
}

unsafe fn rd_u8(addr: usize) -> Option<u8> {
    if addr < 0x10000 || IsBadReadPtr(addr as *const c_void, 1) != 0 { return None; }
    Some(core::ptr::read_unaligned(addr as *const u8))
}

/// Wwise AK::SoundEngine::GetIDFromString: FNV-1 32 of the lower-cased name (verified live).
pub fn hash_name(name: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in name.bytes() {
        h = h.wrapping_mul(0x01000193) ^ b.to_ascii_lowercase() as u32;
    }
    h
}

/// "0x1234abcd" -> that id, a plain decimal -> that id, anything else -> hash of the name.
fn parse_id(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() { return None; }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse::<u32>().ok();
    }
    Some(hash_name(t))
}

/// The sound system and its global emitter (the AkGameObjectID the vanilla music plays on),
/// both checked by vtable on every call.
unsafe fn sound_and_emitter(base: usize) -> Option<(usize, u64)> {
    let sound = rd_u64(base + RVA_SOUND_PTR)? as usize;
    if rd_u64(sound)? as usize != base + RVA_SOUND_VT { return None; }
    let handle = rd_u64(sound + OFF_GLOBAL_HANDLE)? as usize;
    let emitter = rd_u64(handle + 0x10)? as usize;
    if rd_u64(emitter)? as usize != base + RVA_EMITTER_VT { return None; }
    Some((sound, emitter as u64))
}

unsafe extern "C" fn end_callback(kind: u32, info: *const AkEventCallbackInfo) {
    // Wwise audio thread: queue only, never touch Lua.
    if kind != AK_END_OF_EVENT || info.is_null() { return; }
    let (pid, eid) = ((*info).playing_id, (*info).event_id);
    lock(&ACTIVE).retain(|(p, _)| *p != pid);
    let reason = {
        let mut ts = lock(&TRANSITION_STOPS);
        let hit = ts.iter().position(|p| *p == pid);
        if let Some(i) = hit { ts.swap_remove(i); REASON_TRANSITION } else { REASON_END }
    };
    let mut q = lock(&ENDED);
    if q.len() < 256 { q.push_back((pid, eid, reason)); }
}

unsafe fn ensure_registered(f: &Fns) -> bool {
    if REGISTERED.load(Ordering::Acquire) { return true; }
    let r = (f.register)(OBJ);
    if r == AK_SUCCESS {
        REGISTERED.store(true, Ordering::Release);
        log!("audio: game object 0x{OBJ:x} registered");
        true
    } else {
        log!("audio: RegisterGameObj(0x{OBJ:x}) returned {r}");
        false
    }
}

unsafe fn pause_vanilla(f: &Fns, fade_ms: u32) -> bool {
    let Some((_, emitter)) = sound_and_emitter(f.base) else { return false };
    (f.action)(EV_MUSIC_PLAY, 1, emitter, fade_ms, CURVE_LINEAR, 0) == AK_SUCCESS
}

unsafe fn resume_vanilla(f: &Fns, fade_ms: u32) -> bool {
    let Some((sound, emitter)) = sound_and_emitter(f.base) else { return false };
    // the game paused the music itself (Global_Music_Pause): its own Resume will follow
    if rd_u8(sound + OFF_GAME_PAUSED).unwrap_or(0) != 0 { return true; }
    (f.action)(EV_MUSIC_PLAY, 2, emitter, fade_ms, CURVE_LINEAR, 0) == AK_SUCCESS
}

unsafe fn stop_all_jukebox(f: &Fns, fade_ms: u32) -> usize {
    let tracks: Vec<(u32, u32)> = lock(&ACTIVE).clone();
    {
        let mut ts = lock(&TRANSITION_STOPS);
        ts.clear();
        ts.extend(tracks.iter().map(|(p, _)| *p));
    }
    for (pid, eid) in &tracks {
        (f.action)(*eid, 0, OBJ, fade_ms, CURVE_LINEAR, *pid);
    }
    tracks.len()
}

/// Wwise pauses stack (each Pause adds a level, an ordinary Resume removes one), and the game's
/// Global_Music_Resume is a master resume that clears every level (measured live 2026-10-13).
/// So our pause is held at exactly one level: it is applied again only when the controller
/// resumed the music (game-paused byte 1 -> 0) or started a new Global_Music_Play (handle
/// replaced). After the game's own Pause nothing is done: its master Resume follows later.
unsafe extern "C" fn music_controller_detour(sound: *mut c_void, play: u8, pause: u8) {
    let Some(h) = MUSIC_HOOK.get() else { return };
    let s = sound as usize;
    let before = (rd_u8(s + OFF_GAME_PAUSED).unwrap_or(0), rd_u64(s + OFF_MUSIC_HANDLE).unwrap_or(0));
    h.call(sound, play, pause);
    if !VANILLA_PAUSED.load(Ordering::Acquire) { return; }
    let after = (rd_u8(s + OFF_GAME_PAUSED).unwrap_or(0), rd_u64(s + OFF_MUSIC_HANDLE).unwrap_or(0));
    let resumed = before.0 != 0 && after.0 == 0;
    let new_play = after.1 != 0 && after.1 != before.1;
    if resumed || new_play {
        if let Some(f) = FNS.get() {
            let _g = crate::crash::enter_quiet("audio_music_controller");
            pause_vanilla(f, 0);
        }
    }
}

unsafe extern "C" fn set_state_detour(group: u32, state: u32) -> u32 {
    let Some(h) = STATE_HOOK.get() else { return 0 };
    if group == GRP_GAME_MODE && state == ST_LOADING_START {
        if let Some(f) = FNS.get() {
            let _g = crate::crash::enter_quiet("audio_loading_screen");
            let n = stop_all_jukebox(f, 500);
            let was_paused = VANILLA_PAUSED.swap(false, Ordering::AcqRel);
            if was_paused { resume_vanilla(f, 0); }
            TRANSITIONS.fetch_add(1, Ordering::Relaxed);
            let mut q = lock(&ENDED);
            if q.len() < 256 { q.push_back((0, 0, REASON_TRANSITION)); }
            drop(q);
            log!("audio: loading screen: stopped {n} jukebox track(s), vanilla pause released={was_paused}");
        }
    }
    h.call(group, state)
}

pub fn install(t: &Table) {
    if crate::build::config_value("audio").as_deref() == Some("0") {
        set_reason("off (script_extender.cfg audio=0)");
        log!("audio: off (audio=0)");
        return;
    }
    let base = t.base();
    for (name, rva, want) in ANCHORS {
        let n = want.len();
        let got = unsafe {
            if IsBadReadPtr((base + rva) as *const c_void, n) != 0 { None } else {
                Some(core::slice::from_raw_parts((base + rva) as *const u8, n).to_vec())
            }
        };
        if got.as_deref() != Some(&want[..]) {
            log!("audio: anchor mismatch for {name} at rva 0x{rva:x}: got {got:02x?}; audio off");
            set_reason(&format!("anchor mismatch: {name}"));
            return;
        }
    }
    let fns = unsafe {
        Fns {
            base,
            post: core::mem::transmute(base + ANCHORS[0].1),
            action: core::mem::transmute(base + ANCHORS[1].1),
            set_state: core::mem::transmute(base + ANCHORS[2].1),
            register: core::mem::transmute(base + ANCHORS[3].1),
            position: core::mem::transmute(base + ANCHORS[4].1),
        }
    };
    let music: MusicController = unsafe { core::mem::transmute(base + ANCHORS[5].1) };
    let set_state = fns.set_state;
    // SAFETY: both prologues are anchor-verified and hold no RIP-relative operand in their first
    // 14 bytes (music controller: two stack stores + pushes; SetState: stack store, push,
    // sub rsp, two register moves; its first rip-relative load is at +0xe).
    unsafe {
        let Ok(d) = GenericDetour::new(music, music_controller_detour) else {
            set_reason("could not create the music controller detour");
            return;
        };
        let _ = MUSIC_HOOK.set(d);
        let Some(d) = MUSIC_HOOK.get() else { return };
        if let Err(e) = crate::freeze::enable_detour("audio_music_controller", "audio", base + ANCHORS[5].1, d) {
            log!("audio: music controller detour failed ({e}); audio off");
            set_reason("music controller detour failed");
            return;
        }
        let Ok(d) = GenericDetour::new(set_state, set_state_detour) else {
            set_reason("could not create the SetState detour");
            return;
        };
        let _ = STATE_HOOK.set(d);
        let Some(d) = STATE_HOOK.get() else { return };
        if let Err(e) = crate::freeze::enable_detour("audio_set_state", "audio", base + ANCHORS[2].1, d) {
            log!("audio: SetState detour failed ({e}); audio off");
            set_reason("SetState detour failed");
            return;
        }
    }
    let _ = FNS.set(fns);
    set_reason("");
    log!("audio: ready (Wwise entry points verified, music controller + SetState detours installed)");
}

pub fn available() -> bool {
    FNS.get().is_some()
}

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_audio_info", se_audio_info);
    lua::set_global_fn(l, "se_audio_post", se_audio_post);
    lua::set_global_fn(l, "se_audio_action", se_audio_action);
    lua::set_global_fn(l, "se_audio_set_state", se_audio_set_state);
    lua::set_global_fn(l, "se_audio_position", se_audio_position);
    lua::set_global_fn(l, "se_audio_vanilla", se_audio_vanilla);
    lua::set_global_fn(l, "se_audio_poll", se_audio_poll);
}

unsafe fn fail(l: *mut LuaState, msg: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    (api.pushnil)(l);
    lua::push_str(l, msg);
    2
}

unsafe fn ok_msg(l: *mut LuaState, ok: bool, msg: &str) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, msg);
    2
}

fn unavailable() -> String {
    let r = lock(&REASON).clone();
    if r.is_empty() { "audio module not ready".to_string() } else { format!("audio unavailable: {r}") }
}

/// se_audio_info() -> available, reason, vanilla_paused, active_count
unsafe extern "C" fn se_audio_info(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let ok = available();
    (api.pushboolean)(l, ok as c_int);
    lua::push_str(l, &if ok { String::new() } else { unavailable() });
    (api.pushboolean)(l, VANILLA_PAUSED.load(Ordering::Acquire) as c_int);
    (api.pushinteger)(l, lock(&ACTIVE).len() as isize);
    4
}

/// se_audio_post(event, position) -> playing_id | nil, message
unsafe extern "C" fn se_audio_post(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let Some(f) = FNS.get() else { return fail(l, &unavailable()) };
    let name = lua::to_str(l, 1);
    let Some(eid) = parse_id(&name) else { return fail(l, "event name or \"0x\" id expected") };
    if name.to_ascii_lowercase().starts_with("game_mode") {
        return fail(l, "Game_Mode events change a global state and are refused");
    }
    if !ensure_registered(f) { return fail(l, "could not register the jukebox game object"); }
    let mut flags = AK_END_OF_EVENT;
    if (api.toboolean)(l, 2) != 0 { flags |= AK_ENABLE_GET_SOURCE_PLAY_POSITION; }
    // No lock is held across PostEvent: with a full command queue it waits for the audio thread,
    // which may be inside end_callback waiting for ACTIVE. A very short event can therefore end
    // before it is listed; then its id is already in ENDED and it is not added. Lock order
    // ACTIVE -> ENDED everywhere.
    let pid = (f.post)(eid, OBJ, flags, Some(end_callback), core::ptr::null_mut(), 0, core::ptr::null(), 0);
    if pid == 0 {
        return fail(l, &format!("event 0x{eid:08x} is not loaded (no bank holds it)"));
    }
    {
        let mut active = lock(&ACTIVE);
        let ended = lock(&ENDED).iter().any(|(p, _, _)| *p == pid);
        if !ended && active.len() < 64 { active.push((pid, eid)); }
    }
    log!("audio: post '{name}' (0x{eid:08x}) -> playing id {pid}");
    (api.pushinteger)(l, pid as isize);
    lua::push_str(l, &format!("0x{eid:08x}"));
    2
}

/// se_audio_action(target, action, fade_ms) -> ok, message
unsafe extern "C" fn se_audio_action(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let Some(f) = FNS.get() else { return ok_msg(l, false, &unavailable()) };
    let action = (api.tointeger)(l, 2);
    if !(0..=2).contains(&action) { return ok_msg(l, false, "action must be 0 (stop), 1 (pause) or 2 (resume)"); }
    let fade = (api.tointeger)(l, 3).clamp(0, 60_000) as u32;
    // number = playing id of a jukebox track; string = every instance of that event on our object
    let (eid, pid) = if (api.type_)(l, 1) == 3 {
        let pid = (api.tointeger)(l, 1) as u32;
        match lock(&ACTIVE).iter().find(|(p, _)| *p == pid) {
            Some((_, e)) => (*e, pid),
            None => return ok_msg(l, false, &format!("playing id {pid} is not an active jukebox track")),
        }
    } else {
        match parse_id(&lua::to_str(l, 1)) {
            Some(e) => (e, 0),
            None => return ok_msg(l, false, "playing id or event expected"),
        }
    };
    let r = (f.action)(eid, action as u32, OBJ, fade, CURVE_LINEAR, pid);
    ok_msg(l, r == AK_SUCCESS, &format!("ExecuteActionOnEvent(0x{eid:08x}, {action}, id {pid}) = {r}"))
}

/// se_audio_set_state(group, value) -> ok, message
unsafe extern "C" fn se_audio_set_state(l: *mut LuaState) -> c_int {
    let Some(f) = FNS.get() else { return ok_msg(l, false, &unavailable()) };
    let (g, v) = (lua::to_str(l, 1), lua::to_str(l, 2));
    let (Some(gid), Some(vid)) = (parse_id(&g), parse_id(&v)) else { return ok_msg(l, false, "group and value expected") };
    if gid == GRP_GAME_MODE { return ok_msg(l, false, "Game_Mode is the engine's own state and is refused"); }
    let r = (f.set_state)(gid, vid);
    ok_msg(l, r == AK_SUCCESS, &format!("SetState(0x{gid:08x}, 0x{vid:08x}) = {r}"))
}

/// se_audio_position(playing_id) -> ms | nil, message
unsafe extern "C" fn se_audio_position(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let Some(f) = FNS.get() else { return fail(l, &unavailable()) };
    let pid = (api.tointeger)(l, 1) as u32;
    if !lock(&ACTIVE).iter().any(|(p, _)| *p == pid) { return fail(l, "not an active jukebox track"); }
    let mut ms: u32 = 0;
    let r = (f.position)(pid, &mut ms, 1);
    if r != AK_SUCCESS { return fail(l, "no position (post with position=true; music containers have none)"); }
    (api.pushinteger)(l, ms as isize);
    1
}

/// se_audio_vanilla(on, fade_ms) -> ok, message
unsafe extern "C" fn se_audio_vanilla(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let Some(f) = FNS.get() else { return ok_msg(l, false, &unavailable()) };
    let on = (api.toboolean)(l, 1) != 0;
    let fade = (api.tointeger)(l, 2).clamp(0, 60_000) as u32;
    if on {
        if !VANILLA_PAUSED.swap(false, Ordering::AcqRel) {
            return ok_msg(l, true, "vanilla music was not paused by the jukebox");
        }
        let ok = resume_vanilla(f, fade);
        ok_msg(l, ok, if ok { "vanilla music resumed" } else { "sound system not found" })
    } else {
        // idempotent: a second pause would add a second Wwise pause level
        if VANILLA_PAUSED.swap(true, Ordering::AcqRel) {
            return ok_msg(l, true, "vanilla music already paused");
        }
        let ok = pause_vanilla(f, fade);
        if !ok { VANILLA_PAUSED.store(false, Ordering::Release); }
        ok_msg(l, ok, if ok { "vanilla music paused (kept paused until vanilla_music(true) or a loading screen)" } else { "sound system not found" })
    }
}

/// se_audio_poll() -> array of { id, event, reason }
unsafe extern "C" fn se_audio_poll(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let items: Vec<(u32, u32, u8)> = lock(&ENDED).drain(..).collect();
    (api.createtable)(l, items.len() as c_int, 0);
    for (i, (pid, eid, reason)) in items.iter().enumerate() {
        (api.createtable)(l, 0, 3);
        (api.pushinteger)(l, *pid as isize);
        (api.setfield)(l, -2, b"id\0".as_ptr() as *const _);
        lua::set_field_str(l, "event", &format!("0x{eid:08x}"));
        lua::set_field_str(l, "reason", if *reason == REASON_TRANSITION { "transition" } else { "end" });
        (api.rawseti)(l, -2, (i + 1) as c_int);
    }
    1
}
