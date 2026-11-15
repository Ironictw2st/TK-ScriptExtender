//! Creator S6: player-supplied portraits (notes/custom_portraits.md).
//!
//! Players drop PNG files into ONE fixed folder:
//!   %APPDATA%\The Creative Assembly\ThreeKingdoms\190E_portraits\
//! `se_custom_portraits_list()` lists them (sanitised names, PNG magic, size cap, no subfolders).
//! `se_custom_portrait_install(file)` decodes one file and writes the 16 images of a 3K character
//! art folder, centre-cropped and scaled, as loose files under the user maps folder (a VFS search
//! root of the game), in a slot the SE allocates (manifest; see `load_manifest`):
//!   %APPDATA%\The Creative Assembly\ThreeKingdoms\maps\ui\characters\190e_custom_portrait_<slot>\
//! The game then serves `ui/characters/190e_custom_portrait_<slot>/...` from there. No other paths
//! are read or written; the natives take a file name, never a path.

use crate::log;
use crate::lua::{self, LuaState};
use core::ffi::c_int;
use std::path::PathBuf;

const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_SLOT: i64 = 64;

pub unsafe fn register(l: *mut LuaState) {
    lua::set_global_fn(l, "se_custom_portraits_list", se_custom_portraits_list);
    lua::set_global_fn(l, "se_custom_portrait_install", se_custom_portrait_install);
}

fn tw_root() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("The Creative Assembly").join("ThreeKingdoms"))
}

fn drop_folder() -> Option<PathBuf> { tw_root().map(|r| r.join("190E_portraits")) }

/// File names we accept: 1..64 chars of [A-Za-z0-9 _-.], ending in .png, no leading dot.
fn valid_name(n: &str) -> bool {
    n.len() >= 5 && n.len() <= 64 && !n.starts_with('.')
        && n.to_ascii_lowercase().ends_with(".png")
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'_' || b == b'-' || b == b'.')
        && !n.contains("..")
}

fn checked_file(name: &str) -> Result<PathBuf, String> {
    if !valid_name(name) { return Err("file name must be 1-64 characters [A-Za-z0-9 _-.] ending in .png".into()); }
    let p = drop_folder().ok_or("APPDATA not set")?.join(name);
    let md = std::fs::symlink_metadata(&p).map_err(|_| format!("{name} not found in the 190E_portraits folder"))?;
    if !md.is_file() { return Err(format!("{name} is not a regular file")); }
    if md.len() > MAX_FILE || md.len() < 8 { return Err(format!("{name} is larger than 16 MB or empty")); }
    Ok(p)
}

/// se_custom_portraits_list() -> "a.png|b.png|..." (folder created when missing) | nil, msg
unsafe extern "C" fn se_custom_portraits_list(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let out = (|| -> Result<String, String> {
        let dir = drop_folder().ok_or("APPDATA not set")?;
        let _ = std::fs::create_dir_all(&dir);
        let mut names = Vec::new();
        for e in std::fs::read_dir(&dir).map_err(|e| format!("cannot read the folder: {e}"))?.flatten() {
            let Ok(n) = e.file_name().into_string() else { continue };
            if !valid_name(&n) { continue; }
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() || md.len() > MAX_FILE || md.len() < 8 { continue; }
            let mut magic = [0u8; 8];
            if let Ok(mut f) = std::fs::File::open(e.path()) {
                use std::io::Read;
                if f.read_exact(&mut magic).is_err() || magic != *b"\x89PNG\r\n\x1a\n" { continue; }
            } else { continue; }
            names.push(n);
            if names.len() >= 200 { break; }
        }
        names.sort();
        Ok(names.join("|"))
    })();
    match out {
        Ok(s) => { lua::push_str(l, &s); 1 }
        Err(e) => { (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

struct Rgba { w: u32, h: u32, px: Vec<u8> }

fn decode(path: &PathBuf) -> Result<Rgba, String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut dec = png::Decoder::new(f);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().map_err(|e| format!("not a readable PNG: {e}"))?;
    let (w, h) = (r.info().width, r.info().height);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE { return Err(format!("image is {w}x{h}; allowed up to {MAX_SIDE}x{MAX_SIDE}")); }
    let mut buf = vec![0u8; r.output_buffer_size()];
    let fr = r.next_frame(&mut buf).map_err(|e| format!("PNG decode failed: {e}"))?;
    let (ct, _) = r.output_color_type();
    let n = (w * h) as usize;
    let mut px = vec![0u8; n * 4];
    let src = &buf[..fr.buffer_size()];
    match ct {
        png::ColorType::Rgba => px.copy_from_slice(&src[..n * 4]),
        png::ColorType::Rgb => for i in 0..n { px[i * 4..i * 4 + 3].copy_from_slice(&src[i * 3..i * 3 + 3]); px[i * 4 + 3] = 255; },
        png::ColorType::GrayscaleAlpha => for i in 0..n { let g = src[i * 2]; px[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, src[i * 2 + 1]]); },
        png::ColorType::Grayscale => for i in 0..n { let g = src[i]; px[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 255]); },
        _ => return Err("unsupported PNG colour type".into()),
    }
    Ok(Rgba { w, h, px })
}

/// Centre-crop to the target aspect (anchored a little above centre, where faces usually are),
/// then area-average down / bilinear up to tw x th.
fn fit(src: &Rgba, tw: u32, th: u32) -> Vec<u8> {
    let (sw, sh) = (src.w as f64, src.h as f64);
    let ta = tw as f64 / th as f64;
    let (cw, ch) = if sw / sh > ta { (sh * ta, sh) } else { (sw, sw / ta) };
    let cx = (sw - cw) / 2.0;
    let cy = ((sh - ch) * 0.35).max(0.0);
    let mut out = vec![0u8; (tw * th * 4) as usize];
    let sx = cw / tw as f64;
    let sy = ch / th as f64;
    for y in 0..th {
        for x in 0..tw {
            let x0 = cx + x as f64 * sx;
            let y0 = cy + y as f64 * sy;
            let (x1, y1) = (x0 + sx.max(1.0), y0 + sy.max(1.0));
            let mut acc = [0f64; 4];
            let mut cnt = 0f64;
            let mut yy = y0.floor() as i64;
            while (yy as f64) < y1 {
                let mut xx = x0.floor() as i64;
                while (xx as f64) < x1 {
                    let px = xx.clamp(0, src.w as i64 - 1) as usize;
                    let py = yy.clamp(0, src.h as i64 - 1) as usize;
                    let i = (py * src.w as usize + px) * 4;
                    for c in 0..4 { acc[c] += src.px[i + c] as f64; }
                    cnt += 1.0;
                    xx += 1;
                }
                yy += 1;
            }
            let o = ((y * tw + x) * 4) as usize;
            for c in 0..4 { out[o + c] = (acc[c] / cnt.max(1.0)).round() as u8; }
        }
    }
    out
}

fn write_png(path: &PathBuf, w: u32, h: u32, px: &[u8]) -> Result<(), String> {
    if let Some(d) = path.parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
    let f = std::fs::File::create(path).map_err(|e| format!("write {}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(px).map_err(|e| e.to_string())
}

/// The 3K character art layout (sizes from the shipped art, e.g. 3k_dlc05_hero_special_metal_lu_ji).
const STILLS: &[(&str, u32, u32, u32, u32)] = &[
    // (subfolder, w, h, large w, large h)
    ("halfbody_large", 312, 250, 499, 400),
    ("halfbody_small", 110, 84, 176, 134),
    ("mini", 30, 30, 48, 48),
    ("bobbleheads", 84, 138, 134, 220),
    ("unitcards", 82, 272, 131, 435),
];

// ---- slot allocation (the SE owns it; a used slot is never overwritten) ----------------------
// Manifest: <maps>\190e_custom_portrait_manifest.txt, one line per slot: "slot<TAB>hash<TAB>file".
// hash = FNV-1a 64 of the source file bytes (identity, not security). A slot folder that exists on
// disk without a manifest line (manifest lost or edited by hand) counts as used with an unknown hash.

fn chars_dir() -> Option<PathBuf> { tw_root().map(|r| r.join("maps").join("ui").join("characters")) }
fn manifest_path() -> Option<PathBuf> { tw_root().map(|r| r.join("maps").join("190e_custom_portrait_manifest.txt")) }
fn slot_key(slot: i64) -> String { format!("190e_custom_portrait_{slot}") }

fn fnv64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b { h ^= *x as u64; h = h.wrapping_mul(0x100000001b3); }
    h
}

/// slot -> (hash or None when unknown, file name)
fn load_manifest() -> std::collections::BTreeMap<i64, (Option<u64>, String)> {
    let mut m = std::collections::BTreeMap::new();
    if let Some(p) = manifest_path() {
        if let Ok(t) = std::fs::read_to_string(&p) {
            for line in t.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() != 3 { continue; }
                let Ok(slot) = f[0].trim().parse::<i64>() else { continue };
                if !(1..=MAX_SLOT).contains(&slot) { continue; }
                let hash = u64::from_str_radix(f[1].trim(), 16).ok();
                let name = if valid_name(f[2].trim()) { f[2].trim().to_string() } else { String::new() };
                m.insert(slot, (hash, name));
            }
        }
    }
    // folders on disk that the manifest does not know: used, hash unknown
    if let Some(dir) = chars_dir() {
        for slot in 1..=MAX_SLOT {
            if !m.contains_key(&slot) && dir.join(slot_key(slot)).exists() {
                m.insert(slot, (None, String::new()));
            }
        }
    }
    m
}

fn save_manifest(m: &std::collections::BTreeMap<i64, (Option<u64>, String)>) -> Result<(), String> {
    let p = manifest_path().ok_or("APPDATA not set")?;
    if let Some(d) = p.parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
    let mut t = String::from("");
    for (slot, (hash, name)) in m {
        t.push_str(&format!("{slot}\t{}\t{name}\n", hash.map(|h| format!("{h:016x}")).unwrap_or_else(|| "?".into())));
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, t).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// se_custom_portrait_install(file_name) -> art_key, slot | nil, msg
/// Same picture (same bytes) as an installed slot -> that slot; otherwise a never-used slot.
unsafe extern "C" fn se_custom_portrait_install(l: *mut LuaState) -> c_int {
    let Some(api) = lua::api() else { return 0 };
    let name = lua::to_str(l, 1);
    let out = (|| -> Result<(String, i64, bool), String> {
        let file = checked_file(&name)?;
        let bytes = std::fs::read(&file).map_err(|e| format!("read {name}: {e}"))?;
        if bytes.len() as u64 > MAX_FILE { return Err(format!("{name} is larger than 16 MB")); }
        let hash = fnv64(&bytes);
        let mut man = load_manifest();
        let chars = chars_dir().ok_or("APPDATA not set")?;
        if let Some((slot, _)) = man.iter().find(|(_, (h, _))| *h == Some(hash)) {
            let slot = *slot;
            let probe = chars.join(slot_key(slot)).join("stills").join("halfbody_large").join(format!("{}.png", slot_key(slot)));
            if probe.exists() {
                return Ok((slot_key(slot), slot, true));
            }
            // the manifest knows the picture but its files are gone: rebuild them in the same slot
            install_into(&file, slot)?;
            return Ok((slot_key(slot), slot, false));
        }
        let Some(slot) = (1..=MAX_SLOT).find(|s| !man.contains_key(s) && !chars.join(slot_key(*s)).exists()) else {
            return Err(format!("all {MAX_SLOT} custom portrait slots are in use (campaigns may still show them); none was overwritten"));
        };
        install_into(&file, slot)?;
        man.insert(slot, (Some(hash), name.clone()));
        save_manifest(&man)?;
        Ok((slot_key(slot), slot, false))
    })();
    match out {
        Ok((k, slot, reused)) => {
            log!("se_custom_portrait_install: {name} -> slot {slot}{}", if reused { " (same picture, reused)" } else { "" });
            lua::push_str(l, &k);
            (api.pushinteger)(l, slot as _);
            2
        }
        Err(e) => { log!("se_custom_portrait_install: {e}"); (api.pushnil)(l); lua::push_str(l, &e); 2 }
    }
}

/// Decode `file` and write the 16 images of slot `slot` (only ever called for a free slot, or to
/// rebuild a slot whose manifest hash matches this very picture).
fn install_into(file: &PathBuf, slot: i64) -> Result<(), String> {
    {
        let img = decode(file)?;
        let key = slot_key(slot);
        let base = chars_dir().ok_or("APPDATA not set")?.join(&key);
        let png_name = format!("{key}.png");
        let mut n = 0;
        for (sub, w, h, lw, lh) in STILLS {
            let d = base.join("stills").join(sub);
            write_png(&d.join(&png_name), *w, *h, &fit(&img, *w, *h))?;
            write_png(&d.join("large").join(&png_name), *lw, *lh, &fit(&img, *lw, *lh))?;
            n += 2;
        }
        let comp = fit(&img, 1300, 1300);
        for panel in ["large_panel", "small_panel"] {
            for mood in ["norm", "happy", "angry"] {
                write_png(&base.join("composites").join(panel).join(mood).join(format!("{mood}.png")), 1300, 1300, &comp)?;
                n += 1;
            }
        }
        log!("custom portrait: {} ({}x{}) -> {} ({n} files)", file.display(), img.w, img.h, base.display());
        Ok(())
    }
}
