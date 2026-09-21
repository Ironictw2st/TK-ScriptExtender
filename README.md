# TK Script Extender

A native DLL for **Total War: THREE KINGDOMS** that is injected into the running game and
registers extra `se.*` functions into the campaign Lua VM: move characters between factions'
recruitment pools, recruit/replace/disband retinue units, edit experience, skill points, faction
potential, CAI personality, steer auto-resolve and the campaign AI's recruitment, and more.
Multiplayer campaigns are supported since 0.30, under a build-string version lock that keeps
mismatched setups out of the same lobby.

- **Writing scripts against it: [`docs/SCRIPTING.md`](docs/SCRIPTING.md)** — the full `se.*`
  reference, recipes and troubleshooting.
- Integrating or maintaining the DLL: `HANDOFF.md` (injection rules, engine facts, version
  history).

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
the DLL after launch. A full release (a tag without a hyphen) is also zipped and uploaded to the
[Nexus Mods page](https://www.nexusmods.com/totalwarthreekingdoms/mods/249) (shared with TK Mod
Manager) by `.github/workflows/nexus.yml`; run that workflow by hand to re-send a tag.

## Crates

- `crates/script_extender` — the DLL (`lua_gettop` trampoline hook, `se_*` natives, embedded `lua/se_api.lua`).
- `crates/inject_core` — zero-dependency Win32 helpers: spawn the game, wait for its window, `LoadLibrary` injection.
- `crates/injector` — CLI over `inject_core`.
