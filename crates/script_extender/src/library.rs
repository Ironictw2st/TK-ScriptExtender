//! Creator v2 Phase 1 (CreatorUI phase US): the Characters / Factions libraries.
//!
//! The 3K frontend Lua cannot write files, so saved warlords and factions live as one small text
//! file per entry (the hand-off key=value format; the Lua side owns the format) in TWO fixed folders:
//!   %APPDATA%\The Creative Assembly\ThreeKingdoms\190E_characters\<id>.txt
//!   %APPDATA%\The Creative Assembly\ThreeKingdoms\190E_factions\<id>.txt
//! Natives (same safety rules as portraits.rs: a kind + an id, never a path):
//!   se_library_save(kind, id, text) -> true | nil, msg      (atomic: tmp file + rename)
//!   se_library_list(kind)           -> "id1|id2|..." (sorted) | nil, msg
//!   se_library_read(kind, id)       -> text | nil, msg
//!   se_library_delete(kind, id)     -> true | nil, msg      (the file moves to <folder>\.deleted\, kept
//!                                                            for undo; the newest 50 are kept)
//! kind = "characters" | "factions"; id = 1..64 chars [A-Za-z0-9_-]; text <= 256 KB, UTF-8.

use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::c_int;
use std::path::PathBuf;

const MAX_TEXT: usize = 256 * 1024;
const MAX_ENTRIES: usize = 2000;
const KEEP_DELETED: usize = 50;

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_library_save", se_library_save);
    lua::set_global_fn(l, "se_library_list", se_library_list);
    lua::set_global_fn(l, "se_library_read", se_library_read);
    lua::set_global_fn(l, "se_library_delete", se_library_delete);
}

fn tw_root() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("The Creative Assembly").join("ThreeKingdoms"))
}

fn folder(kind: &str) -> Result<PathBuf, String> {
    let sub = match kind {
        "characters" => "190E_characters",
        "factions" => "190E_factions",
        _ => return Err("kind must be \"characters\" or \"factions\"".into()),
    };
    let d = tw_root().ok_or("APPDATA not set")?.join(sub);
    std::fs::create_dir_all(&d).map_err(|e| format!("cannot create {sub}: {e}"))?;
    Ok(d)
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn entry(kind: &str, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) { return Err("id must be 1-64 characters [A-Za-z0-9_-]".into()); }
    Ok(folder(kind)?.join(format!("{id}.txt")))
}

pub fn save(kind: &str, id: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_TEXT { return Err(format!("entry is larger than {} KB", MAX_TEXT / 1024)); }
    let p = entry(kind, id)?;
    if !p.exists() && list(kind)?.len() >= MAX_ENTRIES { return Err(format!("the {kind} library is full ({MAX_ENTRIES})")); }
    let tmp = p.with_extension("txt.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("write {id}: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("save {id}: {e}") })
}

pub fn list(kind: &str) -> Result<Vec<String>, String> {
    let d = folder(kind)?;
    let mut ids = Vec::new();
    for e in std::fs::read_dir(&d).map_err(|e| format!("cannot read the {kind} library: {e}"))?.flatten() {
        let Ok(n) = e.file_name().into_string() else { continue };
        let Some(id) = n.strip_suffix(".txt") else { continue };
        if !valid_id(id) { continue; }
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() || md.len() as usize > MAX_TEXT { continue; }
        ids.push(id.to_string());
        if ids.len() >= MAX_ENTRIES { break; }
    }
    ids.sort();
    Ok(ids)
}

pub fn read(kind: &str, id: &str) -> Result<String, String> {
    let p = entry(kind, id)?;
    let md = std::fs::symlink_metadata(&p).map_err(|_| format!("{id} is not in the {kind} library"))?;
    if !md.is_file() { return Err(format!("{id} is not a regular file")); }
    if md.len() as usize > MAX_TEXT { return Err(format!("{id} is larger than {} KB", MAX_TEXT / 1024)); }
    let b = std::fs::read(&p).map_err(|e| format!("read {id}: {e}"))?;
    String::from_utf8(b).map_err(|_| format!("{id} is not UTF-8 text"))
}

pub fn delete(kind: &str, id: &str) -> Result<(), String> {
    let p = entry(kind, id)?;
    if !p.is_file() { return Err(format!("{id} is not in the {kind} library")); }
    let bin = folder(kind)?.join(".deleted");
    std::fs::create_dir_all(&bin).map_err(|e| format!("cannot create .deleted: {e}"))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::rename(&p, bin.join(format!("{id}.{stamp}.txt"))).map_err(|e| format!("delete {id}: {e}"))?;
    // keep the newest KEEP_DELETED
    if let Ok(rd) = std::fs::read_dir(&bin) {
        let mut v: Vec<(std::time::SystemTime, PathBuf)> = rd.flatten()
            .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.path()))).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, old) in v.into_iter().skip(KEEP_DELETED) { let _ = std::fs::remove_file(old); }
    }
    Ok(())
}

unsafe fn finish(l: *mut LuaState, name: &str, r: Result<Option<String>, String>) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    match r {
        Ok(Some(s)) => { lua::push_str(l, &s); 1 }
        Ok(None) => { (api.pushboolean)(l, 1); 1 }
        Err(e) => { log!("{name}: {e}"); (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

unsafe extern "C" fn se_library_save(l: *mut LuaState) -> c_int {
    let (k, id, t) = (lua::to_str(l, 1), lua::to_str(l, 2), lua::to_str(l, 3));
    let r = save(&k, &id, &t).map(|_| None);
    if r.is_ok() { log!("se_library_save: {k}/{id} ({} bytes)", t.len()); }
    finish(l, "se_library_save", r)
}

unsafe extern "C" fn se_library_list(l: *mut LuaState) -> c_int {
    let k = lua::to_str(l, 1);
    finish(l, "se_library_list", list(&k).map(|v| Some(v.join("|"))))
}

unsafe extern "C" fn se_library_read(l: *mut LuaState) -> c_int {
    let (k, id) = (lua::to_str(l, 1), lua::to_str(l, 2));
    finish(l, "se_library_read", read(&k, &id).map(Some))
}

unsafe extern "C" fn se_library_delete(l: *mut LuaState) -> c_int {
    let (k, id) = (lua::to_str(l, 1), lua::to_str(l, 2));
    let r = delete(&k, &id).map(|_| None);
    if r.is_ok() { log!("se_library_delete: {k}/{id} (kept in .deleted)"); }
    finish(l, "se_library_delete", r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tmp_appdata<F: FnOnce()>(f: F) {
        let d = std::env::temp_dir().join(format!("se_lib_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let old = std::env::var_os("APPDATA");
        std::env::set_var("APPDATA", &d);
        f();
        match old { Some(v) => std::env::set_var("APPDATA", v), None => std::env::remove_var("APPDATA") }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn round_trip_and_rules() {
        with_tmp_appdata(|| {
            assert!(valid_id("zhao_yun-1") && !valid_id("../x") && !valid_id("") && !valid_id("a b"));
            save("characters", "zhao_yun", "forename=Yun\nsurname=Zhao\n").unwrap();
            save("factions", "kingdom_of_zhao", "fname=Kingdom of Zhao\n").unwrap();
            assert_eq!(list("characters").unwrap(), vec!["zhao_yun".to_string()]);
            assert_eq!(read("characters", "zhao_yun").unwrap(), "forename=Yun\nsurname=Zhao\n");
            save("characters", "zhao_yun", "forename=Zilong\n").unwrap();     // overwrite in place
            assert_eq!(read("characters", "zhao_yun").unwrap(), "forename=Zilong\n");
            assert!(save("weapons", "x", "").is_err());
            assert!(save("characters", "x/../y", "").is_err());
            assert!(save("characters", "big", &"a".repeat(MAX_TEXT + 1)).is_err());
            delete("characters", "zhao_yun").unwrap();
            assert!(list("characters").unwrap().is_empty());
            assert!(read("characters", "zhao_yun").is_err());
            assert!(delete("characters", "zhao_yun").is_err());
            assert_eq!(list("factions").unwrap().len(), 1);
        });
    }
}
