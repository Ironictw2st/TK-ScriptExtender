# Campaign AI recruitment in TW:3K 1.7.2.0, and the script extender's policy

Audience: modders who know the 3K DB tables and Lua. Companion to `docs/SCRIPTING.md` sections
4c (read-only trace) and 4d (the policy). Working log: `notes/ai_recruitment.md`.

Labels used throughout: **[V]** verified by decompile or live measurement, **[I]** inference,
**[?]** open. Ghidra addresses are `0x140000000 + RVA`.

---

## 1. Summary

- **[V]** The AI's recruitment planner (`FUN_141cf8fe0`, vtable slot +0x58 of
  `CAI_TASK_RECRUITMENT_PREFERENCE_ANALYSIS`, task id 0xA2) prices **only empty retinue slots**,
  at most **3 slots per pass**, and records the *cheapest* and the average item per slot.
- **[V]** There is no upgrade or replace-unit command anywhere in the exe. An AI army never
  trades a bad unit for a good one through the planner.
- **[V]** Early-campaign trace: 206 AI faction turns, 3415 slots, 1067 empty, 664 purchases; the
  five most-bought units were the cheapest of each role (archer militia 57, sabre militia 46, ji
  militia 44, nanman spearmen 42, peasant band 37).
- **[V]** Mid-game save: 5905 slots, 868 empty; **1371 of the 5037 occupied slots (27%) had a
  same-role unit at least 1.25x better recruitable in that very slot, and all 1371 were
  affordable**. Median treasury over those 144 faction turns: 5240. Money is not the constraint.
- **[V]** 162 (early) and 171 (mid) purchases did land in slots that held a unit at turn start,
  but by the stock quality table they split 67 better / 52 worse / 8 same and 48 / 31 / 8: no
  quality direction, so this is composition churn or disband-then-refill, **not** an upgrade.
- The script extender policy (DLL 0.37, Lua only) runs at every AI faction's `FactionTurnStart`,
  fills empty slots with the best-scoring recruitable unit and replaces an occupied slot's unit
  when a candidate scores `min_gain` (default 1.5x) higher, at normal cost, inside a budget.
  score = AI quality (`cdir_military_generator_unit_qualities`, scaled by the old unit's
  experience) x element weight for the general's element x `duplicate_penalty ^ copies`.
- **[V]** First partial policy run (0.37.0-beta.2): 1192 orders (825 replace, 367 recruit),
  **0 failures**, 326,635 gold over 506 logged AI faction turns (median 536 per turn).

---

## 2. How the engine recruits

### The task and the planning pass

`CAI_TASK_RECRUITMENT_PREFERENCE_ANALYSIS` (vtable `0x1434bddc8`, task id `0xA2`, factory
`FUN_141da06e0`) owns the AI's recruitment intent. Its save-game constructor `FUN_141cce560`
loads four named sections: `::NEW_FORCE`, `::NEW_CHARACTER_RETINUE`, `::NEW_BASIC_RETINUES`,
`::IMPROVE_FORCE`. **[V]**

The planning pass is vtable slot +0x58 = `FUN_141cf8fe0(task, ctx)`, reached through
`FUN_141daf2e0 -> FUN_141ec55a0 -> FUN_141e32490 -> FUN_141d938d0` (no rel32 callers). Per pass it
gates on `FUN_141e39ec0(faction)`, frees the old request list, ages a per-force cooldown map
(task +0x158, 10 passes), computes two budgets - money = `min(FUN_141e5dbd0(faction),
floor(model vfunc +0xd0))` and a second budget = `floor(model vfunc +0x108(1))` - builds a current
and a desired composition vector blended by a CAI personality float, runs six sub-passes
(`FUN_141ce96b0`, `FUN_141ce8800`, `FUN_141ce6070`, `FUN_141ce77a0` only while task +0x188 == 0,
`FUN_141ce6700`, `FUN_141cebc60`), stamps the turn into every request and splits the leftover
budgets evenly. **[V]**

### Request layout (what `se_ai_recruit_trace` copies)

```
task +0x120  CAI faction object        +0x12c  request count   +0x130  requests
task +0x188  pending-order countdown

request (0x48 bytes)
  +0x00  target object (a CAI force / region wrapper)
  +0x18  i32 money budget      +0x1c  i32 second budget     +0x20  u32 turn stamp
  +0x28  u32 row count         +0x30  rows

row (0x1c bytes)
  +0x00 i32 id   +0x04 i32 cost   +0x08 i32 cost2   3 x i32   +0x18 u8 kind
```

Measured row: `{id 107, cost 840, cost2 228, kind 1}`. `id` rises with the faction id, so it is a
character or force id **[I]**; `cost` is recruitment money **[V]**; `cost2` was assumed to be
upkeep **[I]**, but the matching item field (+0x14) comes from a ctx vfunc +0x78 call with *no
unit argument*, so it is **not** per-unit upkeep **[V]**.

### The pricing helpers: why the AI never upgrades

`FUN_141ced9a0(out, retinue, money, budget2)` walks a retinue's slots and takes a slot only when

- slot vfunc +0x28 (the unit) **== 0**, i.e. the slot is empty,
- slot vfunc +0x60 (the recruitment interface) != 0, and
- the interface's vfunc +0x38 is false.

For each such slot it builds the recruit list (interface vfunc +0x88, flag 0), keeps items with
`reasons & 0xffffff5f == 0`, a record, `cost <= money` and `cost2 <= budget2`, records the
**cheapest** and the average per slot, sorts, and **stops after 3 slots**. `FUN_141cecf30` does
the same for a retinue template (`FUN_141931e10` with flag 1). No decompiled path reads an
occupied slot. **[V]**

The exe has no replace-unit command, and no CAI code touches the recruit command layer (the
type-id global at `0x14405aadc` is only read by UI serialisers). **[V]** So the AI must issue
recruitment by calling the model directly; **[?] that call site has not been found**.

### Why one recruit list is expensive (background)

`FUN_141934b40` (interface vfunc +0x88) -> `FUN_141931e10` builds a hash map over *all* of the
character's unit permission records, then, for every candidate unit and flag != 1 (what the AI
passes), rebuilds an equivalent map from the faction's list just to look up one unit and destroys
it: units x permissions per list, about 20 ms per list on a mid-game save. DLL 0.34.1's
permission-map sharing cut `FUN_141931e10` from 24.5% to 5.0% of main-thread time and turn 1 of a
campaign from 62 s to 39 s versus no DLL. Full detail in `notes/performance.md`.

### Data tables that matter

| Table | Verified content |
|---|---|
| `cdir_military_generator_unit_qualities` | Stock: **523 rows**, 405 distinct units, 18 role groups, quality 1 to 7900; 497 of 523 rows have `quality_at_max_xp` exactly **2x** `quality`. With the user's mod set the live table has **925 rows** and a 1.5x ratio. This is the AI's only unit ranking. |
| `cai_personalities_budget_allocations` | 10 rows; `army_funding_cap` = **2000** in 8 of them (0 in the two remaining), `army_funds_allocation_percentage` 50 or 60. |
| `cai_task_management_system_task_generator_groups_generators_junctions` | 664 rows. `CAI_TMS_TASK_GENERATOR_RECRUIT_HIGHER_QUALITY_UNITS` appears **6 times**, one per base group (`tms_aggressive`, `tms_decentralised`, `tms_default`, `tms_defensive`, `tms_passive`, `tms_turtle`), priority **100.0000**, empty variable group. The 133 `_easy` / `_hard` / `_late` groups are deltas and do not repeat it. `CAI_TMS_TASK_GENERATOR_RECRUIT` also covers `tms_major_no_regions` (7 rows). |
| `campaign_variables` | `retinue_slot_health_percent_reduction_when_replacing_a_unit` = **15**. |
| `retinue_slot_upgrade_nodes` | all unlocked, zero switching cost. |
| `military_generator_config_override` | empty for every personality. |

**[?]** `RECRUIT_HIGHER_QUALITY_UNITS` is enabled at full priority in every base TMS group, yet no
decompiled path prices an occupied slot and the traces show no quality direction in the
replacements. Whether the generator ever produces a task that does anything is unresolved.

---

## 3. What we measured

Method: DLL 0.36.x detours the planner and copies the request list after every pass while
`se.ai_recruit.trace(true)` is on. `se_ai_trace.lua` logs per AI faction turn a START line (every
slot and what it could recruit), a PLAN line (planner budgets) and a BOUGHT line per stock
`UnitRecruitmentInitiated`, tagged "empty slot", "REPLACES <old unit>" or "slot not seen at turn
start" from the slot cqi remembered at turn start. Numbers below: `tools\ai_trace_summary.py
<file>` against the stock quality TSV (mod units show as unknown quality).

| | early campaign (0.36.1) | mid game (0.36.2) |
|---|---|---|
| AI faction turns traced | 206 | 144 |
| retinue slots seen | 3415 | 5905 |
| empty at turn start | 1067 | 868 |
| occupied | 2348 | 5037 |
| occupied slots with a same-role unit >= 1.25x better recruitable | **388** | **1371 (27%)** |
| of those, affordable from the treasury | **388 (100%)** | **1371 (100%)** |
| purchases (`UnitRecruitmentInitiated`) | 664 | 298 |
| into a slot empty at turn start | 335 | 39 |
| into a slot not seen at turn start | 167 | 88 |
| into a slot that held a unit | 162 | 171 |
| those, by stock quality: better / worse / same / unknown | 67 / 52 / 8 / 35 | 48 / 31 / 8 / 84 |
| most bought | archer militia 57 (q 225), sabre militia 46 (q 450), ji militia 44 (q 450), nanman spearmen 42 (q 450), peasant band 37 (q 400) | nanman spearmen 25, nanman warriors 23, nanman slingers 19 (q 250), ji militia 11, peasant band 11 |

Mid-game detail (from the 971 slot lines the trace printed, capped at 12 per faction turn):
gap >= 2 in **840**, gap >= 3 in **574**, max gap **89.5**; the cost of the better unit had median
**294**, p90 **595**, max **1344**. Treasuries over those 144 faction turns: median **5240**,
below 2000 in **23**, above 20000 in **16**.

Reading: the AI buys the cheapest item of a role into empty slots, exactly as the decompile
predicts, and the "replaces a unit" purchases are directionless in quality, which rules them out
as an upgrade mechanism. **[I]** They are most likely composition changes or a disband followed
by a refill inside the same turn; separating the two needs a disband listener.

Caveat for anyone re-reading old logs: `se_unit_quality` in DLL 0.36.0 was **wrong** (the table
vector holds pointers to 0x20-byte records, not inline records), so every START line of the
0.36.0 run is garbage. Layout verified live: record `{vtable, group record*, land unit record*,
u32 quality, u32 quality_at_max_xp}`, vector at `table+0x10` = `{cap, count @+0x14, data @+0x18}`,
accessor `FUN_1408cad20(db)`, table pointer cached at `db+0x108e8`. **[V]**

---

## 4. The policy (DLL 0.37, Lua only)

All of this lives in `crates/script_extender/lua/se_api.lua` (embedded in the DLL, mirrored to
`<game>\lua_scripts\se_api.lua`). No new natives: it uses `se.query.retinue`,
`se.query.recruitable`, `se.query.unit_quality`, `se.modify.recruit`, `se.modify.replace`.

### Data flow per AI faction turn

```
core:add_listener("se_ai_recruit_turn_start", "FactionTurnStart", <not human and not dead>, ...)
  -> se.ai_recruit.policy or se.ai_recruit.plan(faction_key)   -- pure, changes nothing
       forces_of(faction)         military_force_list -> character_list -> {cqi, element}
       for each character:  se.query.retinue(cqi)              -- slots, unit_key, strength, xp
       for each wanted slot: se.query.recruitable(cqi, index)  -- the engine's own item list
       score every item, keep the best, emit a candidate
       sort, cut to budget and caps
  -> se.ai_recruit.execute(orders)
       replace -> se.modify.replace(char, slot, unit, {})      -- se_recruit_unit(iface, key, 0)
       recruit -> se.modify.recruit(char, unit, {slot = slot}) -- same native, empty slot
```

Both paths end in the engine's own recruitment routine on the slot's recruitment interface at
normal cost: `mode` is 0 unless the item is locked (bit 1) or `opts.free` (bit 2), and the policy
passes neither. The listener body runs inside the model callback, where `cm:can_modify()` is
true, so `se.on_model` executes immediately instead of queueing.

### Scoring

```
element_factor(general_element, unit_key)
    order = se.ai_recruit.element_order[general_element]
    rank  = first entry the unit matches ("element" always matches; "element:cavalry" only when
            se.ai_recruit.class_of(unit) == "cavalry")
    weight= element_weight[rank] when #element_weight >= #order, else evenly spread between
            element_weight[1] and element_weight[#element_weight]

old_score = (quality + max(0, quality_at_max_xp - quality) * min(xp, 9)/9)
            * element_factor(general_element, old_unit)

new_score = quality(candidate) * element_factor(general_element, candidate)
            * duplicate_penalty ^ (copies of that unit already in the retinue)
```

`quality` is the highest value over the unit's role groups, overridable per unit via
`se.ai_recruit.quality_override`; with `same_role_only = true` the candidate's quality is taken
only from a role group it *shares* with the old unit. General element = first element word of
`character_subtype_key()`; unit element = first element word of the unit key, overridable via
`se.ai_recruit.unit_element`. Either being unknown makes the factor 1.

Default preference lists (Earth and Fire generals rank Water **cavalry**, i.e. horse archers,
above the infantry elements, while Water infantry stays at the bottom):

```lua
wood  = { "wood", "metal", "water", "earth", "fire" }
metal = { "metal", "wood", "water", "earth", "fire" }
water = { "water", "earth", "metal", "wood", "fire" }
fire  = { "fire", "earth", "water:cavalry", "metal", "wood", "water" }
earth = { "earth", "fire", "water:cavalry", "water", "metal", "wood" }
element_weight = { 1.30, 1.15, 1.00, 0.90, 0.80 }
```

Five-entry lists use the weights directly. The six-entry fire and earth lists are longer than
`element_weight`, so `rank_weight` spreads them evenly between 1.30 and 0.80:
**1.30, 1.20, 1.10, 1.00, 0.90, 0.80**. `class_of(unit)` returns "cavalry" when any of the unit's
role groups in the live quality table contains "cavalry" (`Default_land_cavalry_melee` /
`_shock` / `_missile`), else "infantry"; `se.ai_recruit.unit_class` overrides it.

### Candidate selection, budget, caps, order

- A slot qualifies when it is not already recruiting, `can_recruit ~= false`, and it is either
  empty with `fill_empty`, or occupied with `replace` and `strength >= min_strength`.
- An occupied slot whose unit has **no known quality** is skipped (`old_score > 0` is required):
  unknown mod units are never thrown away.
- Items are skipped when `reasons ~= 0` (locked), when the key equals the current unit, or when
  `cost > budget`. Score ties break on the lower unit key.
- Empty slot: best score wins. Occupied slot: emit only when `best.score >= old_score * min_gain`.
- `budget = min(treasury - reserve, max(0, projected_net_income) * income_turns, max_spend)`
  = `min(treasury - 1500, income * 3, 4000)` by default; a faction below `min_income` does nothing.
- Sort: **all replacements before all empty-slot fills**, replacements by descending gain
  (`new/old`), empty slots by descending score, ties by character cqi then slot index. No clocks,
  no addresses, no `math.random`: every machine of a multiplayer game produces the same list.
- Caps applied while walking the sorted list: `max_per_character` 2, `max_per_force` 3,
  `max_per_faction` 8, at most 2 orders of the *same* unit key per character per turn, and the
  running budget must still cover the cost.

Deliberately skipped: units below `min_strength`, slots already recruiting, locked items,
unknown-quality units in occupied slots, the human faction and dead factions.

---

## 5. First results of the policy run (PARTIAL log)

Source: `notes/ai_traces/policy_run_turn1_partial_0.37.0-beta.2.txt`, a **partial** capture from a
campaign started at turn 1 with the user's full mod set. Only faction turns that produced at
least one order write a summary line, so zero-order faction turns are invisible here.

| Metric | Value |
|---|---|
| order lines parsed | 1192 (825 replace, 367 recruit) |
| failures (`-> false`) | **0** |
| logged AI faction turns | 506 over 100 distinct factions (median 5 logged turns per faction, so roughly 5 to 10 campaign turns) |
| total spent | 326,635 (order costs and the summary `spent` fields agree exactly) |
| cost per order | mean 274, median 244, max 1674; 50 orders cost 0 |
| budget per faction turn | mean 1515, median 1222, min 73, max 4000 |
| spent per faction turn | mean 646, median 536, max 3072 |
| candidates per faction turn | mean 6.5, median 6, max 34 |
| orders per faction turn | mean 2.36, max 8; only **1** turn hit `max_per_faction = 8` |
| distinct units placed | 131 |

Gain factor of the 825 replacements: min 1.50, p25 2.45, median **5.75**, p75 10.12, p90 20.99,
max 83.33. Buckets: 1.5-2x 13%, 2-3x 21%, 3-5x 11%, 5-10x 30%, >=10x 25%. The long tail is
militia sitting in armies that could recruit elites, exactly the 27% gap measured in section 3.

Most replaced: archer militia 119, ji militia 88, axe band 71, peasant band 68, sabre militia 44.
Most placed: nanman warriors 114, jian swordguards 94, repeating crossbowmen 76, jian swordguard
cavalry 63, wood spear warriors 51. Most common single swap: nanman slingers -> nanman warriors
(24), archer militia -> jian swordguards (23), axe band -> jian swordguards (19).

Element mix of the units placed: metal 42%, water 21%, wood 17%, earth 11%, fire 9%. Of the
replacements, 201 kept the old unit's element and 622 changed it. **This says nothing about
whether the element preference works**: the log line does not carry the general's element, so the
share of orders matching that general's list is not derivable from this file. The mix is
dominated by what is recruitable and high-quality in the early game. A verdict needs the
general's element logged per order (a one-line change to `execute`).

The 50 zero-cost orders are real free items in the slot's list, mostly faction-unique units
(`3k_main_unit_water_white_horse_raiders` 17, `ep_unit_metal_chu_infantry` 6).

---

## 6. Tuning guide

Console script with all knobs at the top: `<game>\lua_scripts\se_ai_recruit_rules.lua`. Edit,
LOAD once per session (the console caches script text until its panel is closed and reopened),
end turns, read the `[se] ai_recruit ...` lines in `lua_mod_log.txt`. Offline harness:
`C:\python311\python.exe tools\test_lua_aipolicy.py`.

| key | default | raise it | lower it |
|---|---|---|---|
| `fill_empty` | true | (bool) choose the unit for empty slots instead of leaving the engine's cheapest pick | false to only ever replace |
| `replace` | true | (bool) | false to only fill empty slots, i.e. a pure "better new units" mod |
| `min_gain` | 1.5 | fewer, more decisive swaps (3.0 only changes militia for elites) | 1.15 swaps constantly and eats the budget on marginal gains |
| `same_role_only` | false | true keeps army composition intact (spear for spear); recommended if the AI's armies become all-cavalry | false lets the best unit in the list win |
| `min_strength` | 50 | 80 protects damaged veterans | 0 replaces shattered units immediately (they lose the replenishment) |
| `max_per_character` / `max_per_force` / `max_per_faction` | 2 / 3 / 8 | faster re-arming, more turn time, bigger AI spend | 1 / 1 / 3 for a slow drip |
| `reserve` | 1500 | protects AI treasuries from being drained for war/diplomacy | 0 lets poor factions spend their last coin |
| `income_turns`, `max_spend` | 3, 4000 | richer AI armies | tie AI army quality to its economy |
| `min_income` | 0 | e.g. 200 stops bankrupt factions from upgrading at all | |
| `duplicate_penalty` | 0.92 | closer to 1.0 = happily 6 copies of the best unit | 0.7 forces variety |
| `log_orders` | true | per-order lines in the log | false for a quiet run |

**Stronger element themes.** Quality differences between 3K units are 2x to 10x, so the default
+-30% weights only decide between units of similar quality: a Fire general still takes a
6000-quality Wood unit over a 1200-quality Fire one. For visibly themed retinues widen the
spread; `{ 2.0, 1.4, 1.0, 0.6, 0.4 }` is a 5x best-to-worst multiplier, enough to beat most
quality gaps. Combine with `same_role_only = true` for themed *and* structurally sane armies.

**Tighter budgets** for a slower ramp:
`reserve = 4000, income_turns = 1, max_spend = 1200, max_per_faction = 3`.

**Mod units.** Three override tables, plain Lua, safe to set before or after `enable()`:

```lua
se.ai_recruit.quality_override["my_mod_unit"] = 2400   -- missing from cdir_military_generator_unit_qualities
se.ai_recruit.unit_element["my_mod_unit"]     = "metal" -- key does not contain an element word
se.ai_recruit.unit_class["my_mod_unit"]       = "cavalry" -- for "element:cavalry" entries
```

Without a `quality_override` a mod unit is invisible as a candidate, and a slot holding one is
skipped entirely.

**Custom policy.** `set_policy(fn)` replaces the decision maker; `fn(faction_key)` returns
`orders, info`. Call `plan` and edit the result, or build orders from scratch.

```lua
se.ai_recruit.set_policy(function(faction_key)
    local orders, info = se.ai_recruit.plan(faction_key)
    if faction_key == "3k_main_faction_gongsun_zan" then
        local keep = {}
        for _, o in ipairs(orders) do
            if se.ai_recruit.class_of(o.unit) == "cavalry" then keep[#keep + 1] = o end
        end
        return keep, info
    end
    return orders, info
end)
```

Order fields: `op` ("recruit" | "replace"), `force`, `character`, `slot`, `unit`, `cost`, `old`,
`score`, `old_score`, `gain`.

---

## 7. Limitations and risks

- **No per-unit upkeep.** The planner's second budget is not per-unit upkeep (verified) and the
  recruit item list exposes no upkeep field, so the guard is indirect: `projected_net_income` x
  `income_turns` plus the spend caps. An AI faction that upgrades into expensive units can still
  drift to a worse net income than vanilla.
- **Quality dominates the element weight** at the default spread (section 6).
- **The listener does not survive a load.** `se.ai_recruit.enable()` registers once per Lua
  state; re-run the console script (or call `enable()` from a first-tick hook) after every load.
  `disable()` leaves the listener registered but inert.
- **Lua cost per AI faction turn.** `plan` builds the recruit list for every candidate slot of
  every army, the ~20 ms routine above, so a big faction can add hundreds of milliseconds per
  turn. The read-only trace (`se_ai_trace.lua`) is heavier still and must not be left on.
- **Multiplayer.** The pass meets the project's lockstep rules (model callback only, no
  local-player branching, no clocks, addresses or `math.random`, fully tie-broken sort) but has
  **not** been tested on two machines. Any cfg key exposing it must enter `build::sync_tag`.
- **Saves.** Nothing new is stored, so saves stay loadable, but the armies are not what vanilla
  would have built. Test create -> save -> full restart -> load before relying on it.
- **Pre-release** 0.37.0-beta.2: the only live evidence is section 5, one campaign, one mod set,
  ~5 to 10 turns, no battle or difficulty outcome measured.
- Replacing a unit costs the slot 15% health
  (`retinue_slot_health_percent_reduction_when_replacing_a_unit`); the policy does not model it.

---

## 8. Open questions and next steps

1. **Where does the AI actually issue the recruit?** The planner only prices; no CAI code reaches
   the recruit command layer. Finding the model call would allow hooking the decision instead of
   overriding it afterwards.
2. **Does `RECRUIT_HIGHER_QUALITY_UNITS` do anything?** Priority 100 in all six base TMS groups,
   yet nothing observed corresponds to it. A breakpoint on its task factory would settle it.
3. **What are `id` and `kind` in a request row, and what is the target object?** 155 of 168
   traced requests had target vtable RVA `0x34c6c00`.
4. **Separate "REPLACES" purchases from disband-then-refill** with a disband listener, to confirm
   the engine truly never upgrades.
5. **Log the general's element per order**, so element preference can be verified from a run.
6. **Per-unit upkeep**: find a readable upkeep field on the recruit item or the unit record and
   budget against projected upkeep instead of a flat cap.
7. **Run the policy on a mid-game save to turn 20+** and measure AI army strength and end turn
   time against a no-policy control on the same save.
8. Sub-passes `FUN_141ce8800`, `FUN_141ce6700`, `FUN_141cebc60` are decompiled
   (`notes/decomp_cai_recruit2.txt`) but not analysed.
