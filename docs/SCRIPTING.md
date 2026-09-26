# Script Extender: campaign Lua API

Scripting reference for the `se.*` API that the script extender DLL adds to Total War: THREE
KINGDOMS **build 1.7.2.0**. Audience: mod authors who already know 3K campaign scripting
(`cm`, `core:add_listener`, `QUERY_*` / `MODIFY_*` interfaces).

Sources of truth for this document: `crates/script_extender/lua/se_api.lua` (signatures),
the DLL sources under `crates/script_extender/src/` (defaults, refusals), `HANDOFF.md`
(injection rules, version history), `notes/*.md` (behaviour, live verification). Current DLL
version at the time of writing: **0.41.0** (2026-09-20): 0.40.0 (the 0.37 beta line on the stable
channel) plus the MEDIATE PEACE button repair and the diplomacy trace.

### Which DLL version a feature needs

Every function entry in §3 carries its own `Since` line; this is the overview. The mod manager
compares versions as dotted integers, so **0.40 > 0.37 > 0.4**.

| DLL | Brings |
|---|---|
| 0.8 | the embedded `se_api.lua` module itself, characters / pools, retinues, units, progression |
| 0.10 - 0.12 | disband, world-leader seats, assignments, flat XP, skill points, CAI personality, faction potential |
| 0.16 - 0.18 | menu build number and `script_extender.cfg` |
| 0.19 - 0.23 | buildings, alliance names, effect bundles, attitude events, script-side income lines |
| 0.24 - 0.27 | auto-resolve: tunables, prediction, plans (winner + casualties 0.26.2, duels 0.27) |
| 0.28 - 0.30 | horde income hook, **multiplayer support with the enforced version lock** |
| 0.31 - 0.35 | performance work and diagnostics (`se.profile.*`, `se.query.perf`, `se.diag.*`) |
| 0.36 | AI recruitment trace (`se.ai_recruit.trace/report`, `se.query.ai_recruitment/unit_quality`) |
| 0.37 | AI recruitment policy (`se.ai_recruit.plan/execute/enable`) |
| 0.40 | stable release of everything above, horde income on by default |
| **0.41** | the current stable release: repair of the dead MEDIATE PEACE button (`se.ui.fix_followup_button`), diplomacy validation trace (`se.diag.diplomacy_*`), profiler timeline |
| 0.42 (pre-release) | crash reporter `se_crash.txt` (`diag_crash`, on by default; `se.crash.*`, §3.18), the switches `ai_recruit_hook` and `followup_hooks` for narrowing a crash down; beta.2: saved values larger than 64 KiB survive a save (§3.19); beta.3: relatives by marriage may marry (§3.20); beta.4: coexistence with other native mods (ThreeKingdoms-Coop), `se.status()`, the multiplayer check asks the model first, `save_chunking` joins the version lock; beta.5: per-character auto-resolve duel power bonus (`se.modify.duel_power_bonus`, §3.14, cfg `duel_power_hook`) |
| **0.43** | auto-resolve duels decided by the duelists' real stats (`se.modify.duel_power_formula`, §3.14): the unit-card stat block, hit points and abilities, with a refusal gap written into the duel roll's own settings |

---

## 1. Overview

### What the script extender is

A native DLL injected into the running `Three_Kingdoms.exe`. It:

1. fingerprints the executable and verifies every engine address it uses by its first bytes; on
   any mismatch it logs and does nothing, so a patched/updated game is safe but inert;
2. detours `lua_gettop` and, the first time each Lua state passes through it, registers a set of
   `se_*` C natives into that state's globals and runs an embedded Lua module (`se_api.lua`),
   which defines the public `se.query.*` / `se.modify.*` / `se.autoresolve.*` API;
3. exposes only named, high-level operations. There is deliberately no generic peek/poke/call
   primitive.

The natives (`se_recruit_unit`, `se_slot_construct`, ...) are internal. **Scripts use the `se`
module only.**

### Multiplayer (DLL 0.30+)

Campaign multiplayer is lockstep: every machine runs the same scripts and must make the same
change at the same model tick. The API follows three rules so that it can be used there:

1. **`se.modify.*` runs only inside a model callback in multiplayer** (event listeners,
   first tick, turn start, dilemma choices: anything where `cm:can_modify()` is true). Called
   from a UI click, a timer or the in-game console it returns
   `false, "refused in multiplayer: ... called outside a model callback ..."` instead of being
   queued: only that machine would change its model and the game would desync. In single player
   such calls are still queued through `cm:wait_for_model_sp`. Since 0.42.0-beta.4 "multiplayer"
   is asked from the model (`cm:query_model():is_multiplayer()`) first, because
   `cm:is_multiplayer()` reads false until WorldCreated; when neither answers, the call is
   treated as multiplayer and refused.
2. **Nothing a synced script does may depend on which machine it runs on.** Do not branch on
   `cm:get_local_faction()` or on `ctx.*.is_local_player` in code that ends in a modify call;
   use `is_human` and faction keys. The auto-resolve handler is called for every battle with a
   human player, on every machine, and chance-based duel rules are seeded from `ctx.seed`
   (turn number and force cqis), never from anything machine-specific. Do not use `math.random`
   or `os.time` to decide a change; use the model's own random functions.
3. **Both machines must run the same script extender with the same simulation settings.** The
   DLL enforces this through the game's build string, which the multiplayer lobby compares:
   it always ends up containing the DLL version and a fingerprint of the thirteen settings that can
   change the simulation — `autoresolve_hooks`, `ai_recruit_cache`, `recruit_perm_cache`,
   `horde_income`, `horde_income_category`, `ai_recruit_hook`, `followup_hooks`,
   `marriage_inlaws`, `marriage_blood_generations`, `save_chunking`, `duel_power_hook`,
   `prebattle_single_delegate` and `postbattle_single_continue`
   (`script_extender.cfg` text may use `{version}`
   and `{sync}`; text without both gets ` [se <version>.<sync>]` appended; without cfg text the game's
   own string is extended; `se.modify.build_number` cannot remove it). A player without the
   DLL, with another DLL version or with different hook settings shows a different build and
   cannot join. The lobby's version check does use that string: **verified on two machines
   (2026-09-23, 0.42.0-beta.4): the same DLL version joins, a different version is declined.**
   A pair with the same version but different settings has not been tried.

**Two ways a change reaches every machine.** (a) *Replicated local change*: the same model
callback runs on every peer with the same arguments and each changes its own model once. This
is the `se.modify.*` contract; do not wrap such a call in an "only the owning player" check.
(b) *Command submission*: one client submits one engine command, which the engine orders and
runs on every peer (the MEDIATE PEACE repair, `se.ui.fix_followup_button`, works this way). Never
submit such a command from every peer. Decide which of the two a feature uses before writing it;
a UI button that should change the model has to go through (b) or a synchronised event, never
call `se.modify.*` straight from the click.

Session-only state (auto-resolver variables, redefined bundles, income lines, emperor policy)
has to be re-applied by the mod after loading, from a callback that runs on every machine.

Saves made after using `se.modify.*` carry the results — they are ordinary engine state, and the
DLL is not required to load such a save, only to keep using the API.

### Detecting the API

```lua
if type(se) ~= "table" then
    ModLog("script extender not present in this Lua state")
    return
end
ModLog("script extender " .. se.version())
if not se.available("se_slot_construct") then
    ModLog("this DLL is too old for buildings")
end
```

- `se.version()` -> the DLL version string, or `"unknown (DLL older than 0.8)"` when the
  `se_version` native is missing.
- `se.available(native_name)` -> `true` when that native exists in this Lua state. Use it to
  feature-gate against older DLL builds. Native names per feature are listed with each function
  below.
- `se.status()` (0.42.0-beta.4) -> `{ state, version, patches }`: see §3.0.

### Coexisting with other native mods

The DLL patches only process memory and checks the first bytes of every engine function it
uses. Since 0.42.0-beta.4 it no longer anchors functions it does not use, so a mod that detours
one of those (ThreeKingdoms-Coop hooks RVA `0x1457760`) no longer stops it from starting.
When a check still fails, the log says which function and whether its first bytes look like
another mod's detour, and the DLL stays inert.

Other native mods can ask whether the extender is up, without relying on load order:

| Export (`script_extender.dll`) | Returns |
|---|---|
| `uint32_t se_status(void)` | 0 booting, 1 ready, 2 refused (unknown exe or anchor mismatch), 3 not the game |
| `const char* se_version(void)` | `"<dll version>.<sync tag>"`, the pair the build string carries |

`se_inventory.json` next to the DLL lists the exe fingerprint, every anchor read (name, RVA,
bytes, ok), every patch the DLL wrote (name, cfg switch, RVA, patched length, bytes before and
after) and the registered natives. It is written when the bootstrap ends and again after the
first Lua state has been set up. Comparing it with another mod's list of writes shows
write/write and write/anchor collisions before a game is played.

### Setup lines every script needs

The module chunk runs inside the DLL, and its environment does **not** see the script libraries'
globals. Two objects must be handed over explicitly:

```lua
se.logger = ModLog     -- any function(string); otherwise output goes to the DLL log
se.core   = core       -- the event manager; needed by everything that installs a listener:
                       -- emperor_policy, faction_income, autoresolver_variable,
                       -- se.autoresolve.set_handler, se.diag.listeners_start,
                       -- se.ai_recruit.enable
```

Without `se.logger`, the module falls back to a global `ModLog` if one is visible, else to the
DLL's own `se_log` (which writes to `script_extender.log`). Without `se.core`, the functions that
install listeners return
`false, "core (event manager) is not available: set se.core = core ..."`.

### How results are returned

- **Queries** (`se.query.*`) return a value or table on success, and `nil, message` on failure.
- **Modifiers** (`se.modify.*`) return `ok, message`. `ok == true` means *done or queued*.

### The model-thread rule

Model mutation is only legal on the campaign model thread. Almost every `se.modify.*` call goes
through `se.on_model`:

- when `cm:can_modify()` is already true (inside an event handler, a `cm:wait_for_model_sp`
  callback, ...), the work runs immediately and the **real** `ok, message` is returned;
- otherwise the work is queued with `cm:wait_for_model_sp`, and the call returns
  `true, "<tag> queued on the model thread (result in the log / callback)"`. The real result is
  written to the log and passed to the optional callback where a function supports one.

```lua
se.on_model("my step", function()
    -- runs where cm:can_modify() is true
    return true, "ok"
end, function(ok, msg) ModLog("finished: " .. tostring(ok) .. " " .. tostring(msg)) end)
```

`se.on_model(tag, f [, cb])` is public; use it to wrap your own follow-up reads (for example
`se.query.cai_personality`, which also needs the model thread).

**Four modifiers do not go through it**, because they change no model state: `build_number`
(process-local UI strings), `autoresolve_plan_clear` (a DLL-side slot), and the two that only
keep script-side state and install a listener, `emperor_policy` and `faction_income`. They
return the real result directly and do not check multiplayer — but what their listeners *do* is
model state, so install them from code that runs on every machine.

### Logging

`se.log(s)` prefixes `[se] ` and writes through `se.logger` / `ModLog` / `se_log`. Queued
modifiers log their real result there. In the in-game console environment, `ModLog` output lands
in `lua_mod_log.txt` and in the per-session `ironic_log_<faction>_<stamp>.txt`.

---

## 2. Conventions and gotchas

**Lua is 5.1 and this build has quirks:**

- **`lua_Number` is a 32-bit float.** Integers are exact only up to 16,777,216 (2^24). Large cqis
  and treasury values above that lose precision when they cross the Lua boundary. Keep an eye on
  it when you compute with treasury numbers; character cqis in a campaign are far below the limit
  in practice (~1,800 characters in the grand campaign).
- **`string.find` takes only two arguments in this build.** A four-argument call
  (`string.find(s, p, 1, true)`) silently never matches. Same for the `init`/`plain` parameters of
  the method form.
- `cm:method` without a call is a syntax error; list interfaces have no `is_null_interface`
  (use `se.is_null(v)`, which pcall-guards the check).
- Null interfaces answer any method with a function, so every engine call inside the module is
  `pcall`-guarded. Do the same in your own code.

**`cm:callback` does not fire from the in-game Lua console.** Chain follow-ups with
`cm:wait_for_model_sp` instead (the model queue runs in order). This matters for two API
functions that schedule their own follow-up through `cm:callback`:

- `se.modify.recruit` applies `opts.hp` / `opts.experience` one second after the recruit;
- `se.modify.move_character` runs its pool/recruited step one second after a faction move.

From a normal mod script (`script/campaign/mod/...`) these work. From the console they do not
fire, so call `se.modify.unit_strength` / `unit_experience` or the release step yourself in a
`cm:wait_for_model_sp` block.

**The console caches a script's text** until its panel is closed and reopened; after editing a
script, close and reopen the console before loading it again.

**Finding pool characters.** `QUERY_FACTION:character_list()` contains only recruited characters.
Recruitment-pool characters are reachable by cqi (`cm:query_character(cqi)`), through the
`NewCharacterEnteredRecruitmentPool` event, or by scanning cqis. Compare
`q:command_queue_index()` with `tonumber()`.

**Persistence.** Most effects are ordinary engine state and are saved with the campaign. These
are **not**, and must be re-applied after every load:

| Thing | What survives | What to do after a load |
|---|---|---|
| `se.modify.effect_bundle_define` | nothing (the DB record is patched in memory for the session) | call `effect_bundle_define` again and re-apply the bundle |
| `se.modify.autoresolver_variable` | nothing | call it again (within a session the module re-applies values at the local player's `FactionTurnStart`, because the engine rebuilds the variable array when its per-round overrides change) |
| `se.modify.autoresolve_plan` | nothing; the plan is keyed to one pending battle and cleared on `BattleCompleted` | set a new plan / install the handler again |
| `se.modify.build_number` | nothing (process-local UI strings) | set it again, or let the DLL apply `script_extender.cfg` at injection |
| `se.modify.emperor_policy` | the policy string (via `cm:save_named_value`); the listener does not | `se.load_emperor_policy()` |
| `se.modify.faction_income` | the income lines (via `cm:save_named_value`); the listener does not | `se.load_income_lines()` |
| `se.autoresolve.set_handler` | nothing (a Lua function) | install it again |
| `se.ai_recruit.enable` and its tunables | nothing (Lua state) | set the tables and call `se.ai_recruit.enable()` again |
| `se.diag.listeners_start` | nothing (Lua state) | call it again if you still want the timings |

Verified to persist across save / restart / load: `cai_personality` and `faction_potential`.
Not yet tested: persistence of forced world-leader seats, of a renamed alliance, and of the
per-instance custom effect list from `effect_bundle_apply_custom`.

**Reading engine dumps.** Several query tables carry an `engine` (or `engine_*`) field holding a
raw diagnostic string from the DLL. It is for debugging and its format is not stable.

---

## 3. API reference

Notation: `(a, b [, c])` marks optional arguments. "Status" reflects `HANDOFF.md` and the notes at
the time of writing.

### 3.0 Core helpers

#### `se.version() -> string`
DLL version string. Since 0.8 (returns `"unknown (DLL older than 0.8)"` on earlier builds).

#### `se.available(native_name) -> boolean`
True when the given `se_*` native exists in this Lua state.

#### `se.status() -> { state, version, patches }`
`state` is `"ready"`, `"booting"`, `"refused"`, `"not_game"` (or `"unknown"` on DLLs older than
0.42.0-beta.4); `version` is `"<dll>.<sync>"`; `patches` is the number of code patches the
DLL wrote. Since 0.42.0-beta.4. Native: `se_status`.

```lua
local st = se.status()
ModLog("script extender " .. st.version .. " " .. st.state .. ", " .. tostring(st.patches) .. " patches")
```

#### `se.on_model(tag, f [, cb]) -> ok, message`
Run `f` on the model thread (see §1). `f` returns `ok, message`. `cb(ok, msg)` is optional.
Refused in multiplayer outside a model callback; "multiplayer" comes from
`cm:query_model():is_multiplayer()` first (0.42.0-beta.4), and is assumed when unreadable.

#### `se.log(s)`
Log through `se.logger` / `ModLog` / `se_log`, prefixed `[se] `.

#### `se.is_null(v) -> boolean`
True for anything that is not a live userdata interface.

#### `se.G(name) -> value`
Look a global up in the state's real global table (the module chunk's `_G` is not the console's).

#### `se.character(cqi) -> QUERY_CHARACTER | nil, message`
`cm:query_character(cqi)` with a null check and a cqi round-trip check.

#### `se.faction(key) -> QUERY_FACTION | nil, message`
#### `se.region(key) -> QUERY_REGION | nil, message`

#### `se.dump(t [, indent]) -> string`
Pretty-print a table (sorted keys, nested). Console convenience:
`ModLog(se.dump(se.query.retinue(1)))`.

---

### 3.1 Characters and recruitment pools

Engine background (verified live): a CHARACTER carries a state field — 0 = in the faction's
recruitment pool, 1 = recruited, 2 = a third list. `move_to_faction` relinks by the character's
*current* state, so it moves a pool character pool-to-pool and a recruited character
recruited-to-recruited. The one primitive stock Lua lacks is **recruited -> pool**, which is what
`se_release_to_pool` provides.

The court's candidate list hides a pool character whose availability status byte is not 10;
that byte (and its counter) is what `pool_lock` reads and writes. A character moved between
factions is typically left locked (status 5) for a couple of rounds by the engine.

#### `se.query.character(cqi) -> table | nil, message`

Returns
`{ cqi, template, faction, in_pool, recruited, rank, experience, is_faction_leader,
has_military_force, has_region, wounded [, engine] }`.
`engine` is present when the native `se_char_info` exists.

Native: `se_char_info` (optional). Since 0.8. Status: verified live.

```lua
ModLog(se.dump(se.query.character(72)))
```

#### `se.query.pool_lock(cqi) -> status, counter | nil, message`

`status` 10 = available, 5 = locked; `counter` is the remaining round count. Native
`se_pool_lock_get`, since 0.3. Status: verified live.

#### `se.modify.pool_lock(cqi, turns) -> ok, message`

Lock a pool character (status 5) for `turns` rounds (default 1). Native `se_pool_lock_set`.
Model-thread queued. Saved with the campaign (ordinary character fields). The game clears the
lock on its own turn tick. Status: verified live.

#### `se.modify.pool_unlock(cqi) -> ok, message`

Status 10, counter 0 — the character becomes selectable in the court. **Reopen the court screen**
to see the list refresh. Status: verified live.

```lua
se.modify.pool_unlock(176)
```

#### `se.modify.release_to_pool(cqi) -> ok, message`

Move a **recruited** character into their own faction's recruitment pool, through the engine's
own release routine. Native `se_release_to_pool`.

Preconditions enforced by the engine: the faction must have a pool object, the character must
have details, and the character must not hold an assignment post (a governor is refused). UI-level
blockers (faction leader, commands or is embedded in a force, governs a region, has a character
post, wounded) are worth checking yourself. `is_politician()` and a non-null `active_assignment()`
are **not** valid blockers — every court member reports them.

Status: verified live (state 1 -> 0, pool count +1, stable).

#### `se.modify.move_character(cqi, faction_key [, state]) -> ok, message`

Move a character to `faction_key` (when they are not already there) and then force the requested
list membership.

- `state` = `"pool"` (default) or `"recruited"`. Anything else returns
  `false, "state must be 'pool' or 'recruited'"`.
- `"pool"` requires the native `se_release_to_pool`; `"recruited"` uses only stock calls.
- When a faction move happened, the state step runs **one second later** through `cm:callback`
  (so the engine has relinked the character first) and reports into the log, not into the return
  value. From the in-game console `cm:callback` does not fire — do the second step yourself.

Status: verified live for all three cases (pool -> pool, recruited -> own pool, another faction's
recruited -> our pool).

```lua
-- Guan Yu (cqi 72) into Cao Cao's recruitment pool
se.modify.move_character(72, "3k_main_faction_cao_cao", "pool")
```

---

### 3.2 Retinues and units

Slot 0 of a persistent retinue is the commander and has no recruitment interface. **A slot's
recruitment interface recruits INTO that slot, replacing whatever unit is there** — target an
empty slot to add a unit. Every slot's item list holds every unit in the game's recruitment trees
(~466 entries in the grand campaign, of which only a few dozen are unlocked); `reasons` is a bit
mask of lock causes, `0` meaning unlocked.

Unit strength is hit-point scaled internally; the API works in percent of full strength.
Strength and chevron writes are direct field writes confirmed by the stock getters and the
retinue panel; the engine's own setters were never traced (an open item — derived state could in
principle go stale).

#### `se.query.retinue(cqi) -> list | nil, message`

One row per slot, sorted by slot index:
`{ index, slot_cqi, unit_key, strength, experience, can_recruit, is_recruiting, recruiting }`.
`strength` / `experience` are only filled when the slot is linked to a deployed military force
unit. `slot_cqi` is the slot's own command-queue index — the number the stock
`UnitRecruitmentInitiated` event reports as `context:cqi()`, which is how you match an event back
to a slot. Since 0.8. Status: verified live.

Note: right after a campaign load, `retinue_slots()` entries can briefly read as null interfaces;
read the list again a moment later.

#### `se.query.recruitable(cqi, slot_index) -> list | nil, message`

`{ { key, record, cost, turns, reasons }, ... }` for that slot. `reasons == 0` means the item is
unlocked. Native `se_slot_items`, since 0.4. Status: verified live.

```lua
local items = se.query.recruitable(1, 1) or {}
for _, it in ipairs(items) do
    if it.reasons == 0 then ModLog(it.key .. " cost=" .. it.cost .. " turns=" .. it.turns) end
end
```

#### `se.query.unit(cqi, slot_index) -> table | nil, message`

`{ unit_key, strength, experience [, engine] [, strength_engine] }`. Since 0.8.

#### `se.modify.recruit(cqi, unit_key [, opts]) -> ok, message`

Start the engine's recruitment of `unit_key` into one of the character's retinue slots.

| option | type | default | meaning |
|---|---|---|---|
| `source` | `"unlocked"` \| `"locked"` \| `"any"` | `"unlocked"` | `"unlocked"` refuses an item whose `reasons ~= 0`; `"locked"` refuses an unlocked item; `"any"` accepts either. Locked items are forced through by clearing the item's lock reasons. |
| `free` | boolean | `false` | zero the cost |
| `slot` | number | first empty slot that can recruit | slot index to recruit into; slot 0 is refused |
| `replace` | boolean | `false` | allow an occupied slot (its unit is replaced) |
| `hp` | 0..100 | none | strength percent applied 1 s after the recruit (needs `se_unit_strength_set`) |
| `experience` | number | none | unit experience level applied 1 s after the recruit (needs `se_unit_xp_set`) |

Refusals you will see: `unit_key must be a non-empty string`, `opts.hp must be 0..100`,
`slot N holds <key>; pass replace = true to swap it`,
`no empty slot that can recruit (pass opts.slot, or replace = true)`,
`<key> is not in slot N's item list (M items)`,
`<key> is locked (reasons 0x...); use opts.source = 'locked' or 'any'`.

Natives: `se_recruit_unit`, `se_slot_items` (since 0.4/0.5); `hp` / `experience` need 0.8.
Status: verified live, including a locked unit (`reasons 0x402`) forced through, cost deducted,
unit instantly in the slot.

```lua
se.modify.recruit(1, "3k_dlc04_unit_wood_imperial_gate_guards",
                  { source = "any", free = true, slot = 5, hp = 50 })
```

#### `se.modify.replace(cqi, slot_index, unit_key [, opts]) -> ok, message`

Shorthand for `se.modify.recruit` with `opts.slot = slot_index` and `opts.replace = true`.
It writes those two keys **into the table you pass**, so do not reuse one `opts` table for a
later `recruit` call unless you want the slot and the replace flag to come along.

#### `se.modify.disband(cqi, slot_index) -> ok, message`

Empty a retinue slot. Slot 0 is refused
(`refusing to disband the commander's own slot`); an already empty slot is refused.

This is what the UI's Disband does: the slot's recruit command with an empty unit key, which the
engine resolves to the slot's "empty" item. (The engine's `CCQ_DISBAND_UNIT` refuses
retinue-slot units, so it is not used.) Native `se_recruit_unit`. Since 0.10. Status: verified
live.

#### `se.modify.unit_strength(cqi, slot_index, percent) -> ok, message`

Set the unit's strength to `percent` of full strength. Native `se_unit_strength_set`, since 0.8.
Saved with the campaign. Status: verified live (50% set, the stock getter agreed).

#### `se.modify.unit_experience(cqi, slot_index, level) -> ok, message`

Set the unit's chevron level (the example scripts use 0..9; the API does not range-check).
Native `se_unit_xp_set`, since 0.8. Status: implemented, confirmed by the stock getter and the
retinue panel; the engine's own setter was never traced.

---

### 3.3 Faction progression, Three Kingdoms and emperor seats

"Emperor" here means: the faction reached its top progression level and took a **world-leader
seat**. Vanilla `3k_campaign_progression.lua` plays the Three Kingdoms movie once three seats
exist. Seat count and maximum come from the engine's world-leader manager (3 in vanilla).

#### `se.query.faction(key) -> table | nil, message`

`{ key, level, max_level, level_key, is_world_leader, world_leader_regions, is_human, is_dead
[, engine_level, engine_max, engine_level_key, engine_leader, locked | engine_error]
[, leaders] }`.

`leaders` is a string from the DLL of the form `used/total` seats (the module parses the total out
of it). Natives: `se_faction_progression_get`, `se_world_leaders` (optional). Since 0.8.
Status: verified live.

#### `se.query.world_leaders() -> { faction_key, ... } | nil, message`

Stock `is_world_leader()` over every faction in the world.

#### `se.modify.faction_progression(key, level) -> ok, message`

Raise a faction to progression `level` through the engine's own unlock + process path (prestige
thresholds are rewritten, then the engine's own level transition runs, applying the level's
effects and firing its events). At the maximum level the engine also tries to grant a
world-leader seat.

Native `se_faction_progression_set`. Since 0.8. Status: verified live (three factions raised;
vanilla script logged "Emperor Seat Established" for two of them).

Known limitation: factions on a Han-loyal "governor" progression group fail the engine's seat
eligibility check and get the level but no seat — use `se.modify.world_leader` (or leave
`force_three_kingdoms`'s `bypass` default) for them.

#### `se.modify.world_leader(key) -> ok, message`

Grant a world-leader seat directly, bypassing the eligibility check. Needs a free seat and a
capital. Native `se_world_leader_force`. Since 0.10. Status: verified live.

#### `se.modify.force_three_kingdoms([opts]) -> ok, message`

Fill the world-leader seats: forced factions first, then the highest-progression living factions
that are not banned. Each pick is raised to its maximum progression level.

| option | type | default | meaning |
|---|---|---|---|
| `forced` | list of faction keys | `{}` | must take a seat, in order |
| `banned` | list of faction keys | `{}` | never auto-picked |
| `include_human` | boolean | `false` | allow the human faction to be auto-picked (`forced` always applies) |
| `seats` | number | the engine's maximum (3) | number of seats to fill |
| `bypass` | boolean | `true` | when the engine refuses a seat after the level change, seat the faction anyway through the world-leader manager |

Returns a multi-line report. Refuses with
`would need N seats but only M exist` when the picks exceed the seat count, and with
`forced faction '<key>' does not exist`.

Natives: `se_faction_progression_set`, `se_faction_progression_get`, plus
`se_world_leader_force` for the bypass. Since 0.8/0.10. Status: verified live end to end — the
vanilla script logged "We Have Three Kingdoms!" and played the movie. **Save persistence of the
seats has not been tested.**

```lua
se.modify.force_three_kingdoms{
    forced = { "3k_main_faction_kong_rong" },
    banned = { "3k_main_faction_dong_zhuo" },
}
```

#### `se.modify.emperor_policy(policy) -> ok, message`

Persistent steering of who may become emperor. `policy = { forced = {...}, banned = {...} }`, or
`nil` to clear.

- The policy string is saved with `cm:save_named_value("se_emperor_policy", ...)`.
- It installs a `FactionTurnStart` listener (needs `se.core`): banned factions sitting one step
  below their maximum level get `lock_progression_level_changes()`; forced factions are promoted
  to their maximum level while seats are free.

Status: implemented; **the turn-start listener has not been observed across a turn yet.**

#### `se.load_emperor_policy() -> ok, message`

Restore the saved policy and re-install the listener. Call it from a `LoadingGame` / first-tick
hook. Returns `false, "no saved policy"` when nothing was stored.

---

### 3.4 Experience and skill points

Engine background: a character's experience lives in their details block; adding experience runs a
rank loop that awards skill points per rank. The engine's own "scaled" path multiplies the amount
by the character's and the faction's experience-gain modifiers (effect id 385 / `0x181`).

#### `se.query.character_xp(cqi) -> table | nil, message`

`{ experience, rank [, engine_xp, engine_rank, max_rank, skill_points] [, dump] }`.
The engine fields need `se_char_rank_get` (0.12); with only the older `se_char_xp_get` you get
`engine_xp` and a raw `dump` string.

#### `se.query.skill_points(cqi) -> number | nil, message`

Unspent skill points. Returns `nil, "native se_char_rank_get is not available"` on a DLL older
than 0.12.

#### `se.modify.character_add_xp(cqi, n [, scaled]) -> ok, message`

Add experience. `n` must be a positive number.

- `scaled` falsy (default): add **exactly** `n`; rank-ups are processed by the engine's own rank
  loop afterwards.
- `scaled = true`: go through the engine's scaled path, which multiplies `n` by the character's
  and faction's experience-gain modifiers.

Native `se_char_xp_add`. Since 0.12. Saved with the campaign. Status: verified live (raw add of
100 moved 373 -> 473; the stock `add_experience(1000, 0)` applied 1740 on the same character,
which is the scaling this flag avoids).

#### `se.modify.skill_points(cqi, n) -> ok, message`

Set the unspent skill-point counter (0..100). Native `se_skill_points_set`. Since 0.12.

Known gap: there is no "grant skill by key" — skills are not CEOs outside Nanman and the skill
allocation executor has not been wrapped.

#### `se.query.faction_effect_value(faction_key, effect_id) -> number | nil, message`

The faction-level value of an engine effect id (a float). Native `se_faction_effect_value`,
since 0.12.

#### `se.query.faction_xp_gain_percent(faction_key) -> number | nil, message`

Shorthand for effect id `se.EFFECT_CHARACTER_XP_GAIN` (385 / `0x181`), the faction's character
experience gain percentage. This is the **faction-level part only**; a character's own effects are
not included.

There is no effect id -> key map yet; `385` is the only id the API names.

---

### 3.5 Assignments

#### `se.query.assignment(cqi [, dump]) -> table | nil, message`

`{ key, state, rounds, idle [, province, state_index, transition_round] [, engine] }`.

- `key` is the assignment record key, `state` the engine's state string, `rounds` the stock
  `rounds_until_state_transition()` (an idle assignment stores `0xffffffff`, so this reads as a
  huge number), `idle` the stock `is_idle_assignment()`.
- `province` (the key of the province the assignment is performed in, `""` for idle assignments),
  `state_index` and `transition_round` need the native `se_assignment_info` (0.11).
- `dump == true` adds a raw `engine` dump when `se_assignment_dump` exists.

Returns `nil, "character has no active assignment"` when there is none. Status: verified live
(province read as `3k_main_province_yingchuan` on a real assignment).

There is no modify side for assignments.

---

### 3.6 Campaign AI personality

#### `se.query.cai_personality(faction_key) -> { key, record_key } | nil, message`

The faction's current CAI personality. **Must run where `cm:can_modify()` is true** (the CAI
manager comes from `cm:modify_campaign_ai()`); from the console, wrap it in `se.on_model`.
Native `se_cai_personality_get`, since 0.12.

#### `se.modify.cai_personality(faction_key, personality_key) -> ok, message`

Swap an AI faction's personality to any `cai_personalities` key. Native `se_cai_personality_set`.
Since 0.12 (corrected in 0.13/0.14/0.15). Status: **verified live, and persistence across
save / restart / load verified.**

```lua
se.on_model("read personality", function()
    ModLog(se.dump(se.query.cai_personality("3k_main_faction_dong_zhuo") or {}))
    return true, "read"
end)
se.modify.cai_personality("3k_main_faction_dong_zhuo",
                          "3k_cai_personality_tao_qian_early_hard")
```

---

### 3.7 Faction potential

The AI handicap rating: a faction's potential is `base + bonus + roll` (0 for human factions) and
selects handicap effect rows from the database.

#### `se.query.faction_potential(key) -> { value, base, bonus, roll } | nil, message`

Native `se_faction_potential_get`, since 0.12.

#### `se.modify.faction_potential(key, value) -> ok, message`

Set an AI faction's potential (range **-100..150**) by rewriting its base and re-applying the
handicap effects through the engine. Native `se_faction_potential_set`. Status: **verified live,
persistence verified** (103 -> 120 held across save / restart / load).

---

### 3.8 Buildings and region slots

Slot indices are positions in the region's `slot_list()`. All of these are model-thread queued.
Since 0.19; forced construction since 0.23.

#### `se.query.region_slots(region_key) -> list | nil, message`

One row per slot:
`{ index, name, type, has_building [, building, chain, health]
[, engine_key, engine_health, can_damage, engine] }`.

#### `se.query.building_candidates(region_key, slot_index [, opts]) -> { level_key, ... }`

Building levels the slot can hold.

- default: every level the slot's current chain set offers, **blocked ones included**;
- `opts.only_valid = true`: only what the UI would let you build right now. This list goes
  **empty as soon as the slot has any blocking reason**, which is normal, not an error;
- `opts.all_chains = true`: every chain the slot can hold.

Native `se_slot_candidates`. Status: verified live.

#### `se.modify.building_damage(region_key, slot_index, percent) -> ok, message`

Damage the building by `percent`. Only works on buildings the database marks as damageable.
Native `se_slot_damage`. Status: verified live (100 -> 60).

#### `se.modify.building_repair(region_key, slot_index [, opts]) -> ok, message`

`opts.free = true` repairs directly with no cost; the default issues the engine's repair command
(charges like the UI). Native `se_slot_repair`. Status: verified live (60 -> 100 free).

#### `se.modify.building_destroy(region_key, slot_index) -> ok, message`

Native `se_slot_destroy`. Status: verified live.

#### `se.modify.building_construct(region_key, slot_index, level_key [, opts]) -> ok, message`

Start construction of `level_key` in the slot. An **upgrade or a conversion is the same call with
the target level key**.

| option | type | default | meaning |
|---|---|---|---|
| `force` | boolean | `true` | ignore the engine's blocking reasons (cost, requirements, siege). `force = false` behaves like the UI button. |
| `any_chain` | boolean | `true` | when the key is not an upgrade of the current building, retry the lookup across every chain the slot can hold |
| `free` | boolean | `false` | zero the cost; anything the treasury still lost is refunded afterwards |
| `turns` | number | engine value | construction time in turns |
| `complete` | boolean | `false` | shorthand for `turns = 1` |
| `pay_to_complete` | boolean | `false` | also issue the engine's pay-to-complete-next-turn command |

Returns a report string (`...; refunded N` / `pay_to_complete -> ...`). Native
`se_slot_construct` (+ `se_slot_pay_to_complete`).

Status and known limitations:

- verified live: forced free upgrade of a city and of a resource building (cost zeroed, turns
  overridden to 1) **completed at the next turn start**;
- a forced **new** building issued into an empty slot in the same province did **not** complete:
  after the turn the construction item had been dropped. Both entries carried the "constructions
  in progress in the province >= the province construction limit" reason bit. Working theory: turn
  processing keeps only `limit` constructions and discards the rest. Force a build while the
  province is under its limit;
- there is no true same-turn completion: `turns = 1` completes at the next turn start;
- right after issuing, the slot still reports the old building — the construction sits in the
  slot's manager until it completes;
- the call refuses when a construction is already in progress in that slot.

---

### 3.9 Alliances and coalitions

#### `se.query.alliances() -> list | nil, message`

`{ { cqi, name, members = { faction_key, ... } [, engine] }, ... }` over the world's alliance
list. `name` needs the native `se_alliance_info` (0.20).

#### `se.modify.alliance_name(cqi, text [, mode]) -> ok, message`

Rename an alliance / coalition. `mode` is `"inline"` (default) or `"pointer"` — two ways of
storing the string on the engine object; `"inline"` is the likelier serialised field.
Native `se_alliance_name_set`. Since 0.20.

Status: applied live and still in place after a turn; **save persistence not yet tested** — check
it with a save/load before shipping it in a mod.

```lua
for _, a in ipairs(se.query.alliances() or {}) do
    for _, m in ipairs(a.members) do
        if m == "3k_main_faction_yuan_shao" then
            se.modify.alliance_name(a.cqi, "Yuan Shao's Grand Coalition")
        end
    end
end
```

---

### 3.10 Effect bundles

An effect list is given as
`{ { effect = "<effects key>", scope = "<campaign_effect_scopes key>", value = <number> }, ... }`.
Every row needs all three fields, and the list must be non-empty, otherwise the call returns
`effects must be a non-empty list of {effect=, scope=, value=}` or
`effects[i] needs effect (string), scope (string) and value (number)`.

In all three functions `faction_key` only provides the model to the native; it defaults to
`cm:get_local_faction()`.

#### `se.query.effect_bundle(bundle_key [, faction_key]) -> { count, dump } | nil, message`

Engine-side inspection of an `effect_bundles` record: the number of effect entries and a raw
dump. Native `se_effect_bundle_info`. Since 0.21.

#### `se.modify.effect_bundle_define(bundle_key, effects [, faction_key]) -> ok, message`

Replace the effect list of an **existing** `effect_bundles` record for this game session. Every
later stock `apply_effect_bundle(bundle_key, ...)` on any holder (faction, character, region,
force, ...) carries the new effects. Bundles applied *earlier* keep what they had until they are
removed and applied again.

**Not saved.** Define again after every load (first tick) and re-apply. Native
`se_effect_bundle_define`. Since 0.23. Status: verified live (define followed by the stock
`apply_effect_bundle` returned cleanly).

#### `se.modify.effect_bundle_restore(bundle_key [, faction_key]) -> ok, message`

Put the stock effect list back. Native `se_effect_bundle_restore`. Since 0.23.

#### `se.modify.effect_bundle_apply_custom(faction_key, bundle_key, effects [, turns]) -> ok, message`

Apply `bundle_key` to the faction with its **own** per-instance effect list (the engine's custom
list); the database record and every other holder of the bundle are untouched. `turns = 0` (the
default) is permanent.

Native `se_effect_bundle_apply_custom`. Since 0.23 (corrected through 0.23.3). Status: verified
live — the instance rebuilt with one active effect, the faction's bundle count went up and
`has_effect_bundle` reported true. **Save persistence of the custom list has not been tested.**

Open behaviour: a defined `+50 gdp_mod_all / faction_to_region_own` bundle did **not** move
`projected_net_income`, before or after a turn. Whether that scope reaches the income computation
is unresolved — test the effect you intend to use.

```lua
se.modify.effect_bundle_apply_custom("3k_main_faction_cao_cao",
    "3k_dlc05_effect_bundle_yellow_turban_economy",
    { { effect = "3k_main_effect_characters_experience_bonus",
        scope  = "faction_to_character_own_factionwide_unseen", value = 25 } }, 5)
```

---

### 3.11 Diplomacy attitude

#### `se.query.attitude(a, b) -> { standing, stock } | nil, message`

Standing of faction `a` towards faction `b`. `stock` is the stock
`diplomatic_standing_with()`; `standing` is the engine's own computation, present when the native
`se_attitude_get` exists. The attitude is computed from CAI components, not stored as a number.

#### `se.modify.attitude(a, b, level) -> ok, message`

Fire the engine's attitude-change event from `a` towards `b`.
`level` = 1 / 2 / 3 (small / medium / large positive) and -1 / -2 / -3 (negative). The exact
amounts come from the database's attitude-event records, so a mod retunes those rows rather than
passing a number here.

Native `se_attitude_change`. Since 0.22. Status: the read-only side was verified live in a batch
test; **the change call is implemented but not confirmed live**. Per-treaty-component evaluation
bias is not implemented.

---

### 3.12 Income lines

Script-side, not an engine income category: the engine's income breakdown has no slot for an
arbitrary named line, so the amount is paid into the treasury at the faction's turn start and does
**not** appear in the income breakdown or in `projected_net_income`.

#### `se.modify.faction_income(faction_key, amount [, label]) -> ok, message`

Add or replace a per-turn income line. `label` defaults to `"se_income"`; `amount = 0` (or `nil`)
removes the line. Negative amounts are subtracted from the treasury.

Needs `se.core` — it installs a `FactionTurnStart` listener that pays the sum of that faction's
lines with `increase_treasury` / `decrease_treasury`. The lines are saved with
`cm:save_named_value("se_income_lines", ...)`; the listener is not, so call
`se.load_income_lines()` after a load.

This function does not go through `se.on_model` and does not itself check multiplayer.

Since 0.22 (module-side; no native needed). Status: verified live — a 1500 line was paid over a
turn.

#### `se.query.faction_income(faction_key) -> { [label] = amount, ..., total = n }`

The lines currently known to this session.

#### `se.load_income_lines() -> ok, message`

Restore saved lines and install the listener. `false, "no saved income lines"` when nothing was
stored.

---

### 3.12a Horde income (DLL 0.28+; on by default since 0.37.0-beta.4 / stable 0.40.0)

`gdp_abs` effects (bonus value `region_gdp`) only reach the treasury through regions. The DLL
hooks the engine routine that recomputes a faction's income categories (always, unless
`script_extender.cfg` says `horde_income=0`; before 0.37.0-beta.4 it needed `horde_income=1`) and
adds, 1:1, every `gdp_abs` value found on the **faction itself**
(scopes such as `faction_to_faction_own`) and on **each military force it owns**
(`force_to_force_own`, what horde building bundles use). `gdp_mod` values are ignored.
Ordinary factions carry no such values (only percentage entries, which are ignored), so
without a mod that grants them the amount is 0 and nothing changes.
`horde_income_category` selects the category: `0` TAXES, `1` MINING (the default since
0.37.0-beta.4; `0` before), `2` TRADE, `3` MILITARY_FORCE. **MINING is unused in 3K** (never computed, no row in the stock treasury
panel, but part of the totals), so a UI mod can show it as its own line: bind a treasury row to
`CcoFactionEconomy` / `VaryingRegIncomeDetailsSum("MINING")` and label it with
`Loc("<your key>")` (= `campaign_localised_strings_string_<your key>`), which other mods can
translate. Both settings are part of the multiplayer version lock (their effective values, so
an absent key and an explicit default give the same tag). Verified live on 0.28.3 /
0.29.1 (a 150 `gdp_abs` army bundle raised `projected_net_income` by 150); receiving it at turn
end and a region-less faction are not yet confirmed.

#### `se.query.horde_income_hook()` (0.30.1+)

Returns `{ enabled = boolean, category = 0..3, category_name = "TAXES" | "MINING" | "TRADE" |
"MILITARY_FORCE" }`, or `nil, message` on an older DLL. `enabled` is the state of the detour in
this process (cfg flag set and the hook installed), not the cfg text. Read-only, no model
access, usable anywhere.

```lua
local h = se.query.horde_income_hook()
if not (h and h.enabled and h.category == 1) then
	ModLog("horde income is off or points elsewhere: check horde_income / horde_income_category in script_extender.cfg (DLL older than 0.37.0-beta.4 needs horde_income=1, horde_income_category=1)")
end
```

#### `se.query.faction_force_gdp(faction_key)` (0.28+)

Returns `{ total = number, entries = string }`: the `gdp_abs` total the hook would add for that
faction and one line per contributing entry (`faction` or `force[i]`). Works with the hook off.

### 3.13 Menu build number

The main menu's build-number strings. Process-local UI state, not campaign state: **no model
thread, no multiplayer check, nothing saved.** The DLL can also apply these at injection time from
`script_extender.cfg` next to it (`build_number=`, `build_number_short=`, `build_modified=`).

#### `se.query.build_number() -> { build, short, modified } | nil, message`

#### `se.modify.build_number(build, short, modified) -> ok, message`

`""` or `nil` for `build` / `short` keeps the existing string; `modified` is the "build modified"
flag (pass `nil` to keep it). Natives `se_build_info_get` / `se_build_info_set`. Since 0.16
(corrected in 0.17, deferred apply in 0.18). Status: verified live through the mod manager.

```lua
se.modify.build_number(nil, "1.7.2.0 SE 1.0", nil)
```

---

### 3.14 Auto-resolve

Three separate mechanisms, in increasing order of intrusiveness:

1. **Tunables** — the auto-resolver's own constants (`autoresolver_*` campaign variables, ~85 of
   them, including 7 `autoresolver_duel_*`). Session-only.
2. **Read-out** — the pending battle's context and the engine's prediction.
3. **Plans** — a per-battle instruction the DLL applies to the freshly computed result.

Everything here applies to battles a human player is involved in (every machine of a multiplayer
game evaluates the same handler; see the multiplayer rules in §1).

#### What each DLL version actually applies

| Plan field | Status |
|---|---|
| `winner` | applied by the engine hook; **use 0.26.2 or later** (verified live there: winning alliance index and per-side result ids are written; 0.26.0 / 0.26.1 had no effect on the winner) |
| `casualties` | applied since 0.26; **verified live** (a 10% cap turned a predicted 798 -> 563 into 798 -> 730, and the army kept about 90% after the battle) |
| `duels` | applied since **0.27**; **verified live** (a forced winner flipped the game log from "Proposer Won Duel" to the other character). `default = "none"` removes every duel without a rule, `max` trims the list, a pair the engine did not roll is appended when both hero unit keys are known |
| `duels[i].fate` | accepted and validated — **not applied**: the campaign rolls the loser's wound or death afterwards |
| `bias` | accepted, clamped and stored — **not applied** by any current version |
| `refresh_prediction` | applied (re-runs the engine compute routine so the panel prediction matches) |

Further limitations of the result rewrite:

- **A side made of more than one army record is skipped** (the element size of that vector is not
  mapped). A defender built from two forces was only partly covered in 0.26.0; 0.26.1 skips
  neutral sides entirely and moves a summary only by the delta of the units it actually rewrote.
- Single-man records (characters) are never made *worse* by casualty or winner rules.
- The pre-battle panel's casualty bar does not refresh after a recompute (the UI caches the
  prediction it read when the panel opened). Cosmetic.
- Plans are keyed to the pending-battle object that existed when the plan was set; every other
  battle is ignored.
- The engine's own auto-resolve simulation is deterministic: it re-runs on the auto-resolve click,
  which is why the DLL hooks the computation instead of rewriting a stored prediction.

#### `se.query.autoresolver_variable(key) -> number | nil, message`

One campaign variable. Only keys starting with `autoresolver_` are accepted.

#### `se.query.autoresolver_variables() -> { key = value, ... } | nil, message`

Every `autoresolver_*` key with its current value.

#### `se.modify.autoresolver_variable(key, value) -> ok, message`

Retune one constant for this session. **Not saved** — call it again after a load. Within a
session, the module remembers the value and re-applies it at the local player's
`FactionTurnStart` (needs `se.core`), because the engine rebuilds the variable array when its
per-round overrides change.

Native `se_ar_variable_set`. Since 0.24. Status: verified live (reads matched the database
values, writes took).

```lua
se.modify.autoresolver_variable("autoresolver_duel_base_chance", 1.0)  -- vanilla 0.5
se.modify.autoresolver_variable("autoresolver_duel_max_limit", 2)      -- vanilla 6
```

#### `se.modify.autoresolver_variables_reset() -> ok, message`

Every variable back to what the engine had before the first script write.

#### `se.query.autoresolve_prediction() -> table | nil, message`

The engine's own prediction for the pending battle (what the pre-battle panel shows). Keys, all
flat in one table:

`available` (boolean), `pending_battle`, `night`, `result_index`, `results`,
and per side (`attacker` / `defender`): `<side>_prediction` (one of `close_victory`,
`decisive_victory`, `heroic_victory`, `pyrrhic_victory`, `draw`, `close_defeat`,
`decisive_defeat`, `crushing_defeat`, `valiant_defeat`), `<side>_prediction_id` (0..8 in that
order), `<side>_casualties_percent`, `<side>_strength_share`, `<side>_men_before`,
`<side>_men_after`, `<side>_men_lost`.

Native `se_ar_prediction`. Since 0.24.

#### `se.query.pending_battle() -> table | nil, message`

```
{ active, battle_type, is_siege, is_ambush, is_night, human_involved,
  local_player_involved,
  attacker = { faction, is_human, is_local_player, strength,
               forces     = { { cqi, general_cqi, units }, ... },
               characters = { { cqi, faction, template, rank }, ... } },
  defender = { ...same... },
  prediction = se.query.autoresolve_prediction() }
```

Returns `nil, "no pending battle interface"` when there is none. Uses only stock interfaces plus
the prediction native.

#### `se.modify.autoresolve_plan(plan [, ctx]) -> ok, message`

Store a plan for the current pending battle. `ctx` defaults to `se.query.pending_battle()`;
the call refuses with `refused: no pending battle with a human player` when `ctx.human_involved`
is not true. Since 0.30 the test is "a human is involved", not "the local player is involved",
so that every machine of a multiplayer game sets the same plan for the same battle; the model
step itself still goes through `se.on_model` and is refused outside a model callback in
multiplayer.

Plan shape (schema, not runnable code — `|` means "one of"):

```text
plan = {
  winner = "attacker" | "defender" | nil,       -- force the winner
  casualties = {                                 -- per side
     attacker = { scale = 1.0, max = 1.0 },      -- scale 0..10, max 0..1 (fraction lost)
     defender = { scale = 1.0, max = 1.0 },
  },
  bias  = { attacker = 1.0, defender = 1.0 },    -- 0.1..10, STORED BUT NOT APPLIED
  duels = {                                      -- applied since 0.27
     max = 6,                                    -- 0..16, trims the duel list
     default = "vanilla" | "none",               -- "none" drops every duel without a rule
     pairs = { { a = cqi, b = cqi, happen = true, win_chance = 0.5, winner = cqi,
                 fate = "kill" | "wound" | "spare" | "flee",   -- fate is NOT applied
                 a_key = "unit key", b_key = "unit key" } },   -- only to create a missing duel
  },
  refresh_prediction = true,                     -- false skips the immediate panel recompute
}
```

Validation errors: `plan must be a table`,
`plan.winner must be 'attacker', 'defender' or nil`,
`plan.duels.default must be 'vanilla' or 'none'`,
`plan.duels.pairs[i] needs character cqis a and b`,
`plan.duels.pairs[i].fate must be kill, wound, spare or flee`.
Numeric values outside their range are clamped rather than rejected.

A pair naming two characters the engine did not pair up is **created**, but only when both hero
unit keys are known. The module resolves them for you when the character's force holds exactly
one `_hero_` unit; when it holds two or more (a force with several generals), pass `a_key` /
`b_key` yourself. `win_chance` is a probability, so in multiplayer derive the roll from
`ctx.seed` (see §1) and never from `math.random`.

Native `se_ar_plan_set` (+ `se_ar_recompute`). Since 0.24; winner and casualties applied since
0.26.2, duels since 0.27.

#### `se.modify.autoresolve_plan_clear() -> ok, message`

Drop the stored plan (vanilla behaviour again). No model queue, no multiplayer check.

#### `se.query.autoresolve_plan() -> plan, encoded`

The plan table as it was given, plus the encoded string the DLL currently holds.

#### `se.autoresolve.set_handler(fn) -> ok, message`

Install a handler called on every `PendingBattle` that involves **a human player** — on every
machine of a multiplayer game, not only the one whose battle it is. `fn(ctx)` receives the
`se.query.pending_battle()` table and returns a plan table, or `nil` for vanilla behaviour. Needs
`se.core`. The listener clears any previous plan first, and a second listener drops the plan
again on `BattleCompleted`. Calling it again replaces the handler (`"handler replaced"`).

**Never branch on `ctx.*.is_local_player`** (or `cm:get_local_faction()`) inside the handler: it
is true on one machine and false on the other, so the two would store different plans and desync.
Branch on `is_human` and faction keys, and seed any chance from `ctx.seed`.

#### `se.autoresolve.clear_handler() -> ok, message`

Forget the handler and clear the stored plan. The listeners stay registered but do nothing.

#### Duel power (DLL 0.42.0-beta.5+)

An auto-resolved duel is decided by **duel power** alone: each hero's power is
`int(melee + missile)`, built from the hero unit's `main_units.melee_cp` / `missile_cp`
(about 165 for strategists up to 815 for Lu Bu), the rank bonus
(`unit_stats_land_experience_bonuses`, +10 per level), the top ten special abilities'
`additional_melee_cp` / `additional_missile_cp` (weighted 1.0, 0.9 … 0.1) and a condition
factor. Once two duelists are paired, no dice are rolled: the stronger one wins, the attacker
wins a tie, and a gap above `autoresolver_duel_refuse_variable` (150) means the duel is refused.
Equipment (CEOs) is never read. The functions below add a per-character bonus to that power
**before** the engine decides, so the vanilla rules still apply. The bonus works in every
auto-resolve, AI battles included, and in both the panel prediction and the result on click.

#### `se.modify.duel_power_bonus({ [character_cqi] = bonus, ... }) -> ok, message`

Merge bonuses into the DLL's table: an entry stays until you change it, and a bonus of `0` removes
it. Values are clamped to ±2000. The table is **not saved**, so set it again at the first tick of
every load, and clear it first: it survives loading another save in the same session.
It changes the simulation, so in multiplayer call it from model events only (first tick, turn
start, `CharacterCeoEquipped`, `PendingBattle` …) with values that are the same on every machine.
Outside a model callback it is refused in multiplayer and queued in single player. Refused when
`script_extender.cfg` has `duel_power_hook=0` or `autoresolve_hooks=0`.

#### `se.modify.duel_power_bonus_clear() -> ok, message`

Empty the table.

#### `se.query.duel_power_hook() -> { installed, entries } | nil, message`

`installed` is false when the hook is switched off in `script_extender.cfg`; `entries` is the
number of characters that have a bonus; `formula` (0.43+) is true while a stats formula is
active. Every adjusted duelist is logged
(`ar_duel_candidates: character <cqi> duel power vanilla <old> -> <new> [breakdown]`).

#### `se.modify.duel_power_formula(f) -> ok, message` (DLL 0.43.0+)

Replace (or blend) vanilla duel power with a weighted sum of the duelist's **own stats**. These are
the numbers on the unit card, with attributes, skills, equipment and effects already applied. The
engine builds them for every auto-resolve duelist, and the DLL reads them from that object; no
engine code is called.

```lua
se.modify.duel_power_formula({
    vanilla = 0,          -- weight on the vanilla power (0 = replace it, 1 = keep it and add)
    abilities = 1,        -- weight on the vanilla special-ability term (top 10, 1.0 .. 0.1)
    health = 0.02,        -- per current hit point
    health_scale = true,  -- multiply the stat and ability terms by the health fraction
    refuse = 600,         -- optional: duel refusal gap while the formula is active
    stats = { stat_melee_damage_base = 0.5, stat_melee_damage_ap = 1, stat_armour = 4,
              stat_melee_defence = 10, stat_charge_bonus = 0.5 },
})
```

`power = vanilla × old + (Σ weight × stat + abilities × ability_cp) × (health % if health_scale)
+ health × current_hp`, then the `duel_power_bonus` of the character is added. The stat value is
the card value: base + modifier. For example, Cao Cao in one overhaul has armour 65, Melee Evasion
(`stat_melee_defence`) 10 + 5, melee damage 885 + 274, AP 188, charge 161 + 50 and 18,360 hit
points.

- Stat keys are the exe's own names; `se.query.duel_stat_names()` lists them.
- Stats outside a unit's stat block (e.g. `stat_weapon_damage`) are refused.
- Weights are limited to ±10000.
- Duelists more than the refusal gap apart never fight, and **the game then shows the weaker
  hero as the winner**. The gap is `autoresolver_duel_refuse_variable`, 150 by default. Stat-based
  power spreads heroes much wider than the fixed values, so pass `refuse` (the 190E Duels tab uses
  600). The duel roll reads its own copy of the variables, which
  `se.modify.autoresolver_variable` does not reach. That is why the gap travels with the formula:
  the DLL sets it in the roll's copy for the duration of each duel roll only and puts the game's
  value back right after. The model is unchanged afterwards, which multiplayer requires; 0.43.0-beta.1
  left it changed and desynced at the first battle.
- Like `duel_power_bonus`, the formula changes the simulation: in multiplayer it runs only from
  model callbacks with the same values on every machine, and it is not saved.

#### `se.modify.duel_power_formula_clear() -> ok, message`

Back to vanilla duel power (the bonuses stay).

#### `se.query.duel_power_formula() -> string`

The active formula as sent to the DLL (`""` when none).

#### `se.query.duel_stat_names() -> { stat_armour = 3, ... } | nil, message`

Every stat of a unit's stat block, by the exe's names.

#### `se.query.duel_last(cqi) -> string | nil`

The character's most recent auto-resolve duel candidacy with the formula breakdown, e.g.
`vanilla 1300 -> 1652 [melee_damage_base=1159.3 melee_damage_ap=188 armour=65 ... hp=18360/18360]`.
Use it to tune weights: open a pre-battle panel, then read the heroes' lines.

---

### 3.15 Performance counters and the profiler

Read-only measurement of the DLL's own caches and of the game process. Nothing here changes what
the game computes, nothing needs the model thread, and everything is safe in multiplayer. What
the caches themselves do, and the `script_extender.cfg` keys that switch them, is §4b.

#### `se.query.perf() -> table | nil, message`

The DLL's counters, parsed from the native's `k=v;` string into numbers (plus `installed`, a
boolean). The set of keys grows with the DLL; the ones worth reading:

| Key | Meaning |
|---|---|
| `installed` | the UI recruit-list cache detour is in place |
| `ttl_ms` | its time-to-live, from `ui_recruit_cache_ms` |
| `perm_mode`, `ai_mode` | the `recruit_perm_cache` / `ai_recruit_cache` setting actually in effect |
| `hits` / `misses` | UI recruit-list queries answered from the cache / computed by the engine |
| `passed_through` | queries from the AI and other non-UI callers, which are never cached |
| `entries`, `miss_new`, `miss_expired`, `miss_state`, `stamp_failed` | why a query missed |
| `perm_built`, `perm_shared`, `perm_same`, `perm_diff` | unit-permission tables built, reused, and the result of the self-check (`recruit_perm_cache`) |
| `ai_*` | the diagnostic AI-planner list cache (`ai_recruit_cache`) |
| `file_probes`, `file_probes_skipped` | loose-file lookups seen and answered without a system call (`file_probe_cache_ms`) |
| `dip_*` | diplomacy evaluation counters, only when `diag_diplomacy=1` |

Native `se_perf_stats`. Since 0.32 (`perm_*` 0.34, `file_probes*` 0.35). Status: verified live.

```lua
local p = se.query.perf()
if p then ModLog(("recruit list: %d hits / %d misses, %d AI"):format(p.hits, p.misses, p.passed_through)) end
```

#### `se.profile.start(seconds [, delay_seconds [, label]]) -> ok, message`

Start the sampling profiler: every 1 ms it walks the six busiest threads, for `seconds`
(1..120, default 20) after a delay of `delay_seconds` (default 3, so you can close the console
first). `label` (default `"run"`) names the report. One run at a time; it costs a few percent of
frame time while it runs and reads only.

Two files are written when the run ends, into `profiles\` **next to the DLL's version folder**
(that is `dll\profiles\` in the mod-manager layout, so the manager's cleanup of old version
folders does not take them):

- `profile_<label>.txt` — per thread, the functions the CPU was in (`self`) and the functions on
  the stack, as Ghidra addresses;
- `profile_<label>.folded.txt` — folded call stacks, the input for
  `tools/profile_tree.py <folded.txt> [--min 2] [--depth 14] [--callers <fn>] [--minus <baseline>]`.

Native `se_profile_start`. Since 0.31 (report folder 0.32.2).

#### `se.profile.stop() -> ok, message`

End the running profile now; its reports are still written.

```lua
se.profile.start(50, 4, "endturn")   -- close the console, then press End Turn
```

---

### 3.16 Script listener diagnostics

Which script listener costs the time. This wraps the event manager, not the engine: behaviour is
unchanged (same arguments, same return values, errors still pass through the game's own `pcall`).
Read-only, not saved, and it lasts until the Lua state is rebuilt — install it again after a load.

#### `se.diag.listeners_start() -> ok, message`

Put a stopwatch around the condition and the callback of every listener registered through
`core:add_listener`, including the ones registered later (`core.add_listener` itself is wrapped).
Needs `se.core` and the natives `se_timer_push` / `se_timer_pop`. Idempotent: a listener is never
wrapped twice. Returns `"timing N listeners (new ones are added as they register)"`.

#### `se.diag.listeners_reset() -> true`

Zero the counters — call it right before the thing you want to measure, e.g. End Turn.

#### `se.diag.listeners_report([top]) -> rows`

Log a per-event summary and the `top` (default 30) most expensive listeners, and return the rows:
`{ event, name, calls, fired, total_ms, condition_ms, callback_ms }`, sorted by total time.
`calls` counts every evaluation of the condition, `fired` the ones whose callback ran.

**Times are inclusive**: a callback that triggers further events carries their listeners' time
too, so the numbers do not add up to a total. Natives `se_timer_push` / `se_timer_pop`.
Since 0.35.

```lua
se.core = core
se.diag.listeners_start()
se.diag.listeners_reset()            -- then press End Turn
se.diag.listeners_report(40)
```

#### `se.ui.fix_followup_button() -> ok, message` and `se.modify.followup_propose() -> ok, message`

Game build 1.7.2.0: the first follow-up negotiation popup raised by a vassalisation (the ultimatum
against a faction at war with your new vassal - **MEDIATE PEACE** after taking the Emperor) has a
lit but dead button: the click never reaches its `ProposeDeal` command, and the popup comes back
every turn. `se.ui.fix_followup_button()` installs a `ComponentLClickUp` listener for that button
(`button_accept` inside `diplomacy_followup_negotiation_popup_panel`); on a click it calls
`se.modify.followup_propose()`, which queues the engine's own "propose" command for the deal on
screen. The DLL sends it from the main thread on the next UI update, only if the deal is still
the one that was shown and still waiting, and not when the engine handled the click itself (the
later popups of the same batch do work). The recipient then accepts or refuses as in the
unmodified game; cancel is untouched. Needs `se.core = core`; hand over
`se.UIComponent = UIComponent` so the listener can check the popup. Natives: `se_followup_propose`.
Since 0.41.0.

```lua
cm:add_first_tick_callback(function()
    if type(se) ~= "table" or not se.ui or not se.ui.fix_followup_button then return end
    se.core = core
    se.UIComponent = UIComponent
    se.ui.fix_followup_button()
end)
```

#### `se.diag.diplomacy_trace(on [, include_ai]) -> was_on`

Record every "is this treaty component valid between faction A and faction B" question the
engine asks - the diplomacy screen, an ultimatum popup, a scripted automatic deal, and with
`include_ai` the campaign AI's too - together with the `campaign_diplomacy_groups` tree it
walked to answer it. Read-only. Needs `diag_diplomacy=1` (or `2`) in `script_extender.cfg` when
the DLL is injected; switching the trace on clears the previous recording. Identical questions
are stored once with a count. Since 0.41.0.

#### `se.diag.diplomacy_mark(label)`

Put a time marker between the recorded evaluations (a click, a step of a test).

#### `se.diag.diplomacy_report() -> { path, evaluations, blocked, summary }`

Write `dip_trace.txt` next to the DLL and log the blocked evaluations
(`component | A -> B | flags | reason`). In the file every evaluation is followed by its walk:
`AND` / `OR` / `NOT` lines are group nodes (key, reason), `REQ` lines requirement leaves, `+`
passed and `-` failed, so the first `-` leaf under a `BLOCKED` entry is the rule that refused it.

```lua
se.diag.diplomacy_trace(true)
-- ... open the diplomacy screen, click the button that does nothing ...
local t = se.diag.diplomacy_report()
se.diag.diplomacy_trace(false)
```

---

### 3.17 AI recruitment

Two layers: a **read side** (0.36) that shows what the campaign AI budgets, buys and could have
bought, and a **policy** (0.37) that adds recruitment orders of its own. §4c and §4d are the
narrative versions; this is the reference.

Engine background: the AI's recruitment planner only ever prices **empty** retinue slots, at most
three per character per pass, cheapest first — nothing in the engine replaces a unit that is
already in a slot. Its ranking of units is the database table
`cdir_military_generator_unit_qualities`.

#### `se.query.unit_quality(unit_key) -> rows, best`

The AI's own ranking of a unit, read from the live table (so other mods' rows count):
`rows = { { group, quality, quality_at_max_xp }, ... }`, one row per role group the unit belongs
to; `best` is the highest quality over the groups, or the value in
`se.ai_recruit.quality_override`. An unknown unit gives `{}, 0`.

Results are **cached per unit key for the life of the Lua state**; an override set afterwards
still wins, because it is applied after the lookup. Native `se_unit_quality`. Since 0.36.

#### `se.ai_recruit.trace(on) -> was_on`

Let the DLL copy the AI's recruitment requests after every planning pass. **Off by default**;
switching it off drops whatever was collected. Native `se_ai_recruit_trace`. Since 0.36.

#### `se.query.ai_recruitment() -> passes`

**Drains** what the trace collected since the last call:

```
{ { seq, faction_id, pending,
    requests = { { money, budget2, turn, target,
                   rows = { { id, cost, cost2, kind } } } } } }
```

`money` / `budget2` are what the planner set aside for that request, `rows` the purchases it
priced, `target` the class of the request's target object (hex RVA of its vtable) and
`faction_id` the engine id of the planning faction (`0` when it could not be resolved). Native
`se_ai_recruit_passes`. Since 0.36.

#### `se.ai_recruit.report(faction_key [, opts]) -> report | nil, message`

A read-only audit of every retinue slot of every army of a faction:

```
{ faction, treasury, slots, empty, upgradable, affordable,
  forces = { { cqi, characters = { { cqi, slots = { {
      index, unit, experience, strength, recruiting,
      quality, effective, best = { key, quality, cost, turns }, gap, affordable } } } } } } }
```

`effective` scales the unit's quality towards `quality_at_max_xp` by its experience, the way the
AI's own table values a veteran; `best` is the highest-quality unit of a **shared role group**
the slot could recruit right now with no lock reason; `gap` = `best.quality / effective`, so a
gap above 1 means a better unit is available. `opts.min_gap` (default 1.0) only fills `best` when
the gap reaches it. Since 0.36.

#### `se.ai_recruit.plan(faction_key [, money]) -> orders, info`

What the policy would do this turn. **Changes nothing** — the planning half of the pass, exposed
so you can inspect or edit it.

```
orders = { { op = "recruit" | "replace", force, character, general, slot,
             unit, cost, copies, old, score, old_score, gain }, ... }
info   = { faction, treasury, income, candidates, budget, spent [, skipped ] }
```

Orders come back sorted deterministically (replacements first, then by gain, then by character
and slot — the same order on every machine), already cut to the budget and the per-turn caps.
`money = { treasury, income }` overrides the faction's own numbers for a what-if. Budget =
`min(treasury - reserve, max(0, income) * income_turns, max_spend)`.

Score of a unit = its AI quality (the old unit's scaled up by its experience) x the general's
element weight x `duplicate_penalty` per copy already in that retinue. Since 0.37.

#### `se.ai_recruit.execute(orders) -> done, failed`

Carry the orders out at **normal cost**, through `se.modify.replace` / `se.modify.recruit`.
Returns two counts. Since 0.37.

#### `se.ai_recruit.set_policy(fn) -> true`

`fn(faction_key) -> orders` replaces `se.ai_recruit.plan` as the decision maker of the turn-start
pass; it may call `plan()` and edit the result. `nil` restores the default. Since 0.37.

#### `se.ai_recruit.enable() -> ok, message`

Install the turn-start pass: a `FactionTurnStart` listener that runs for every AI faction that is
not dead. Needs `se.core`. Once per Lua state — **call it again after a load**. Returns
`"listening"`, or `"already listening; enabled"` when it was installed before.

It runs inside the model callback, so it is legal in multiplayer: no local-player branching and
no randomness anywhere in the pass. Both machines must run the same rules, i.e. the same values
in the tables below — ship them as mod content, not as a console edit on one machine.

#### `se.ai_recruit.disable() -> true`

The listener stays registered but does nothing.

#### Tunables

Plain tables; edit them before or after `enable()`.

| Table | Default | Meaning |
|---|---|---|
| `se.ai_recruit.config` | see below | the policy's numbers |
| `se.ai_recruit.element_order[element]` | five lists | units ranked best to worst for a general of that element. An entry is `"element"` or `"element:class"` with class `cavalry` / `infantry`; a unit takes the best entry it matches |
| `se.ai_recruit.element_weight` | `{ 1.30, 1.15, 1.00, 0.90, 0.80 }` | score multiplier by rank in that list. A longer list is spread evenly between the first and the last value |
| `se.ai_recruit.unit_element[key]` | `{}` | element of a unit whose key does not name one |
| `se.ai_recruit.unit_class[key]` | `{}` | `"cavalry"` / `"infantry"` for units the quality table does not know |
| `se.ai_recruit.quality_override[key]` | `{}` | quality for units missing from the table, or a rebalance |
| `se.ai_recruit.max_experience` | `9` | the experience level that counts as fully veteran |

| `config` key | Default | Meaning |
|---|---|---|
| `fill_empty` / `replace` | `true` / `true` | which of the two jobs run |
| `min_gain` | `1.5` | replace only when the new score is at least this times the old |
| `same_role_only` | `false` | a replacement must share a role group with the old unit |
| `min_strength` | `50` | leave units below this strength % alone |
| `max_per_character` / `max_per_force` / `max_per_faction` | `2` / `3` / `8` | orders per turn |
| `reserve` | `1500` | treasury the pass never touches |
| `income_turns` / `max_spend` | `3` / `4000` | the other two budget limits |
| `min_income` | `0` | a faction with a lower projected income does nothing |
| `duplicate_penalty` | `0.92` | score multiplier per copy of the unit already in the retinue |
| `max_copies` | `0` | hard limit of copies of one unit per retinue (0 = none) |
| `log_orders` | `true` | log every executed order with before / after |

#### Helpers

`se.ai_recruit.element_of(key) -> element | nil` — the first `_`-separated word of a unit or
character-subtype key that names an element (`unit_element` overrides).
`se.ai_recruit.class_of(unit_key) -> "cavalry" | "infantry"` — cavalry when any of the unit's
role groups names cavalry (`unit_class` overrides).
`se.ai_recruit.element_factor(general_element, unit_key) -> number` — the weight of the best
entry of that general's list the unit matches, 1 when either side is unknown.
`se.ai_recruit.forces_of(faction_key) -> { { cqi, characters = { { cqi, element } } } }` — the
pass's data source, kept separate so a test or another script can replace it.

---

### 3.18 Crash reporter

Since 0.42.0-beta.1. On by default (`diag_crash=1` in `script_extender.cfg`; `0` turns it off,
`2` adds a continuous activity trace). Nothing here changes the game: the DLL installs an
exception handler that, when the process faults, appends a report to **`se_crash.txt` next to the
DLL** before the game's own crash reporter runs. The report holds the fault as
`Three_Kingdoms.exe+rva` (with the Ghidra address) or `script_extender.dll+rva`, the registers, a
call stack unwound with the modules' own unwind tables, a scan of the stack for return addresses,
**which SE hook or native the faulting thread was inside**, the last 64 SE events of all threads
(hooks entered and left, natives called, `se.crash.mark` texts), and the list of installed hooks.
That is what tells whether a crash is the DLL's or the game's. The game's minidumps still land in
`%APPDATA%\The Creative Assembly\ThreeKingdoms\crash_report\` and `%LOCALAPPDATA%\CrashDumps`.

The handler is first-chance: it also sees an exception the engine catches and survives. Such a
report says so in its last line, and reports are written at most once per faulting address and
at most eight per session. `se_crash.txt` is appended across sessions; delete it whenever you like.

```lua
se.crash.mark("before recruit " .. key)      -- a breadcrumb, 63 bytes kept
local t = se.crash.info()                    -- { installed, level, natives, hooks, reports, ... }
se.crash.selftest()                          -- writes a test report without faulting
```

- `se.crash.mark(text) -> ok, message`
- `se.crash.info() -> table | nil, message`: `installed`, `level`, `natives` (registered),
  `hooks` (installed detours), `reports`, `repeats` (duplicates suppressed), `dropped` (activity
  events lost at level 2), `last_code`, `last_rva`, `scope_overflow`.
- `se.crash.selftest([mode]) -> ok, message`: mode 1 (default) writes a report of the current
  context directly; mode 2 raises a private exception that travels through the handler, which
  dismisses it (the call returns normally; an attached debugger stops on it).

Narrowing a crash down: every hook group has a cfg switch (`autoresolve_hooks`, `horde_income`,
`ai_recruit_hook`, `followup_hooks`, `recruit_perm_cache`, `ui_recruit_cache_ms`,
`file_probe_cache_ms`, `diag_diplomacy`); turn them off one at a time between sessions and keep
the `se_crash.txt` of each. The first two and the two `*_hook(s)` switches are part of the
multiplayer version lock.

### 3.19 Saved values larger than 64 KiB

Since 0.42.0-beta.2. Automatic, nothing to call. The engine keeps at most about 65,536 bytes of
one saved string, and the campaign manager writes the whole `cm.saved_values` store as one string.
Once a campaign's saved values grow past that, the save holds a cut string, and on the next load
every mod's saved values reset at once. With the DLL, `cm:save_named_value` and
`cm:load_named_value` split any string or table that serialises above 60,000 bytes into numbered
savegame entries (`<name>__se_1`, `<name>__se_2`, ...) plus a marker in its own entry, and join
them again on load. That covers `saved_values` and every mod's own named values. Values below the
limit are saved exactly as before, and saves made without the DLL load as before.

A save written with chunking must be loaded with the DLL: without it, a chunked value loads as
empty, which is the same reset that would have happened anyway. `save_chunking=0` in
`script_extender.cfg` turns it off.

```lua
local t = se.saves.info()   -- { installed, enabled, path, chunked_saves, chunked_loads, last_name, last_len }
```

- `se.saves.info() -> table`: `installed` (the campaign manager's save and load functions are
  wrapped in this Lua state), `enabled` (the cfg switch), `chunked_saves` / `chunked_loads` this
  session, `last_name` / `last_len` of the last chunked value.

### 3.20 Relatives by marriage may marry

Since 0.42.0-beta.3. Automatic, nothing to call. The engine refuses a marriage between two
characters whenever any chain of family links joins them, and that chain includes marriages. So
after one marriage between two houses (Sun Jian's and Cao Cao's, say), every member of one family
counts as related to every member of the other, and diplomacy says "There exist no two characters
in these factions who can be married", although history has several marriages between the same
houses.

With the DLL, the marriage check alone ignores links through marriage: two characters are related
for marriage purposes only if they share a blood ancestor within `marriage_blood_generations`
generations (default 0: this test never blocks). The engine's own close-kin rule still applies on
top, so parents, children, siblings and grandparents can never marry. Nothing else changes: the
characters keep their distant-relative status and icon, family trees, faction-leader family
membership and every other use of the family links behave as before. Both diplomacy marriages and
the family-tree Marry actions follow the new rule, for the player and the AI.

`marriage_inlaws=0` restores the engine's rule. Both keys are part of the multiplayer version lock.

```lua
local t = se.query.marriage_hook()   -- { installed, generations, verdicts, searches, blood_blocked, may_marry }
```

- `se.query.marriage_hook() -> table | nil, message`: `installed`, `generations` (the cfg value),
  `verdicts` (pair checks the engine made), `searches` (relatedness answers given by the DLL),
  `blood_blocked` (pairs refused as blood relatives within `generations`), `may_marry` (pairs the
  engine accepted), all since injection.

---

## 4. Recipes

Each of these is a complete script. Console scripts live in `<game root>\lua_scripts\` and are
listed in `index.txt`; the same code works from a mod script (`script/campaign/mod/...`), where
`cm:callback` also works.

### 4.1 Move a character into another faction's pool and unlock them

```lua
-- se_recipe_move.lua : cqi CQI -> TARGET's recruitment pool, available immediately.
local CQI    = 72
local TARGET = "3k_main_faction_cao_cao"

local function log(s) ModLog("[recipe_move] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" then log("script extender not present") return end
se.logger = ModLog

log("before: " .. se.dump(se.query.character(CQI) or {}))
local ok, msg = se.modify.move_character(CQI, TARGET, "pool")
log("move_character -> " .. tostring(ok) .. " : " .. tostring(msg))

-- The move's own state step runs 1 s later via cm:callback (which does not fire from the
-- console), so unlock on the model queue instead; the queue runs in order.
cm:wait_for_model_sp(function()
    local c = se.query.character(CQI)
    if c and c.in_pool then
        local ok2, msg2 = se.modify.pool_unlock(CQI)
        log("pool_unlock -> " .. tostring(ok2) .. " : " .. tostring(msg2))
    else
        log("not in a pool yet: " .. se.dump(c or {}))
    end
    log("lock now: " .. tostring(se.query.pool_lock(CQI)))
end)
-- reopen the court screen to see the candidate list refresh
```

### 4.2 Recruit a locked unit into an empty retinue slot at 50% strength

```lua
-- se_recipe_recruit.lua
local CQI      = 1
local UNIT_KEY = "3k_dlc04_unit_wood_imperial_gate_guards"

local function log(s) ModLog("[recipe_recruit] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" or not se.available("se_recruit_unit") then log("SE missing") return end
se.logger = ModLog

-- find the first empty slot
local slot
for _, row in ipairs(se.query.retinue(CQI) or {}) do
    if not slot and row.index > 0 and row.unit_key == "" and row.can_recruit then slot = row.index end
end
if not slot then log("no empty slot") return end

local ok, msg = se.modify.recruit(CQI, UNIT_KEY,
    { source = "any", free = true, slot = slot })
log("recruit -> " .. tostring(ok) .. " : " .. tostring(msg))

-- opts.hp would be applied through cm:callback, which does not fire from the console:
cm:wait_for_model_sp(function()
    local ok2, msg2 = se.modify.unit_strength(CQI, slot, 50)
    log("unit_strength -> " .. tostring(ok2) .. " : " .. tostring(msg2))
    log("slot now: " .. se.dump(se.query.unit(CQI, slot) or {}))
end)
```

### 4.3 Force the Three Kingdoms with a ban list

```lua
-- se_recipe_three_kingdoms.lua
local FORCED = { "3k_main_faction_kong_rong" }
local BANNED = { "3k_main_faction_dong_zhuo", "3k_main_faction_yuan_shu" }

local function log(s) ModLog("[recipe_3k] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" then log("SE missing") return end
se.logger = ModLog
se.core = core

log("leaders before: " .. table.concat(se.query.world_leaders() or {}, ", "))
local ok, msg = se.modify.force_three_kingdoms{ forced = FORCED, banned = BANNED }
log("force_three_kingdoms -> " .. tostring(ok) .. " : " .. tostring(msg))
cm:wait_for_model_sp(function()
    log("leaders after: " .. table.concat(se.query.world_leaders() or {}, ", "))
end)
-- keep the bans in force for the rest of the campaign (saved; call se.load_emperor_policy()
-- after every load)
se.modify.emperor_policy{ forced = FORCED, banned = BANNED }
```

### 4.4 Force-build a free building in an empty slot

```lua
-- se_recipe_build.lua
local FACTION = "3k_main_faction_cao_cao"

local function log(s) ModLog("[recipe_build] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" or not se.available("se_slot_construct") then log("SE too old") return end
se.logger = ModLog

local okr, region = pcall(function() return cm:query_faction(FACTION):capital_region():name() end)
if not okr then log("no capital region") return end

local empty
for _, row in ipairs(se.query.region_slots(region) or {}) do
    if not row.has_building and empty == nil then empty = row.index end
end
if not empty then log("no empty slot in " .. region) return end

local candidates = se.query.building_candidates(region, empty, { all_chains = true })
local level = candidates[1]
if not level then log("no candidates for slot " .. empty) return end

local ok, msg = se.modify.building_construct(region, empty, level,
    { force = true, any_chain = true, free = true, complete = true })
log("construct " .. level .. " -> " .. tostring(ok) .. " : " .. tostring(msg))
-- It completes at the NEXT turn start, and only if the province is under its construction limit.
```

### 4.5 Rename a coalition

```lua
-- se_recipe_rename_alliance.lua
local MEMBER = "3k_main_faction_yuan_shao"
local TEXT   = "Yuan Shao's Grand Coalition"

local function log(s) ModLog("[recipe_alliance] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" or not se.available("se_alliance_name_set") then log("SE too old") return end
se.logger = ModLog

local target
for _, a in ipairs(se.query.alliances() or {}) do
    log("cqi " .. tostring(a.cqi) .. " '" .. tostring(a.name) .. "': " .. table.concat(a.members, ", "))
    for _, m in ipairs(a.members) do if m == MEMBER then target = a.cqi end end
end
if not target then log("no alliance contains " .. MEMBER) return end
local ok, msg = se.modify.alliance_name(target, TEXT, "inline")
log("alliance_name -> " .. tostring(ok) .. " : " .. tostring(msg))
-- run the script again to see the new name; verify it survives a save/load before shipping
```

### 4.6 Redefine an effect bundle and apply it (re-applied after every load)

```lua
-- se_recipe_bundle.lua : put this in a mod script so the re-define also runs after a load.
local FACTION = "3k_main_faction_cao_cao"
local BUNDLE  = "3k_dlc05_effect_bundle_ai_cao_cao_bonus_income"
local EFFECTS = {
    { effect = "3k_main_effect_characters_experience_bonus",
      scope  = "faction_to_character_own_factionwide_unseen", value = 25 },
}

local function log(s) ModLog("[recipe_bundle] " .. tostring(s)) end
if type(se) ~= "table" or not se.available("se_effect_bundle_define") then log("SE too old") return end
se.logger = ModLog
se.core = core

local function install()
    local ok, msg = se.modify.effect_bundle_define(BUNDLE, EFFECTS, FACTION)
    log("define -> " .. tostring(ok) .. " : " .. tostring(msg))
    cm:wait_for_model_sp(function()
        -- a holder that already carries the bundle keeps the OLD effect list until the bundle is
        -- removed and applied again, so apply it here after every define
        local ok2, e = pcall(function() cm:modify_faction(FACTION):apply_effect_bundle(BUNDLE, 0) end)
        log("stock apply -> " .. tostring(ok2) .. " " .. tostring(e))
    end)
end

install()                      -- the definition is NOT saved:
core:add_listener("recipe_bundle_load", "LoadingGame", true, function() install() end, true)
```

### 4.7 Cap the player's auto-resolve casualties and force a loss against one faction

```lua
-- se_recipe_ar_rules.lua : load once per session (or ship the same code in a mod script).
local MINE     = "3k_main_faction_cao_cao"     -- the faction the rules protect, by key: the same
                                               -- on every machine (never is_local_player)
local NEMESIS  = "3k_main_faction_dong_zhuo"   -- battles against this faction are always lost
local MY_CAP   = 0.25                          -- that faction never loses more than 25% per unit

local function log(s) ModLog("[recipe_ar] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" or type(se.autoresolve) ~= "table" then log("SE too old") return end
se.logger = ModLog
se.core = core

local function handler(ctx)
    local attacking = ctx.attacker.faction == MINE
    if not (attacking or ctx.defender.faction == MINE) then return nil end   -- not our battle
    local mine     = attacking and "attacker" or "defender"
    local theirs   = attacking and "defender" or "attacker"
    local enemy    = ctx[theirs]
    log("battle vs " .. tostring(enemy.faction) .. " (" .. tostring(ctx.battle_type) .. ")")

    local plan = { casualties = { [mine] = { max = MY_CAP } } }
    if enemy.faction == NEMESIS then
        plan.winner = theirs                       -- forced defeat
        plan.casualties[mine] = { max = 1.0 }      -- and take the losses that come with it
    end
    return plan
end

local ok, msg = se.autoresolve.set_handler(handler)
log("set_handler -> " .. tostring(ok) .. " : " .. tostring(msg))
-- The handler runs on every machine, so it must decide from faction keys only. plan.duels works
-- since 0.27; plan.bias is stored but never applied.
```

### 4.7a Weapons and mounts count in auto-resolve duels

The 190E script extender pack ships this as the MCT option "190E Duel CEO", with a bonus table
you can edit. The core of it:

```lua
-- se_recipe_duel_ceo.lua : equipped items add duel power (first tick + equip events)
local BONUS = { ["3k_main_ancillary_weapon_trident_halberd"] = 80, ["3k_main_ancillary_mount_red_hare"] = 40 }

if type(se) ~= "table" or type(se.modify.duel_power_bonus) ~= "function" then return end
se.logger = ModLog
se.core = core

local function bonus_of(character)
    local sum, list = 0, character:ceo_management():all_ceos_equipped_on_character()
    for i = 0, list:num_items() - 1 do sum = sum + (BONUS[list:item_at(i):ceo_data_key()] or 0) end
    return sum
end

local function refresh(characters)
    local map = {}
    for i = 0, characters:num_items() - 1 do
        local c = characters:item_at(i)
        map[c:command_queue_index()] = bonus_of(c)     -- 0 removes a stale entry
    end
    se.modify.duel_power_bonus(map)
end

cm:add_first_tick_callback(function()
    se.modify.duel_power_bonus_clear()                 -- the DLL table outlives a save load
    local factions = cm:query_model():world():faction_list()
    for i = 0, factions:num_items() - 1 do refresh(factions:item_at(i):character_list()) end
end)
core:add_listener("recipe_duel_ceo", "CharacterCeoEquipped", true, function(context)
    local c = context:query_character()
    se.modify.duel_power_bonus({ [c:command_queue_index()] = bonus_of(c) })
end, true)
```

### 4.8 A per-turn income line that survives loading

```lua
-- se_recipe_income.lua
local FACTION = "3k_main_faction_cao_cao"

local function log(s) ModLog("[recipe_income] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" then log("SE missing") return end
se.logger = ModLog
se.core = core

local ok, msg = se.modify.faction_income(FACTION, 1500, "horde_income")
log("faction_income -> " .. tostring(ok) .. " : " .. tostring(msg))
log("lines: " .. se.dump(se.query.faction_income(FACTION)))

-- after a load, restore the saved lines and re-install the listener:
core:add_listener("recipe_income_load", "LoadingGame", true, function()
    se.core = core
    log("load_income_lines -> " .. tostring(se.load_income_lines()))
end, true)
-- the amount is paid at FactionTurnStart; it does not show in the income breakdown.
```

### 4.9 Re-tune an AI faction: personality, potential, attitude

```lua
-- se_recipe_ai.lua
local FACTION     = "3k_main_faction_dong_zhuo"
local PERSONALITY = "3k_cai_personality_dong_zhuo_late_hard"
local POTENTIAL   = 140                          -- -100..150

local function log(s) ModLog("[recipe_ai] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" then log("SE missing") return end
se.logger = ModLog

local function report(what, ok, msg) log(what .. " -> " .. tostring(ok) .. " : " .. tostring(msg)) end

log("potential before: " .. se.dump(se.query.faction_potential(FACTION) or {}))
-- cai_personality needs the model thread, so read it there
se.on_model("read personality", function()
    log("personality before: " .. se.dump(se.query.cai_personality(FACTION) or {}))
    return true, "read"
end)

report("cai_personality", se.modify.cai_personality(FACTION, PERSONALITY))
report("faction_potential", se.modify.faction_potential(FACTION, POTENTIAL))
report("attitude", se.modify.attitude("3k_main_faction_cao_cao", FACTION, -3))
-- personality and potential persist across save/load; the attitude event is DB-valued.
```

### 4.10 Grant experience and unspent skill points to a character

```lua
-- se_recipe_xp.lua
local CQI = 14

local function log(s) ModLog("[recipe_xp] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" or not se.available("se_char_xp_add") then log("SE too old") return end
se.logger = ModLog

log("before: " .. se.dump(se.query.character_xp(CQI) or {}))
log("add_xp -> " .. tostring(select(2, se.modify.character_add_xp(CQI, 5000, false))))  -- exact
log("skill_points -> " .. tostring(select(2, se.modify.skill_points(CQI, 5))))
cm:wait_for_model_sp(function()
    log("after: " .. se.dump(se.query.character_xp(CQI) or {}))
end)
```

---

## 4b. `script_extender.cfg` and the performance features (DLL 0.31 - 0.35)

### The configuration file

The DLL reads `script_extender.cfg` **next to itself, then in its parent folder** — the second
place is the mod-manager layout, where the DLL sits in `dll\<version>\` and the file in `dll\`,
so one file serves every installed version. Format: one `key=value` per line, `#` comments,
values optionally quoted; an unknown key is logged and ignored.

| Key | Default | Purpose |
|---|---|---|
| `build_number`, `build_number_short`, `build_modified` | — | menu build strings applied at injection (§3.13) |
| `autoresolve_hooks` | on | `0` disables every auto-resolve hook (§3.14) |
| `horde_income` | on | `0` disables the horde income hook (§3.12a) |
| `horde_income_category` | `1` (MINING) | which income category receives the total, `0..3` |
| `recruit_perm_cache` | `1` | unit-permission table reuse, see below |
| `ui_recruit_cache_ms` | `5000` (max 30000) | UI recruit-list cache TTL |
| `ai_recruit_cache` | `0` | diagnostic AI-planner list cache |
| `file_probe_cache_ms` | `10000` (max 600000) | missing-directory cache for loose-file lookups |
| `diag_diplomacy` | `0` | `1` diplomacy evaluation counters + validation trace on request, `2` trace recording from injection |
| `diag_crash` | `1` | crash reporter (§3.18): `0` off, `1` `se_crash.txt` on a fault, `2` also the activity trace `se_activity.txt` |
| `ai_recruit_hook` | `1` | `0` skips the AI recruitment planner hook (no AI recruitment trace, no AI scope for the caches) |
| `followup_hooks` | `1` | `0` skips the MEDIATE PEACE button repair hooks (`se.ui.fix_followup_button` then refuses) |
| `save_chunking` | `1` | saved values larger than 64 KiB are saved in chunks (§3.19); `0` = vanilla behaviour |
| `marriage_inlaws` | `1` | relatives by marriage may marry (§3.20); `0` = the engine's rule |
| `marriage_blood_generations` | `0` | with `marriage_inlaws=1`: blood relatives sharing an ancestor within this many generations may not marry (0..6; 0 = only the engine's close-kin rule) |
| `duel_power_hook` | `1` | per-character auto-resolve duel power bonus (§3.14); `0` = no hook (`se.modify.duel_power_bonus` refuses) |
| `prebattle_single_delegate` | `1` | multiplayer pre-battle: one human's Delegate (autoresolve) vote counts for every human in the battle; `0` = every human must click Delegate |
| `postbattle_single_continue` | `1` | multiplayer post-battle: one human's Continue clears the post-battle screen for every human (an unmade captive choice falls back to the default); `0` = every human must click Continue |

**Thirteen of them are part of the multiplayer version lock** — `autoresolve_hooks`,
`ai_recruit_cache`, `recruit_perm_cache`, `horde_income`, `horde_income_category`,
`ai_recruit_hook`, `followup_hooks`, `marriage_inlaws`, `marriage_blood_generations`,
`save_chunking`, `duel_power_hook`, `prebattle_single_delegate` and
`postbattle_single_continue`. The DLL
hashes their *effective* values into the build string, so an absent key and an explicitly written
default give the same tag, but two players with different values cannot join each other (§1).

### The performance features

These need no script. None of them changes what the game computes.

| cfg key | default | what it does |
|---|---|---|
| `recruit_perm_cache` | `1` | The engine's "what can this retinue slot recruit" routine rebuilt a complete table of the faction's unit permissions **for every candidate unit** (units x permissions per list; with a large unit roster this was the biggest single cost of an AI turn and of an open character panel). The DLL lets the engine build each table once per list and reuses it. The first 3000 reuses of a session are checked against the engine's own rebuild; one difference turns the feature off for the session (DLL log). `0` = off, `2` = check only. |
| `ui_recruit_cache_ms` | `5000` | UI only: a recruitable-unit list asked for again by the panels is served from memory while the faction's treasury and the turn are unchanged, for at most this long. `0` = off. |
| `file_probe_cache_ms` | `10000` | Before reading a file from a pack the engine looks for a loose copy in every search root (each subscribed mod folder, `data`, ...) and remembers nothing: zooming the camera in fired 1316 failed lookups in one burst. The DLL remembers for this long that a *directory* does not exist and answers lookups into it without a system call. A folder created while the game runs is seen after at most this time. `0` = off. |
| `diag_diplomacy` | `0` | Diagnostics only, nothing is changed. `1`: the `dip_*` counters and the hooks behind `se.diag.diplomacy_trace`. `2`: the same, and the trace records from injection and rewrites `dip_trace.txt` next to the DLL every two seconds (for a session without a Lua console). |
| `ai_recruit_cache` | `0` | Diagnostic (whole-list cache inside the AI's recruitment budget planner). Measured as not worth it; leave off. |
| `diag_crash` | `1` | Diagnostics only, nothing is changed. `1`: the crash reporter (§3.18) writes `se_crash.txt` next to the DLL when the game faults. `2`: the same, and every hook entry / native call is also streamed to `se_activity.txt` next to the DLL (one line each; rotated at 32 MB) for a session that ends in a crash without a report. `0`: no exception handler at all. |

Measured on a modded late campaign: end turn 76 s -> 45 s (horde income hook made cheap in
0.32.4) -> about 30 s (permission tables, 0.34); turn 1 went 62 s -> 39 s.

### Reading the numbers from a script

The full reference for these three groups is §3.15 and §3.16.

```lua
se.query.perf()                     -- cache counters: hits / misses / passed_through, perm_*,
                                    -- ai_*, file_probes*
se.profile.start(50, 4, "endturn")  -- CPU sampling: 50 s after a 4 s delay; writes
                                    -- profile_endturn.txt + .folded.txt into dll\profiles\
se.profile.stop()                   -- end the run now; the reports are still written

se.core = core                      -- hand over the event manager once
se.diag.listeners_start()           -- time every core:add_listener listener from now on
se.diag.listeners_reset()           -- zero the numbers (e.g. right before End Turn)
se.diag.listeners_report(40)        -- log totals per event and the 40 most expensive listeners
```

`tools/profile_tree.py <folded.txt> [--min 2] [--depth 14] [--callers <fn>] [--minus <baseline>]`
turns the folded stacks into a call tree (addresses are Ghidra function starts).

---

## 4c. Watching the campaign AI recruit (DLL 0.36+, read-only; reference in §3.17)

The AI's recruitment planner only ever prices **empty** retinue slots (at most three per
character per pass, cheapest first); nothing in the engine replaces a unit that is already in a
slot. These calls show what it budgets, what it buys and what each slot could have had.

```lua
se.ai_recruit.trace(true)                 -- let the DLL copy the planner's requests (off by default)
local passes = se.query.ai_recruitment()  -- drains them: { { seq, faction_id, pending, requests = {
                                          --   { money, budget2, turn, target, rows = { { id, cost, cost2, kind } } } } } }
local rows, best = se.query.unit_quality("3k_main_unit_metal_jian_sword_guards")
                                          -- the AI's own ranking (cdir_military_generator_unit_qualities,
                                          -- read live): { { group, quality, quality_at_max_xp } }, best
se.ai_recruit.quality_override["my_mod_unit"] = 2400   -- units the table does not know, or a rebalance
local r = se.ai_recruit.report("3k_main_faction_cao_cao", { min_gap = 1.25 })
-- r = { faction, treasury, slots, empty, upgradable, affordable, forces = { { cqi, characters = { {
--   cqi, slots = { { index, unit, experience, strength, recruiting, quality, effective,
--   best = { key, quality, cost, turns }, gap, affordable } } } } } } }
```

`effective` scales a unit's quality towards `quality_at_max_xp` with its experience, the way the
AI's own table values a veteran; `gap` = best option / effective. Stock event
`UnitRecruitmentInitiated` (`context:faction()`, `context:unit_key()`) tells what was really
bought. The console script `se_ai_trace.lua` logs all three per AI faction turn.

---

## 4d. AI recruitment policy: better armies that fit their general (DLL 0.37+; reference in §3.17)

The engine fills empty retinue slots with the cheapest unit and never replaces a unit. This pass
runs at the start of every AI faction's turn, inside the model callback (the same on every
multiplayer machine), at normal recruitment cost:

```lua
se.core = core
se.ai_recruit.element_order.wood = { "wood", "metal", "water", "earth", "fire" }  -- best first, per general element
se.ai_recruit.element_order.fire = { "fire", "earth", "water:cavalry", "metal", "wood", "water" }  -- "element:cavalry" / "element:infantry" entries: a unit takes the best entry it matches
se.ai_recruit.unit_class["my_unit"] = "cavalry"       -- class of units the quality table does not know (cavalry = a role group naming cavalry)
se.ai_recruit.element_weight = { 1.30, 1.15, 1.00, 0.90, 0.80 }                  -- multiplier by rank in that list
se.ai_recruit.unit_element["my_unit"] = "metal"       -- keys that do not name their element
se.ai_recruit.quality_override["my_unit"] = 2400      -- units the game's quality table lacks
se.ai_recruit.config.min_gain = 1.5                   -- see the table below
se.ai_recruit.enable()                                -- once per Lua state (again after a load)
se.ai_recruit.disable()

local orders, info = se.ai_recruit.plan("3k_main_faction_cao_cao")   -- what it would do; changes nothing
se.ai_recruit.execute(orders)                                        -- carry orders out
se.ai_recruit.set_policy(function(faction_key)                       -- your own decision maker
	local orders, info = se.ai_recruit.plan(faction_key)
	-- edit / filter / replace the orders: { op = "recruit" | "replace", character, slot, unit, cost, ... }
	return orders, info
end)
```

score = the game's AI quality of the unit (`se.query.unit_quality`, the old unit's scaled up with
its experience) x element weight (the general's element comes from `character_subtype_key()`, or,
for captains and other leaders whose subtype names none, from their bodyguard unit in slot 0; the
unit's element from its key: the first `_`-separated word that names one) x
`duplicate_penalty` per copy already in the retinue. Empty slots take the best score; an occupied
slot is replaced when the best score is at least `min_gain` times the old one.

Every `config` key with its default is in §3.17; the ones you are most likely to move are
`min_gain` (how much better a replacement must be), `reserve` / `income_turns` / `max_spend`
(the budget) and `max_per_character` / `max_per_force` / `max_per_faction` (how fast an army
may change).

Console script with all of this at the top: `se_ai_recruit_rules.lua`. Test: `tools/test_lua_aipolicy.py`.

---

## 5. Troubleshooting

### The API is not there at all

- `type(se) ~= "table"`: the DLL is not injected, or it has not yet registered into *this* Lua
  state. Registration happens the first time a state ticks Lua, so the main-menu state is served
  first and the campaign state once a campaign has loaded.
- The DLL is inert when its fingerprint check fails. Look in its log for
  `fingerprint mismatch: expected ...` or `N anchor(s) failed; refusing to run` — that means the
  game was updated and the DLL must be rebuilt against the new build. A healthy log contains
  `all N addresses and M vtables verified`, `lua_gettop hook installed`, then per state
  `registered se_* functions into lua_State ...` and `se_api.lua loaded into lua_State ...`.
- Never inject twice into the same process, and never inject a second (or newer) DLL on top of a
  running one. To update: quit the game, start it, inject once after the main menu is up.

### Common refusal messages

| Message | Meaning |
|---|---|
| `native se_x is not available (DLL too old or not injected)` | this DLL predates the feature; gate with `se.available` |
| `refused in multiplayer: ... called outside a model callback` | multiplayer is lockstep: call `se.modify.*` from an event listener / turn-start callback that runs on every machine, not from UI code or the console |
| `campaign manager (cm) is not available in this Lua state` | called from a state with no `cm` (for example the frontend) |
| `core (event manager) is not available: set se.core = core ...` | assign `se.core = core` before `emperor_policy`, `faction_income`, `autoresolver_variable` or `set_handler` |
| `<tag> queued on the model thread (result in the log / callback)` | not an error: the real result is in the log |
| `no character with cqi N` / `cqi mismatch for N` | the cqi does not resolve, or the interface reports a different cqi |
| `no faction '<key>'` / `no region '<key>'` | wrong database key |
| `character commands no persistent retinue` | the character has no retinue (not a general) |
| `slot is not linked to a military force slot (army not deployed?)` | the slot's unit is not on the map, so strength/experience cannot be read or written |
| `slot N holds <key>; pass replace = true to swap it` | target an empty slot or pass `replace` |
| `<key> is locked (reasons 0x...); use opts.source = 'locked' or 'any'` | the item is not unlocked in that tree |
| `refusing to disband the commander's own slot` | slot 0 is the commander |
| `refused: no pending battle with a human player` | no pre-battle panel, or the battle is AI against AI |
| `effects must be a non-empty list of {effect=, scope=, value=}` | malformed effect bundle list |
| `cm:modify_campaign_ai() is not available (needs the model thread)` | `se.query.cai_personality` was called outside a model callback — wrap it in `se.on_model` |
| `state must be 'pool' or 'recruited'` | third argument of `move_character` |
| `usage: autoresolver_variable(key, number)` | the key must be a string and the value a number |
| `a profile is already running` | one profiler run at a time; `se.profile.stop()` first |
| `no saved policy` / `no saved income lines` | `se.load_emperor_policy()` / `se.load_income_lines()` found nothing in the save |

### Nothing visibly changed

- **Court candidate list**: close and reopen the court screen after `pool_unlock` /
  `move_character`.
- **Retinue / unit panel**: re-read after a moment; some values only refresh when the panel is
  rebuilt.
- **Building**: right after `building_construct` the slot still reports the old building; the
  construction completes at the next turn start (and is dropped if the province is over its
  construction limit).
- **Auto-resolve panel**: the casualty bar does not refresh after a plan recompute; the numbers
  the engine uses are still the planned ones.
- **A queued call**: check the log for the real result line, `"<tag> -> true : ..."`.

### Where the logs are

- **DLL log**: `script_extender.log` next to the injected `script_extender.dll` (truncated on
  every injection, also mirrored to `OutputDebugString`).
- **Lua log** (in-game console environment): `lua_mod_log.txt` in the game root and the
  per-session `ironic_log_<faction>_<stamp>.txt` — the newest by modification time is the current
  one.
- **Crash report of the DLL** (0.42+): `se_crash.txt` next to the DLL (§3.18); with
  `diag_crash=2` also `se_activity.txt`.
- **Crash dumps of the game**: `%APPDATA%\The Creative Assembly\ThreeKingdoms\crash_report\`
  (`D<date>_T<time>.mdmp`; `%APPDATA%` already ends in `Roaming`, do not add it again) and
  `%LOCALAPPDATA%\CrashDumps`. The `.stack.txt` beside each `.mdmp` only lists module names.

### Reporting a crash

Send, in this order of usefulness: **`se_crash.txt`** (the last block), the game's `.mdmp` from
`crash_report\` of the same minute, the **DLL log** next to the DLL, the DLL version
(`se.version()`), the Lua log, and the exact script that was running. The `se_crash.txt` block
names the hook or native that was active on the faulting thread; "se scopes on this thread" empty
and no `script_extender.dll` frame in the stack means the DLL was idle at the time. A block whose
last line says "first-chance" for a session that did not end there was an exception the engine
handled itself. `tools/dump_crash.py <file.mdmp>` in the workspace prints the fault-time
registers and the stack of a minidump as module+RVA.
Every crash the project has had so far came from an engine routine being handed the wrong object;
the natives validate vtables, back-pointers and registry round-trips and refuse instead of
writing, so a refusal message is the expected outcome of a bad argument — a crash is a bug worth
reporting.

---

## 6. Index of public functions

Every function the module defines. Optional arguments in `[ ]`; §3 has the details.

| Function | Description |
|---|---|
| `se.ai_recruit.class_of(unit_key)` | cavalry or infantry, from the unit's role groups |
| `se.ai_recruit.disable()` | stop the AI recruitment pass (the listener stays registered) |
| `se.ai_recruit.element_factor(element, unit_key)` | score multiplier of a unit for a general of that element |
| `se.ai_recruit.element_of(key)` | the element named by a unit or character-subtype key |
| `se.ai_recruit.enable()` | install the AI recruitment pass at faction turn start |
| `se.ai_recruit.execute(orders)` | carry recruitment orders out at normal cost |
| `se.ai_recruit.forces_of(faction_key)` | the pass's own view of a faction's armies |
| `se.ai_recruit.plan(faction_key [, money])` | the orders the policy would issue; changes nothing |
| `se.ai_recruit.report(faction_key [, opts])` | per-slot audit: unit, quality, best option, gap |
| `se.ai_recruit.set_policy(fn)` | replace the pass's decision maker |
| `se.ai_recruit.trace(on)` | let the DLL copy the AI's recruitment requests |
| `se.autoresolve.clear_handler()` | forget the auto-resolve handler and clear the stored plan |
| `se.autoresolve.set_handler(fn)` | call fn(ctx) on every PendingBattle with a human player |
| `se.available(native)` | is a given se_* native present in this Lua state |
| `se.character(cqi)` | checked cm:query_character |
| `se.crash.info()` | crash reporter state: installed, level, hooks, reports written |
| `se.crash.mark(text)` | breadcrumb for the crash report |
| `se.crash.selftest([mode])` | write a test crash report without faulting |
| `se.modify.followup_propose()` | queue the engine's propose for the follow-up negotiation popup on screen |
| `se.ui.fix_followup_button()` | repair the dead MEDIATE PEACE button (1.7.2.0) |
| `se.diag.diplomacy_mark(label)` | time marker inside the diplomacy validation trace |
| `se.diag.diplomacy_report()` | write dip_trace.txt, log the blocked evaluations |
| `se.diag.diplomacy_trace(on [, include_ai])` | record the engine's treaty-component validations |
| `se.diag.listeners_report([top])` | totals per event and the most expensive listeners |
| `se.diag.listeners_reset()` | zero the listener timings |
| `se.diag.listeners_start()` | time every core:add_listener listener from now on |
| `se.dump(t [, indent])` | pretty-print a table |
| `se.faction(key)` | checked cm:query_faction |
| `se.G(name)` | read a global from the state's real global table |
| `se.is_null(v)` | null-interface test that also works on list interfaces |
| `se.load_emperor_policy()` | restore the saved emperor policy and its listener |
| `se.load_income_lines()` | restore saved income lines and their listener |
| `se.log(s)` | log through se.logger / ModLog / the DLL log |
| `se.modify.alliance_name(cqi, text [, mode])` | rename an alliance / coalition |
| `se.modify.attitude(a, b, level)` | fire an attitude-change event, level -3..3 |
| `se.modify.autoresolve_plan(plan [, ctx])` | store a plan for the current pending battle |
| `se.modify.autoresolve_plan_clear()` | drop the stored plan |
| `se.modify.autoresolver_variable(key, value)` | retune one autoresolver_* constant (session) |
| `se.modify.autoresolver_variables_reset()` | all auto-resolver constants back to engine values |
| `se.modify.build_number(build, short, modified)` | replace the main-menu build strings |
| `se.modify.building_construct(region, slot, level [, opts])` | build / upgrade / convert, optionally forced and free |
| `se.modify.building_damage(region, slot, percent)` | damage a building |
| `se.modify.building_destroy(region, slot)` | destroy a building |
| `se.modify.building_repair(region, slot [, opts])` | repair a building (opts.free) |
| `se.modify.cai_personality(faction, personality)` | swap an AI faction's CAI personality |
| `se.modify.character_add_xp(cqi, n [, scaled])` | add exact or engine-scaled experience |
| `se.modify.disband(cqi, slot)` | empty a retinue slot |
| `se.modify.effect_bundle_apply_custom(faction, bundle, effects [, turns])` | apply a bundle with a per-instance effect list |
| `se.modify.effect_bundle_define(bundle, effects [, faction])` | rewrite a bundle record's effects for the session |
| `se.modify.effect_bundle_restore(bundle [, faction])` | restore a bundle's stock effects |
| `se.modify.emperor_policy(policy)` | persistent forced / banned emperor steering |
| `se.modify.faction_income(faction, amount [, label])` | script-side per-turn income line |
| `se.modify.faction_potential(key, value)` | set an AI faction's potential (-100..150) |
| `se.modify.faction_progression(key, level)` | raise a faction to a progression level |
| `se.modify.force_three_kingdoms([opts])` | fill the world-leader seats with forced / banned lists |
| `se.modify.move_character(cqi, faction [, state])` | move a character between factions and into pool / recruited |
| `se.modify.pool_lock(cqi, turns)` | lock a pool character for N rounds |
| `se.modify.pool_unlock(cqi)` | make a pool character available now |
| `se.modify.recruit(cqi, unit_key [, opts])` | recruit a unit into a retinue slot |
| `se.modify.release_to_pool(cqi)` | recruited character -> their own faction's pool |
| `se.modify.replace(cqi, slot, unit_key [, opts])` | recruit into an occupied slot |
| `se.modify.skill_points(cqi, n)` | set the unspent skill-point counter |
| `se.modify.unit_experience(cqi, slot, level)` | set a unit's chevron level |
| `se.modify.unit_strength(cqi, slot, percent)` | set a unit's strength percent |
| `se.modify.world_leader(key)` | grant an emperor seat directly |
| `se.on_model(tag, f [, cb])` | run a function on the model thread |
| `se.profile.start(seconds [, delay [, label]])` | sampling profiler run; reports next to the DLL |
| `se.profile.stop()` | end the running profile now |
| `se.query.ai_recruitment()` | drain the traced AI recruitment planning passes |
| `se.query.alliances()` | alliances with cqi, name and members |
| `se.query.assignment(cqi [, dump])` | active assignment key, state, rounds, province |
| `se.query.attitude(a, b)` | standing of a towards b |
| `se.query.autoresolve_plan()` | the stored plan and its encoded form |
| `se.query.autoresolve_prediction()` | the engine's prediction for the pending battle |
| `se.query.autoresolver_variable(key)` | one autoresolver_* constant |
| `se.query.autoresolver_variables()` | all autoresolver_* constants |
| `se.query.build_number()` | current main-menu build strings |
| `se.query.building_candidates(region, slot [, opts])` | building levels a slot can take |
| `se.query.cai_personality(faction)` | current CAI personality (model thread) |
| `se.query.character(cqi)` | character summary incl. pool / recruited state |
| `se.query.character_xp(cqi)` | experience, rank, max rank, skill points |
| `se.query.effect_bundle(bundle [, faction])` | engine dump of a bundle record |
| `se.query.faction(key)` | progression level, world-leader state, seats |
| `se.query.faction_effect_value(faction, effect_id)` | faction-level value of an effect id |
| `se.query.faction_force_gdp(faction_key)` | gdp_abs total on the faction and its armies (what the horde income hook adds) |
| `se.query.faction_income(faction)` | script-side income lines and their total |
| `se.query.faction_potential(key)` | potential value, base, bonus, roll |
| `se.query.faction_xp_gain_percent(key)` | faction character-experience-gain percentage |
| `se.query.marriage_hook()` | marriage hook state and counters (relatives by marriage) |
| `se.query.horde_income_hook()` | whether the horde income hook is installed, and its income category |
| `se.query.pending_battle()` | full pending-battle context incl. prediction |
| `se.query.perf()` | counters of the DLL's caches and file probes |
| `se.query.pool_lock(cqi)` | pool availability status and counter (two numbers) |
| `se.query.recruitable(cqi, slot)` | the slot's recruitment items with cost / turns / lock reasons |
| `se.query.region_slots(region)` | region slots with their buildings and health |
| `se.query.retinue(cqi)` | retinue slots with units, strength, experience |
| `se.query.skill_points(cqi)` | unspent skill points |
| `se.query.unit(cqi, slot)` | unit key, strength, experience of one slot |
| `se.query.unit_quality(unit_key)` | the campaign AI's quality rows for a unit, and the best of them |
| `se.query.world_leaders()` | faction keys holding an emperor seat |
| `se.region(key)` | checked cm:query_region |
| `se.saves.info()` | save chunking state (saved values above 64 KiB) |
| `se.version()` | DLL version string |

### Values the module reads from you, and its own state

| Name | Purpose |
|---|---|
| `se.logger` | `function(string)` used by `se.log`; set it to `ModLog` in a console script |
| `se.core` | the event manager; needed by everything that installs a listener |
| `se.EFFECT_CHARACTER_XP_GAIN` | `385` (`0x181`), the character-experience-gain effect id |
| `se.autoresolve.handler` | the handler `set_handler` stored |
| `se.ai_recruit.config` and the tables beside it | tuning of the AI recruitment policy (§3.17) |

Fields starting with an underscore (`se._income`, `se._ar_vars`, `se._ar_plan`,
`se._emperor_policy`, ...) and `se.ai_recruit.enabled` / `listening` / `policy` are the
module's own state. They are visible, but they are not API and may change with any DLL version.

---

## 7. Open questions and source discrepancies

Re-checked against `se_api.lua`, the DLL sources and `notes/*.md` on **2026-09-19, DLL 0.40.0**;
the 0.41.0 additions (sections 3.16, 4b) on **2026-09-20**.
These are the places where a source disagrees with another, or where something is implemented but
not confirmed by a live test. Signatures follow `se_api.lua`; behaviour follows the newest dated
section of the notes.

**Not confirmed live yet**

1. **The multiplayer lobby check.** The whole version lock rests on the lobby comparing the build
   string the DLL rewrites. That has **not been tested on two machines** - neither the join
   refusal between different DLL versions nor a campaign running in lockstep with the API in use.
   Everything else about multiplayer follows from the lockstep rules, not from a test.
2. **Persistence** of forced world-leader seats, of a renamed alliance
   (`se.modify.alliance_name`) and of the per-instance effect list of
   `se.modify.effect_bundle_apply_custom`. Verified to persist across save / restart / load:
   `cai_personality` and `faction_potential`.
3. **`se.modify.emperor_policy`** installs a `FactionTurnStart` listener that has never been
   observed running across a turn.
4. **`se.modify.attitude`** (the change event) is implemented but not confirmed live; the
   per-treaty-component evaluation bias is not implemented at all.
5. **Forced new buildings.** A forced construction into an empty slot was dropped at turn
   processing while the province was over its construction limit. Whether a forced build under
   the limit completes has not been tested.

**Implemented but deliberately incomplete**

6. **`plan.bias` and `plan.duels[i].fate`** are validated, clamped and stored, and neither is
   applied. `winner` and `casualties` are applied since 0.26.2, `duels` since 0.27 (all three
   verified live).
7. **Multi-force sides in an auto-resolve plan.** A side whose alliance summary holds more than
   one army record is skipped by the result rewrite; the element size of that vector is not
   mapped.
8. **Unit strength and chevron writes** are direct field writes. The engine's own setters were
   never traced, so derived state could in principle go stale. The stock getters and the retinue
   panel agree with what is written.
9. **No direct skill grant** by skill key, and **no effect id -> key map** beyond the single
   named constant `se.EFFECT_CHARACTER_XP_GAIN`.
10. **Effect bundle income.** A defined `+50 gdp_mod_all / faction_to_region_own` bundle produced
    no change in `projected_net_income`, before or after a turn. Force-scoped GDP effects are not
    implemented at all (the region GDP computation was never reached) - which is what the horde
    income hook exists for.

**Stale text in the sources**

11. **`HANDOFF.md` is a maintainer document and its API table lags.** Where the two disagree,
    this document and `se_api.lua` are right. Its pre-0.31 version paragraph ends at 0.24.
12. **`se_api.lua:1657` still documents `ui_recruit_cache_ms` as "default 250"**; the DLL's
    default is **5000** (`perf.rs:390`), which is what §4b lists. The comment is stale, the code
    is right.
13. **Example scripts carry their own version claims** in their header comments
    (`se_ar_rules.lua` still describes the 0.24-era auto-resolve state). Trust this document.
