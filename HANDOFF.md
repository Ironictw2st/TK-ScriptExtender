# Script extender DLL: handoff for the mod-manager integration

Written 2026-09-17. Everything below was verified live on Total War: THREE KINGDOMS **1.7.2.0**
(Steam build 25370317) unless marked otherwise. Current DLL: **0.30.0**, `Z:\RE\se_deploy\0.26.2\`
(`script_extender.dll` + `injector.exe`; releases are the `v*` tags on GitHub). Source:
`Z:\Claude\ScriptExtender` (Rust workspace). Deep RE notes: `notes/*.md`; day-to-day rules:
`CLAUDE.md`; **scripting documentation for mod authors: `docs/SCRIPTING.md`**.

## 1. What the DLL is

A native DLL injected into the running `Three_Kingdoms.exe` that:

1. fingerprints the exe (PE `TimeDateStamp 0x69ce4c84`, `SizeOfImage 0x4836000`) and verifies 53
   engine addresses by their first 8 bytes plus 3 vtables by their slot-0 RVA (`src/addrs.rs`);
   **on any mismatch it logs and does nothing** (safe on a patched game);
2. detours `lua_gettop` (a trampoline hook, installed with all other threads frozen) and, the
   first time each Lua state passes through it, registers the `se_*` C natives into that state's
   globals and runs the embedded `lua/se_api.lua`, which defines the public `se.query.*` /
   `se.modify.*` API (`src/hook.rs`);
3. exposes named, high-level operations only (no generic peek/poke/call to Lua).

**Multiplayer is supported from 0.30** under lockstep rules: in multiplayer `se.modify.*` only runs inside model callbacks (never queued), synced logic must not depend on the local machine, and the game build string is always version-locked (`[se <version>.<sync>]`, sync = fingerprint of the simulation-relevant cfg keys) so only identical script extenders can share a lobby (lobby check not yet verified on two machines). The manager must give both players the same DLL version and the same `autoresolve_hooks` / `horde_income*` cfg values.
Saves that used `se.modify.*` carry the results (they are ordinary engine state); the DLL is
not needed to load them, only to keep using the API.

## 2. Injection: what the mod manager must do

`crates/injector` is a thin CLI over `crates/inject_core` (`find_pid`, `inject(pid, dll)`,
`spawn_game(exe, args, cwd)`, `wait_for_main_window(pid, timeout)`). Reuse `inject_core`
directly; the CLI is only for development.

```
injector.exe <path\to\script_extender.dll> [Three_Kingdoms.exe]
  prints "injector: OK, remote HMODULE low = 0x..." and exits 0, or "injector: error: ..." and exits 1
```

Rules that matter for a launcher:

- **Inject once per game process, after the main menu is up** (LoadLibrary via remote thread;
  `wait_for_main_window` is enough). Injecting during the loading screens is untested.
- **Never inject a second copy or a newer DLL into a process that already has one**: the second
  `lua_gettop` hook would stack on the first. Any DLL update means: quit game, start game,
  inject. The DLL keeps its own log next to itself: `<dll dir>\script_extender.log`
  (truncated on each injection; also mirrored to `OutputDebugString`).
- Success line to check in the log: `all 53 addresses and 3 vtables verified` then
  `lua_gettop hook installed`; per-state lines `registered se_* functions into lua_State ...`
  and `se_api.lua loaded into lua_State ...` appear once the game's Lua ticks (main menu state
  first, campaign state after a campaign loads).
- Fingerprint mismatch (`fingerprint mismatch: expected ...` / `N anchor(s) failed; refusing to
  run`) = the game was updated; the DLL is inert. The manager should surface this rather than
  retry. Fixing it is an RE job: re-derive the RVAs in `src/addrs.rs` (see section 7).
- The DLL must stay on disk while the game runs (it is a loaded module). Do not overwrite the
  file of a running game; deploy new versions to a new folder (`Z:\RE\se_deploy\<version>\`).
- Do not run the game under a debugger with default x64dbg settings; not a manager concern,
  but documented in `CLAUDE.md`.
- Launching the exe directly (cwd = game root, Steam running) skips the CA launcher; mods then
  need a `mod_list.txt`-style argument (see `spawn_game`). The in-game Lua console mod
  (`ironic_lua_console`) is only a development aid; the API does not depend on it.

## 3. Build

```
cd Z:\Claude\ScriptExtender
cargo build --release          # target\release\script_extender.dll, injector.exe
```
Workspace version in `Cargo.toml` (`se.version()` returns it). Crates: `script_extender`
(cdylib, `retour` for the hook), `injector` (bin), `inject_core` (lib, Win32 only).
Release profile: LTO thin, `panic = "abort"`, stripped. Rust stable, no other deps.

## 4. The public Lua API (embedded `lua/se_api.lua`)

Every `se.modify.*` call returns `ok, message`; when called outside the model thread it queues
itself through `cm:wait_for_model_sp` and returns `true, "... queued ..."`, with the real result
written to the log (`se.logger`). Every `se.query.*` returns a value/table or `nil, message`.
Two globals the scripts must hand over because the chunk's environment cannot see them:
`se.logger = ModLog` (or any `function(string)`) and `se.core = core` (only for
`emperor_policy`). Check `type(se) == "table"` before use; `se.available("se_x")` tells whether
a native is present. `se.version()` = DLL version.

| Area | Query | Modify | Engine path (verified) |
|---|---|---|---|
| Characters / pools | `character(cqi)`, `pool_lock(cqi)` | `move_character(cqi, faction, "pool"/"recruited")`, `release_to_pool(cqi)`, `pool_lock(cqi, turns)`, `pool_unlock(cqi)` | stock `move_to_faction` + `FUN_1417c6130` release; lock = CHARACTER `+0x75c` status (10 free / 5 locked), `+0x764` counter |
| Retinue / units | `retinue(cqi)`, `recruitable(cqi, slot)`, `unit(cqi, slot)` | `recruit(cqi, key, {source="unlocked"/"locked"/"any", free, slot, replace, hp, experience})`, `replace(cqi, slot, key, opts)`, `disband(cqi, slot)`, `unit_strength(cqi, slot, pct)`, `unit_experience(cqi, slot, lvl)` | slot recruitment interface vfunc `+0x80`; disband = same command with an empty key; UNIT `+0xa8/+0xac` men, `+0x110` chevrons (direct writes) |
| Three Kingdoms | `faction(key)`, `world_leaders()` | `faction_progression(key, level)`, `force_three_kingdoms{forced, banned, seats, include_human, bypass}`, `world_leader(key)`, `emperor_policy{forced, banned}` + `se.load_emperor_policy()` | thresholds rewrite + `FUN_1419f3310`; seat via `FUN_1416901c0` + `FUN_141690300` when the engine refuses (governor factions) |
| Experience / skills | `character_xp(cqi)`, `skill_points(cqi)`, `faction_effect_value(key, id)`, `faction_xp_gain_percent(key)` | `character_add_xp(cqi, n, scaled)`, `skill_points(cqi, n)` | DETAILS `+0xcc` xp, `+0xc4` rank-1, `+0xc0` unspent points; raw add = field write + `FUN_141a36ae0(details, 0)` rank loop; effect 0x181 via `FUN_140abcba0` |
| Assignments | `assignment(cqi)` -> key, state, rounds, **province** | (none) | ASSIGNMENT `+0x28` -> target `+0xb8` -> PROVINCE, key at `*(*(prov+0x20))+8` |
| Campaign AI | `cai_personality(key)` (model thread) | `cai_personality(key, personality_key)` | component `+0x10` key / `+0x30` runtime object from registry `*(ai_world+0xa70)`; persists |
| Faction potential | `faction_potential(key)` | `faction_potential(key, value)` (-100..150) | FACTION `+0xee0` {base,bonus,roll} + `FUN_1419f5aa0`; persists |
| Menu build number | `build_number()` -> {build, short, modified} | `build_number(build, short, modified)`; also auto-applied at injection from `<dll dir>\script_extender.cfg` (`build_number=`, `build_number_short=`, `build_modified=`) | GameCore = `*(*(DAT_143c53a28)+0x960)`; CA::Strings at `+0x90` (BuildNumber) / `+0xa0` (BuildNumberShort), byte `+0xea` IsBuildModified; composed once by FUN_1402e6740 ("v%d.%d.%d  Build %d.%d (modded)"). 0.18: cfg is read from the DLL folder or its parent, and the apply waits (background thread, up to 120 s) until GameCore holds the composed strings, so injecting seconds after launch is fine. Verified live through the mod manager. |
| Diplomacy deals | (stock) | not wrapped: `cm:modify_faction(a):apply_automatic_diplomatic_deal(situation, query_faction_b, "faction_key:"..b)` after `can_apply_automatic_diplomatic_deal`; situations e.g. `data_defined_situation_war_proposer_to_recipient`, `..._peace`, `..._create_alliance_no_conditions`, `..._vassalise_recipient_forced` | vanilla `3k_campaign_diplomacy_manager.lua:555+` |
| Buildings (0.19, forced build 0.23; damage/repair/destroy/forced construct verified live 0.23.3, completion after a turn pending) | `region_slots(region)`, `building_candidates(region, slot, {only_valid, all_chains})` | `building_damage(region, slot, pct)`, `building_repair(region, slot, {free})`, `building_destroy`, `building_construct(region, slot, level_key, {force=true, any_chain=true, free, turns, complete, pay_to_complete})` (upgrade/convert = target level key) | SLOT+0x318 manager M, M+0x20 building B (+0x20 record, health via FUN_141cc03c0/FUN_141cc0410), M+0x10 construction in progress; list = M vtable +0x130 (only_valid, 1, all_chains, 0, 0, 0) -> 0x30-byte entries (+0 record, +0x10 cost, +0x14 turns, +0x18 reason bits, +0x24 secondary cost); construct = M vtable +0x10 (M, entry*) on a copy of the entry (free zeroes the costs, turns overrides +0x14); repair/destroy/pay-to-complete = +0x40/+0x38/+0x28; notes/buildings.md |
| Alliance names (0.20, pending) | `alliances()` -> cqi, name, members | `alliance_name(cqi, text, "inline"/"pointer")` | ALLIANCE +8 cqi, name = `*(+0x60)` UniString* else inline UniString at +0x68 (CcoDiplomacyAlliance.Name); UniString ctor FUN_140663120, swap FUN_140663ea0; persistence to verify |
| Effect bundles (0.21 read, 0.23.3 write; define + apply_custom verified live, persistence pending) | `effect_bundle(key)` -> engine entries dump | `effect_bundle_define(bundle_key, {{effect=, scope=, value=}, ...})` rewrites an existing record's list for the session (any holder, stock apply; redo after each load), `effect_bundle_restore(bundle_key)`, `effect_bundle_apply_custom(faction, bundle_key, effects, turns)` = engine per-instance custom list | record +0x3c count / +0x40 entries (0x30: +0 effect, +8 scope, +0x10 f32 value, +0x18 bonus-value vector, +0x28 stage 7); entry ctor FUN_140e6e260(out, effect, scope, f32); instance (0x38) FUN_140e6ea50, custom entries vector at +0x28 used by FUN_140e87a50 when count != 0; faction apply FUN_1419a2f80(faction, instance), list at FACTION+0xc78, deep copies (six natives share the name apply_effect_bundle; FUN_141902ea0 belongs to another holder type); notes/income_effects.md |
| Attitude (0.22, pending) | `attitude(a, b)` -> standing | `attitude(a, b, level)` level -3..3 = the engine's small/medium/large attitude events (values from DB) | FUN_141b965e0(mgr, A, B); FUN_141b7cf60(mgr, A, B, level) = the `diplomatic_attitude_change` payload; treaty-component bias not done |
| Income lines (0.22, script-side) | `faction_income(key)` | `faction_income(key, amount, label)`, `se.load_income_lines()` after a load | paid at FactionTurnStart via increase_treasury; not in the engine breakdown; force-scoped gdp hook not done (region GDP code not reached) |
| Auto-resolve (0.24 read + tunables + plan; 0.25 simulation hook; **0.26.2: plan.casualties and plan.winner applied and verified live**; plan.bias and plan.duels stored only) | `pending_battle()` -> context, `autoresolve_prediction()`, `autoresolver_variable(key)`, `autoresolver_variables()`, `autoresolve_plan()` | `autoresolver_variable(key, value)`, `autoresolver_variables_reset()`, `autoresolve_plan(plan)`, `autoresolve_plan_clear()`, `se.autoresolve.set_handler(fn(ctx) -> plan)` (PendingBattle listener, local player battles only) | campaign variables = f32[774] at `*(world+0x3b58)` indexed by descriptor index (descriptor array RVA 0x3e33520, stride 0x78, name at +0x68); PB = `*(world+0x3b80)`, prediction in result `(*(PB+0xd0+night*0x10))[*(PB+0xe8)]`, side block +0x7c/+0x64, +8 casualties, +0xc enum; notes/autoresolve.md |

Raw natives (all `se_*` globals) are listed at the top of each `src/*.rs` file; treat them as
internal. Test/console scripts for every feature live in
`<game>\lua_scripts\se_*.lua` (registered in `index.txt`); they double as usage examples.

## 5. Engine facts a maintainer needs

- Lua 5.1, **`lua_Number` is a 32-bit float** in this build (`lua_pushnumber` = `movss`);
  `string.find` accepts only two arguments in this build.
- Script objects handed to natives are full userdata: `*(payload)` = script object,
  `*(obj+0x18)` = engine object (CHARACTER, FACTION, UNIT, PROVINCE...).
- Model/world from a character: `*(*(char+0x250)+0x78)`; from a faction:
  `*(*(faction+0x288)+0x78)`. DB manager `world+0x3b38` (`db_get`), faction manager
  `world+0x3b68`, world-leader manager `*(fm+0x270)`, character cqi table `*(world+0x3c18)`.
- DB record keys: land units `record+8 -> String*`; personalities / provinces / progression
  levels use an **inline** CA::String (`{u32 len, u32 cap, char* @+8}`) at `+8`.
- Crashes we hit and their lessons: (a) a routine given the wrong owner object
  (`FUN_141b94560` wants `*cai_manager`, the AI world); (b) faking an engine object with a bare
  pointer cell (the personality object has sub-objects the AI reads). Natives now validate
  vtables / back-pointers / registry round-trips and refuse instead of writing.
- Never modify the exe on disk; all patches are in-memory and anchor-checked.

## 6. Version history (each folder under `Z:\RE\se_deploy\`)

0.5 recruit; 0.7 disband attempt; 0.8 embedded Lua API + strength/chevrons + progression +
xp/assignment probes; 0.9 unit vtable match; 0.10 float numbers, disband via empty key,
governor bypass; 0.11 assignment province; 0.12 flat xp, skill points, effect value, CAI
personality, potential; 0.13 CAI apply owner fix, float effect value; 0.14 registry-based
personality object; 0.15 registry self-check; **0.16 menu build number + cfg file (wrote into the wrong object); 0.17 correct GameCore pointer; 0.18 cfg lookup in parent folder + deferred apply (verified through TKModManager, which builds from the workspace and stages `dll\<version>\`); 0.19 buildings; 0.20 alliance names; 0.21 effect bundle inspection; 0.22 attitude events + script-side income lines (read-only batch verified live); **0.23 forced building construction through the list entry + effect bundle define / restore / apply_custom (buildings, bundles, alliance rename, income line verified live in 0.23.3)**; **0.24 auto-resolve read-out, autoresolver_* tunables, plan storage + handler (current; untested live)**.

## 7. When the game updates

1. New `TimeDateStamp` / `SizeOfImage` -> update the constants in `src/addrs.rs`.
2. Re-derive every RVA in `ENTRIES`/`VTABLES` in Ghidra (`Z:\RE\TW3K` project) using the
   function names and decompile landmarks recorded in `notes/*.md` (each note names the
   `FUN_14xxxxxxx` and how it was found: script-native registration tables, string xrefs,
   CCO handler names). Read the first 8 bytes from the new exe (`tools`-style Python that maps
   RVA -> file offset through the section table) for the anchors.
3. Rebuild, inject, confirm `all N addresses ... verified`, then re-run the `se_*.lua` scripts.

## 8. Open items (not blocking the manager)

- Direct skill grant by skill key (skills are not CEOs outside Nanman): needs the skill
  allocation executor. Unspent-point counter is done.
- Character-side experience-gain modifier (the +74% seen on a character was mostly his own
  effects) and an effect id -> key map (effects table index).
- Chevron writes and unit strength are direct field writes; confirmed by the stock getters and
  the retinue panel, but the engine's own setters were never traced.
- `emperor_policy` turn-start listener registered but not observed across a turn yet;
  three-kingdoms seats verified in session, save persistence of the seats not yet tested.
- Diplomacy wrapper (stock automatic deals) not written.
