# Script Extender: campaign Lua API

Scripting reference for the `se.*` API that the script extender DLL adds to Total War: THREE
KINGDOMS **build 1.7.2.0**. Audience: mod authors who already know 3K campaign scripting
(`cm`, `core:add_listener`, `QUERY_*` / `MODIFY_*` interfaces).

Sources of truth for this document: `crates/script_extender/lua/se_api.lua` (signatures),
`HANDOFF.md` (injection rules, version history), `notes/*.md` (behaviour, live verification),
`CLAUDE.md` (Lua gotchas). Current DLL version at the time of writing: **0.26.2**.

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
   such calls are still queued through `cm:wait_for_model_sp`.
2. **Nothing a synced script does may depend on which machine it runs on.** Do not branch on
   `cm:get_local_faction()` or on `ctx.*.is_local_player` in code that ends in a modify call;
   use `is_human` and faction keys. The auto-resolve handler is called for every battle with a
   human player, on every machine, and chance-based duel rules are seeded from `ctx.seed`
   (turn number and force cqis), never from anything machine-specific. Do not use `math.random`
   or `os.time` to decide a change; use the model's own random functions.
3. **Both machines must run the same script extender with the same simulation settings.** The
   DLL enforces this through the game's build string, which the multiplayer lobby compares:
   it always ends up containing the DLL version and a fingerprint of `autoresolve_hooks`, `ai_recruit_cache`,
   `horde_income` and `horde_income_category` (`script_extender.cfg` text may use `{version}`
   and `{sync}`; otherwise ` [se <version>.<sync>]` is appended; without cfg text the game's
   own string is extended; `se.modify.build_number` cannot remove it). A player without the
   DLL, with another DLL version or with different hook settings shows a different build and
   cannot join. This relies on the lobby's version check using that string: **not verified on
   two machines yet.**

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

### Setup lines every script needs

The module chunk runs inside the DLL, and its environment does **not** see the script libraries'
globals. Two objects must be handed over explicitly:

```lua
se.logger = ModLog     -- any function(string); otherwise output goes to the DLL log
se.core   = core       -- the event manager; needed by emperor_policy, faction_income,
                       -- autoresolver_variable and se.autoresolve.set_handler
```

Without `se.logger`, the module falls back to a global `ModLog` if one is visible, else to the
DLL's own `se_log` (which writes to `script_extender.log`). Without `se.core`, the functions that
install listeners return
`false, "core (event manager) is not available: set se.core = core ..."`.

### How results are returned

- **Queries** (`se.query.*`) return a value or table on success, and `nil, message` on failure.
- **Modifiers** (`se.modify.*`) return `ok, message`. `ok == true` means *done or queued*.

### The model-thread rule

Model mutation is only legal on the campaign model thread. Every `se.modify.*` call goes through
`se.on_model`:

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

#### `se.on_model(tag, f [, cb]) -> ok, message`
Run `f` on the model thread (see §1). `f` returns `ok, message`. `cb(ok, msg)` is optional.
Refuses in multiplayer.

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
`{ index, unit_key, strength, experience, can_recruit, is_recruiting, recruiting }`.
`strength` / `experience` are only filled when the slot is linked to a deployed military force
unit. Since 0.8. Status: verified live.

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

### 3.12a Horde income (DLL 0.28+, hook optional)

`gdp_abs` effects (bonus value `region_gdp`) only reach the treasury through regions. With
`horde_income=1` in `script_extender.cfg` the DLL hooks the engine routine that recomputes a
faction's income categories and adds, 1:1, every `gdp_abs` value found on the **faction itself**
(scopes such as `faction_to_faction_own`) and on **each military force it owns**
(`force_to_force_own`, what horde building bundles use). `gdp_mod` values are ignored.
`horde_income_category` selects the category: `0` TAXES, `1` MINING, `2` TRADE,
`3` MILITARY_FORCE. **MINING is unused in 3K** (never computed, no row in the stock treasury
panel, but part of the totals), so a UI mod can show it as its own line: bind a treasury row to
`CcoFactionEconomy` / `VaryingRegIncomeDetailsSum("MINING")` and label it with
`Loc("<your key>")` (= `campaign_localised_strings_string_<your key>`), which other mods can
translate. Both settings are part of the multiplayer version lock. Verified live on 0.28.3 /
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
	ModLog("horde income needs script_extender.cfg: horde_income=1, horde_income_category=1")
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

Everything here applies to battles a human player is involved in (every machine of a multiplayer game evaluates the same handler; see the multiplayer rules).

#### What each DLL version actually applies

| Plan field | Status |
|---|---|
| `winner` | applied by the engine hook; **use 0.26.2 or later** (verified live there: winning alliance index and per-side result ids are written; 0.26.0 / 0.26.1 had no effect on the winner) |
| `casualties` | applied since 0.26; **verified live** (a 10% cap turned a predicted 798 -> 563 into 798 -> 730, and the army kept about 90% after the battle) |
| `bias` | accepted, clamped and stored — **not applied** by any current version |
| `duels` | accepted, validated and stored — **not applied** by any current version |
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
the call refuses with `refused: no pending battle with the local player` when the local player is
not involved, and with `refused: multiplayer campaign` in multiplayer.

Plan shape (schema, not runnable code — `|` means "one of"):

```text
plan = {
  winner = "attacker" | "defender" | nil,       -- force the winner
  casualties = {                                 -- per side
     attacker = { scale = 1.0, max = 1.0 },      -- scale 0..10, max 0..1 (fraction lost)
     defender = { scale = 1.0, max = 1.0 },
  },
  bias  = { attacker = 1.0, defender = 1.0 },    -- 0.1..10, STORED BUT NOT APPLIED
  duels = {                                      -- STORED BUT NOT APPLIED
     max = 6,                                    -- 0..16
     default = "vanilla" | "none",
     pairs = { { a = cqi, b = cqi, happen = true, win_chance = 0.5, winner = cqi,
                 fate = "kill" | "wound" | "spare" | "flee" } },
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

Native `se_ar_plan_set` (+ `se_ar_recompute`). Since 0.24; applied since 0.26.

#### `se.modify.autoresolve_plan_clear() -> ok, message`

Drop the stored plan (vanilla behaviour again). No model queue, no multiplayer check.

#### `se.query.autoresolve_plan() -> plan, encoded`

The plan table as it was given, plus the encoded string the DLL currently holds.

#### `se.autoresolve.set_handler(fn) -> ok, message`

Install a handler called on every `PendingBattle` that involves the local player. `fn(ctx)`
receives the `se.query.pending_battle()` table and returns a plan table, or `nil` for vanilla
behaviour. Needs `se.core`. The listener clears any previous plan first, and a second listener
drops the plan again on `BattleCompleted`. Calling it again replaces the handler
(`"handler replaced"`).

#### `se.autoresolve.clear_handler() -> ok, message`

Forget the handler and clear the stored plan. The listeners stay registered but do nothing.

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
local NEMESIS  = "3k_main_faction_dong_zhuo"   -- battles against this faction are always lost
local MY_CAP   = 0.25                          -- the player never loses more than 25% per unit

local function log(s) ModLog("[recipe_ar] " .. tostring(s)) end
pcall(function() return cm:query_model():world() end)
if type(se) ~= "table" or type(se.autoresolve) ~= "table" then log("SE too old") return end
se.logger = ModLog
se.core = core

local function handler(ctx)
    local i_attack = ctx.attacker.is_local_player == true
    local mine     = i_attack and "attacker" or "defender"
    local theirs   = i_attack and "defender" or "attacker"
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
-- bias and duels in a plan are stored but not applied by the current DLL.
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

## 4b. Performance features and diagnostics (DLL 0.31 - 0.34)

These need no script; they are controlled from `script_extender.cfg` (next to the versioned DLL
folders). None of them changes what the game computes.

| cfg key | default | what it does |
|---|---|---|
| `recruit_perm_cache` | `1` | The engine's "what can this retinue slot recruit" routine rebuilt a complete table of the faction's unit permissions **for every candidate unit** (units x permissions per list; with a large unit roster this was the biggest single cost of an AI turn and of an open character panel). The DLL lets the engine build each table once per list and reuses it. The first 3000 reuses of a session are checked against the engine's own rebuild; one difference turns the feature off for the session (DLL log). `0` = off, `2` = check only. |
| `ui_recruit_cache_ms` | `5000` | UI only: a recruitable-unit list asked for again by the panels is served from memory while the faction's treasury and the turn are unchanged, for at most this long. `0` = off. |
| `ai_recruit_cache` | `0` | Diagnostic (whole-list cache inside the AI's recruitment budget planner). Measured as not worth it; leave off. |

`recruit_perm_cache` and `ai_recruit_cache` are part of the multiplayer sync fingerprint: both
players need the same values.

```lua
se.query.perf()              -- counters: hits / misses (UI cache), perm_built / perm_shared /
                             -- perm_same / perm_diff (permission tables), ai_* (diagnostic)
se.profile.start(50, 4, "endturn")  -- CPU sampling: 50 s after a 4 s delay; report
                                    -- profile_endturn.txt + .folded.txt in <dll folder>\profilesse.profile.stop()            -- end the run now; the report is written when a run ends
```

`tools/profile_tree.py <folded.txt> [--min 2] [--depth 14] [--callers <fn>] [--minus <baseline>]`
turns the folded stacks into a call tree (addresses are Ghidra function starts).

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
| `refused: no pending battle with the local player` | no pre-battle panel, or the battle does not involve you |
| `effects must be a non-empty list of {effect=, scope=, value=}` | malformed effect bundle list |

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
- **Crash dumps**: `%LOCALAPPDATA%\CrashDumps`.

### Reporting a crash

Send: the game build number, the DLL version (`se.version()`), the **DLL log** next to the DLL,
the minidump from `%LOCALAPPDATA%\CrashDumps`, the Lua log, and the exact script that was running.
Every crash the project has had so far came from an engine routine being handed the wrong object;
the natives now validate vtables, back-pointers and registry round-trips and refuse instead of
writing, so a refusal message is the expected outcome of a bad argument — a crash is a bug worth
reporting.

---

## 6. Index of public functions

| Function | Description |
|---|---|
| `se.autoresolve.clear_handler()` | forget the auto-resolve handler and clear the stored plan |
| `se.autoresolve.set_handler(fn)` | call `fn(ctx)` on every local-player `PendingBattle` to produce a plan |
| `se.available(native)` | is a given `se_*` native present in this Lua state |
| `se.character(cqi)` | checked `cm:query_character` |
| `se.dump(t [, indent])` | pretty-print a table |
| `se.faction(key)` | checked `cm:query_faction` |
| `se.G(name)` | read a global from the state's real global table |
| `se.is_null(v)` | null-interface test that also works on list interfaces |
| `se.load_emperor_policy()` | restore the saved emperor policy and its listener |
| `se.load_income_lines()` | restore saved income lines and their listener |
| `se.log(s)` | log through `se.logger` / `ModLog` / the DLL log |
| `se.modify.alliance_name(cqi, text [, mode])` | rename an alliance / coalition |
| `se.modify.attitude(a, b, level)` | fire an attitude-change event, level -3..3 |
| `se.modify.autoresolve_plan(plan [, ctx])` | store a plan for the current pending battle |
| `se.modify.autoresolve_plan_clear()` | drop the stored plan |
| `se.modify.autoresolver_variable(key, value)` | retune one `autoresolver_*` constant (session) |
| `se.modify.autoresolver_variables_reset()` | all auto-resolver constants back to engine values |
| `se.modify.build_number(build, short, modified)` | replace the main-menu build strings |
| `se.modify.building_construct(region, slot, level [, opts])` | build / upgrade / convert, optionally forced and free |
| `se.modify.building_damage(region, slot, percent)` | damage a building |
| `se.modify.building_destroy(region, slot)` | destroy a building |
| `se.modify.building_repair(region, slot [, opts])` | repair a building (`opts.free`) |
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
| `se.query.alliances()` | alliances with cqi, name and members |
| `se.query.assignment(cqi [, dump])` | active assignment key, state, rounds, province |
| `se.query.attitude(a, b)` | standing of a towards b |
| `se.query.autoresolve_plan()` | the stored plan and its encoded form |
| `se.query.autoresolve_prediction()` | the engine's prediction for the pending battle |
| `se.query.autoresolver_variable(key)` | one `autoresolver_*` constant |
| `se.query.autoresolver_variables()` | all `autoresolver_*` constants |
| `se.query.build_number()` | current main-menu build strings |
| `se.query.building_candidates(region, slot [, opts])` | building levels a slot can take |
| `se.query.cai_personality(faction)` | current CAI personality (model thread) |
| `se.query.character(cqi)` | character summary incl. pool / recruited state |
| `se.query.character_xp(cqi)` | experience, rank, max rank, skill points |
| `se.query.effect_bundle(bundle [, faction])` | engine dump of a bundle record |
| `se.query.faction(key)` | progression level, world-leader state, seats |
| `se.query.faction_effect_value(faction, effect_id)` | faction-level value of an effect id |
| `se.query.faction_force_gdp(faction_key)` | gdp_abs total on the faction and its armies (what the horde income hook adds) |
| `se.query.horde_income_hook()` | whether the horde income hook is installed, and its income category |
| `se.query.faction_income(faction)` | script-side income lines and their total |
| `se.query.faction_potential(key)` | potential value, base, bonus, roll |
| `se.query.faction_xp_gain_percent(key)` | faction character-experience-gain percentage |
| `se.query.pending_battle()` | full pending-battle context incl. prediction |
| `se.query.pool_lock(cqi)` | pool availability status and counter |
| `se.query.recruitable(cqi, slot)` | the slot's recruitment items with cost / turns / lock reasons |
| `se.query.region_slots(region)` | region slots with their buildings and health |
| `se.query.retinue(cqi)` | retinue slots with units, strength, experience |
| `se.query.skill_points(cqi)` | unspent skill points |
| `se.query.unit(cqi, slot)` | unit key, strength, experience of one slot |
| `se.query.world_leaders()` | faction keys holding an emperor seat |
| `se.region(key)` | checked `cm:query_region` |
| `se.version()` | DLL version string |

Constant: `se.EFFECT_CHARACTER_XP_GAIN = 385` (character experience gain effect id).

---

## 7. Open questions and source discrepancies

These are places where the sources disagree or are explicitly unresolved. Signatures follow
`se_api.lua`; behaviour follows the newest dated section of the notes.

1. **Auto-resolve version claims.** `HANDOFF.md` §4 and §6 describe 0.24 as the current DLL and
   say "winner 0.25, bias/casualties 0.26, duels 0.27 pending". `Cargo.toml`, the git history and
   `notes/autoresolve.md` are at **0.26.2**, where *casualties and winner* are applied and *bias*
   is not. The example script `se_ar_rules.lua` repeats the older claim in its header comment.
   This document follows the notes and the code.
2. **Prediction table keys.** The comment above `se.query.autoresolve_prediction` in `se_api.lua`
   lists neither `<side>_prediction_id` nor `pending_battle`, but the DLL emits both, and
   `se_ar_plan.lua` reads `attacker_prediction_id`. Documented here from the DLL's output.
3. **`se.query.attitude` return shape.** `HANDOFF.md` describes it as returning a standing value;
   `se_api.lua` returns a table `{ standing, stock }`. The table is correct.
4. **Forced winner: verified live on 0.26.2** (2026-09-18: a predicted close victory was forced
   to a defeat; the game logged "player has lost a field battle as an attacker" and awarded
   "Battle Defeat" XP). 0.26.0 / 0.26.1 swapped the wrong fields and had no effect on the
   winner. The casualty cap is verified live as well.
5. **Multi-force sides.** A side whose alliance summary holds more than one army record is skipped
   by the plan rewrite; the element size of that vector is not mapped.
6. **Forced new buildings.** A forced construction into an empty slot was dropped at turn
   processing while the province was over its construction limit; whether a forced build under the
   limit completes has not been tested.
7. **Effect bundle income.** A defined `+50 gdp_mod_all / faction_to_region_own` bundle produced
   no change in `projected_net_income` before or after a turn. Force-scoped GDP effects are not
   implemented at all (the region GDP computation was never reached).
8. **Persistence not yet tested**: forced world-leader seats, renamed alliances, and the custom
   effect list of `effect_bundle_apply_custom`.
9. **`emperor_policy`** installs a `FactionTurnStart` listener that has never been observed
   running across a turn.
10. **Unit strength and chevron writes** are direct field writes; the engine's own setters were
    never traced, so derived state could in principle go stale.
11. **No direct skill grant** by skill key, and **no effect id -> key map** beyond the single
    named constant.
12. **`se.modify.attitude`** (the change event) and the treaty-component evaluation bias are
    untested / not implemented respectively.
