# Script extender DLL: handoff for the mod-manager integration

Written 2026-09-17. Everything below was verified live on Total War: THREE KINGDOMS **1.7.2.0**
(Steam build 25370317) unless marked otherwise. Current stable DLL: **0.41.0**, `Z:\RE\se_deploy\0.41.0\`; pre-release line 0.42.0-beta.N (latest
0.42.0-beta.4, `Z:\RE\se_deploy\0.42.0-beta.4\`)
(`script_extender.dll` + `injector.exe`; releases are the `v*` tags on GitHub). Source:
`Z:\Claude\ScriptExtender` (Rust workspace). Deep RE notes: `notes/*.md`; day-to-day rules:
`CLAUDE.md`; **scripting documentation for mod authors: `docs/SCRIPTING.md`**.

## 1. What the DLL is

A native DLL injected into the running `Three_Kingdoms.exe` that:

1. fingerprints the exe (PE `TimeDateStamp 0x69ce4c84`, `SizeOfImage 0x4836000`) and verifies 109
   engine addresses by their first 8 bytes plus 3 vtables by their slot-0 RVA (`src/addrs.rs`);
   **on any mismatch it logs and does nothing** (safe on a patched game). The Epic build
   (`TimeDateStamp 0x69ce4df8`) uses the same table;
2. detours `lua_gettop` (a trampoline hook, installed with all other threads frozen) and, the
   first time each Lua state passes through it, registers the `se_*` C natives into that state's
   globals and runs the embedded `lua/se_api.lua`, which defines the public `se.query.*` /
   `se.modify.*` API (`src/hook.rs`);
3. exposes named, high-level operations only (no generic peek/poke/call to Lua).

**Multiplayer is supported from 0.30** under lockstep rules: in multiplayer `se.modify.*` only runs inside model callbacks (never queued), synced logic must not depend on the local machine, and the game build string is always version-locked (`[se <version>.<sync>]`, sync = fingerprint of the simulation-relevant cfg keys) so only identical script extenders can share a lobby (lobby check not yet verified on two machines). The manager must give both players the same DLL version and the same simulation cfg values (the ten sync-tag keys listed in docs/SCRIPTING.md §1).
Other native mods (ThreeKingdoms-Coop) can query the DLL through its exports `se_status()` /
`se_version()` and read `se_inventory.json` next to it (anchors read, patches written); see
docs/SCRIPTING.md "Coexisting with other native mods".
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
- Success line to check in the log: `all 109 addresses and 3 vtables verified` then
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
Release profile: LTO thin, `panic = "abort"`, line tables only (0.42+: a `script_extender.pdb`
is written next to the DLL and shipped with each release; the crash reporter prints DLL frames as
`script_extender.dll+rva`, and the PDB of the same build names them). Rust stable, no other deps.

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
| Crash reporter (0.42, `diag_crash`, on by default, not in the sync tag) | `se.crash.info()` | `se.crash.mark(text)`, `se.crash.selftest([mode])` | vectored exception handler (first-chance, CONTINUE_SEARCH) writes `se_crash.txt` next to the DLL: fault as module+RVA, registers, RtlVirtualUnwind stack (a frame without unwind info is a retour trampoline: popped), stack scan, the SE hook / native active on the thread (thread-local scope stack), last 64 SE events, installed hooks. Every native runs through one shim (`lua.rs native_shim`, upvalue = entry pointer as an 8-byte string). `diag_crash=2` streams every hook / native entry to `se_activity.txt`. `crash.rs` |
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
personality object; 0.15 registry self-check; **0.16 menu build number + cfg file (wrote into the wrong object); 0.17 correct GameCore pointer; 0.18 cfg lookup in parent folder + deferred apply (verified through TKModManager, which builds from the workspace and stages `dll\<version>\`); 0.19 buildings; 0.20 alliance names; 0.21 effect bundle inspection; 0.22 attitude events + script-side income lines (read-only batch verified live); **0.23 forced building construction through the list entry + effect bundle define / restore / apply_custom (buildings, bundles, alliance rename, income line verified live in 0.23.3)**; **0.24 auto-resolve read-out, autoresolver_* tunables, plan storage + handler**; 0.25 - 0.27 the auto-resolve result is actually rewritten (winner and casualties verified live on 0.26.2, duels on 0.27.0); 0.28 - 0.29 horde income hook; **0.30 multiplayer support with the enforced build-string version lock**.

### 0.31 - 0.34: performance (2026-09-19)

- 0.31 sampling profiler (`se.profile.start/stop`, reports in `<dll folder>\..\profiles\`).
- 0.32 UI recruit-list cache (`ui_recruit_cache_ms`); 0.32.4 made the horde income hook cheap
  (it had cost 35% of the main thread during an end turn: one IsBadReadPtr per field).
- 0.34 `recruit_perm_cache` (default on): the recruit list routine FUN_141931e10 rebuilt a full
  permission hash map per candidate unit; now one per list call, shared as views, self-checked
  for the first 3000 reuses per session. Measured on a modded late campaign: end turn 76 s ->
  45 s (income hook) -> ~30 s (permission maps). Details: `notes/performance.md`.
- 0.35 `file_probe_cache_ms` (missing-directory cache for loose-file lookups), Lua listener
  timing (`se.diag.listeners_*`), `diag_diplomacy` (measurement).
- 0.36 AI recruitment trace (read-only): `se.ai_recruit.trace/report`, `se.query.ai_recruitment`,
  `se.query.unit_quality`; details `notes/ai_recruitment.md`.
- 0.37.0-beta.1 (PRE-RELEASE channel from here on, tags `vX.Y.Z-beta.N`): AI recruitment policy
  `se.ai_recruit.enable/plan/execute/set_policy` (element-weighted quality score; fills empty
  slots, replaces weak units at normal cost). Lua only; no new hook, no cfg key.
- 0.37.0-beta.4: the horde income hook is ON by default (category MINING); `horde_income=0`
  turns it off. Mods no longer depend on a cfg line. NOTE for the manager: its cfg writer
  (`dll.rs` render_cfg) keeps only the build_number keys and drops every other line of
  `script_extender.cfg`; it should preserve unknown keys.
- **0.40.0 = the 0.37 beta line gone live (2026-09-19)**: stable channel now has the performance
  work (0.31-0.35), the AI recruitment trace + policy (0.36-0.37) and the default-on horde income
  hook. Version numbers compare as dotted integers in the manager (0.40 > 0.37 > 0.4).
- 0.41.0-beta.1 (pre-release line after 0.40.0; measurement build for the court screen open
  latency): the profiler unwinds through our own detours (callers of a hooked function are no
  longer cut off) and writes a third report `profile_<label>.timeline.txt` (one line per main
  thread sample; `tools/profile_timeline.py`). No new hook, no cfg key, no API change.
- **0.41.0 (stable, 2026-09-20)** = 0.41.0-beta.10 as released: the MEDIATE PEACE button fix, the diplomacy
  validation trace (`diag_diplomacy=1|2`, off by default) and the beta.1 profiler timeline. beta.2 - beta.10
  were local builds, never tagged. Mods must call `se.ui.fix_followup_button()` on first tick.
- 0.41.0-beta.10 (**confirmed in game 2026-09-20**): fix for the dead MEDIATE PEACE button of the first
  follow-up negotiation popup on 1.7.2.0 (`followup.rs`, always installed): `se.ui.fix_followup_button()`
  (click listener, needs a mod script to call it on first tick) + `se.modify.followup_propose()`; the DLL
  sends the engine's own negotiation command op 5 from the per-frame UI update. beta.4 - beta.9 were
  diagnostic steps towards it (more `diag_diplomacy` hooks: deal builder, state machine, command handler,
  ProposeDeal / CanPropose). Rule learned: never call a UI/CCO handler from inside a Lua native.
- 0.41.0-beta.3: beta.2 plus detours on the deal builder (FUN_141ad4c00 add component, FUN_141ad0870
  expand required treaties), exe call stacks and mouse-click markers in `dip_trace.txt`. Same cfg switch.
- 0.41.0-beta.2 (diagnostic build for "MEDIATE PEACE does nothing", reproduced without 190Expanded and
  without the DLL): `diag_diplomacy=1|2` additionally hooks the diplomacy condition tree
  (FUN_1413c86b0 group node, FUN_1413d0cc0 requirement leaf; `diptrace.rs`) and records the engine's
  treaty-component validations: `se.diag.diplomacy_trace / diplomacy_mark / diplomacy_report`,
  report `dip_trace.txt` next to the DLL. `2` records from injection (no Lua console needed). Read-only,
  not in the sync tag. Default `0` = no hook at all.
- Manager-relevant: cfg keys `recruit_perm_cache`, `ai_recruit_cache` are in the sync tag;
  profiler reports live in `dll\profiles\`, which the manager must not delete.
- 0.42.0-beta.1 (2026-09-22): **crash reporter** (`crash.rs`, cfg `diag_crash`, default on, not
  in the sync tag): on a fatal exception the DLL appends a report to `se_crash.txt` next to itself
  (fault as module+RVA / Ghidra address, registers, unwound stack, stack scan, the hook or native
  the faulting thread was in, the last 64 SE events, installed hooks); `diag_crash=2` also streams
  every hook / native entry to `se_activity.txt`. Lua: `se.crash.mark/info/selftest`. Every native
  now runs through one shim (`lua.rs`), so "which native" is known. New cfg switches
  **`ai_recruit_hook`** and **`followup_hooks`** (default 1; `0` skips the planner detour / the
  MEDIATE PEACE detours) for bisecting a crash: **both are in the sync tag** (seven keys now).
  `tools/dump_crash.py` reads the fault-time context from a minidump (the thread's own context in
  the dump is the dump writer's). Release builds ship `script_extender.pdb`. Manager notes: surface
  `se_crash.txt` from the DLL folder in a bug report; the cfg writer must keep unknown keys.
- 0.42.0-beta.2 (2026-09-22): **save chunking** (se_api.lua, automatic, cfg `save_chunking`,
  default on, not in the sync tag). The engine keeps at most ~64 KiB of one saved string and
  `cm.saved_values` is saved as one string, so large campaigns lost every mod's saved values on the
  next load. `campaign_manager:save_named_value` / `load_named_value` are wrapped on the class (at
  se_api load, or as soon as the class and its methods are defined, via temporary metatables):
  values above 60,000 bytes go to `<name>__se_<i>` entries plus a `--se_chunks:<n>:<len>` marker.
  Chunked saves need the DLL to load their big values. `se.saves.info()`; offline test
  `tools/test_save_chunking.py` (real vanilla lib_campaign_manager under lupa 5.1).
- 0.42.0-beta.3 (2026-09-23): **relatives by marriage may marry** (`marriage.rs`, cfg
  `marriage_inlaws` default 1 and `marriage_blood_generations` default 0, **both in the sync tag:
  nine keys now**). The marriage verdict FUN_14141e7e0 refused any pair joined by a chain of
  family links (FUN_1413d4c20: father / mother / spouse / +0x50 / children, unbounded), so one
  marriage between two houses blocked every further one. Detours on both: while the verdict runs
  on the thread, the relatedness search answers "related" only for a shared blood ancestor within
  N generations; the verdict's close-kin test still applies. Distant-relative status and every
  other use of the search are unchanged. `se.query.marriage_hook()`; notes/family.md.
- 0.42.0-beta.4 (2026-09-23): **coexistence with ThreeKingdoms-Coop** (review in
  `script-extender-compatibility.md`). The unused `char_by_cqi` anchor (RVA 0x1457760) is gone:
  coop detours that function, and with coop loaded first (its AGS proxy, then the manager) the
  anchor check refused the whole table. Exports `se_status()` (0 booting / 1 ready / 2 refused /
  3 not the game) and `se_version()` ("<version>.<sync>"), `se_inventory.json` next to the DLL
  (anchors, patches with bytes before/after, natives), Lua `se.status()`. Hardening: every detour
  is published before it is enabled (lua_gettop and auto-resolve were not), no patch is written
  while a thread is still inside the target bytes, anchor mismatches that look like another
  mod's detour say so in the log. Version lock: `{version}` without `{sync}` in cfg text no longer
  drops the sync hash, and **`save_chunking` joins the sync tag (ten keys now)**. se_api.lua asks
  `cm:query_model():is_multiplayer()` first (`cm:is_multiplayer()` reads false before
  WorldCreated) and treats "unknown" as multiplayer; `tools/test_lua_mp_guard.py`.

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
