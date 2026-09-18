# TK Script Extender

A native DLL for **Total War: THREE KINGDOMS** (single-player) that is injected into the running
game and registers extra `se.*` functions into the campaign Lua VM: move characters between
factions' recruitment pools, recruit/replace/disband retinue units, edit experience, skill
points, faction potential, CAI personality, and more. See `HANDOFF.md` for the API and the
engine facts behind it.

The DLL fingerprints the game build (PE `TimeDateStamp` + `SizeOfImage`) and verifies every
engine address before touching anything; on a mismatch it logs and stays inert.

## Building

```
cargo build --release
```
produces `target/release/script_extender.dll` and `injector.exe` (a dev loader:
`injector.exe <dll> [Three_Kingdoms.exe]`, run once the main menu is up).

## Releases

Tag `vX.Y.Z` (matching `Cargo.toml`) and the release workflow publishes `script_extender.dll`,
`injector.exe` and `manifest.json`. [TK Mod Manager](https://github.com/Ironictw2st/TKModManager)
downloads these, checks the manifest's game fingerprint against the installed exe, and injects
the DLL after launch.

## Crates

- `crates/script_extender` — the DLL (`lua_gettop` trampoline hook, `se_*` natives, embedded `lua/se_api.lua`).
- `crates/inject_core` — zero-dependency Win32 helpers: spawn the game, wait for its window, `LoadLibrary` injection.
- `crates/injector` — CLI over `inject_core`.
