# Changelog

## 0.48.1 (unreleased; branch fix/guards-1112, test builds 0.48.1-test.N)

- **Three more engine crash guards** in `engine_crash_guards`, each with its own off switch (all in the version lock):
  - `move_faction` (cfg `guard_move_faction`): `modify_character:move_to_faction_and_make_recruited(key)` on a character in a
    recruitment pool with a faction key that is not in the campaign crashed the game (exe+0x17c60d0; 5 identical reports from
    one player on 0.48.0, the 3-5 Nov End Turn reports). The engine routine (FUN_1415ec970) checks the faction for the court
    move only and then uses it in the pool branch. A stub at exe+0x15ec9a8 sends a missing faction to the routine's own exit:
    the move does nothing. A known key moves exactly as before.
  - `cco_list_walk` (cfg `guard_cco_list_walk`): a UI list getter (FUN_142f99bf0) walking an engine list that was unlinked
    while the UI refreshed, early end turns (exe+0x1c254d4; 2 player reports, seen in-house 5 and 8 Nov). Stubs at the loop
    entry (exe+0x2f99c2b) and bottom (exe+0x2f99d50) end the walk on the function's own exits when a node is NULL, below
    0x10000, misaligned or not a user address: the panel gets the entries read so far. A freed node that still looks like a
    valid pointer is not caught.
  - `garrison_holder` (cfg `guard_garrison_holder`): applying a battle result (Delegate, or the end of a fought battle)
    abandoned a region with no owner or a dead owner and crashed while removing its garrison (exe+0x1a70740; 7 identical
    reports from one player on 0.48.0, siege of San Pass). FUN_141778690 detaches the unit and calls FUN_141464f10 on its force,
    which uses the force's holder without a check; the holder was already gone. A stub at exe+0x1778716 makes that call only
    when the force and its holder exist, otherwise it skips it (the unit is already detached; skipped: the force stat refresh
    and the force-changed / unit-removed notifications, all of which crashed before).
  - `se.diag.engine_guards()` gains `move_faction`, `move_faction_hits`, `cco_list_walk`, `cco_list_walk_hits`,
    `garrison_holder`, `garrison_holder_hits`;
    `se.diag.engine_guards_selftest()` checks the move jump and calls the list getter on test objects (NULL head stopped,
    empty list unchanged) and the garrison jump + stub bytes. A guard switched off in the cfg is reported as `off (cfg)`,
    not as a failure.
  - Unit tests: site pins, jump layout, and execution tests that run all three stubs on every path.

## 0.48.0 (stable, 2026-11-11)

Stable release of everything since 0.46.0: the 0.47.0-beta.1 to beta.3 pre-releases plus the following (built and
game-tested as 0.47.0-beta.4; only the version number changed for 0.48.0).

- **Smarter campaign AI (AI factions only; settings in the 190E "AI Campaign" MCT tab).** The AI's own scores are nudged,
  never replaced, so its choices stay its own:
  - Equipment: items go to the character they suit (role, element, ranged abilities such as Warning Shot or volleys).
    A named item (e.g. Huang Zhong's bow) stays with its owner: anyone else values it at the minimum.
  - Court posts: posts go to characters with the matching skills.
  - Skills: the skill-pick nudge is in but OFF by default (slider 100): a same-save test showed no role-fit gain yet.
  - New Lua natives `se_db_rec_dump`, `se_db_rec_set_i32`, `se_db_rec_restore_all`: read / change loaded DB records
    (the AI Campaign tab uses them for the AI's construction budget; the original values come back when the tab is off).
  - Verified in game: equipment fit (Liu Biao keeps his hands off the bow), the construction budget (40 % doubled the AI
    provinces that build in a 5-turn test), and 30 AI turns clean with all nudges at x1.5.
- Engine crash guards (below) were merged from fix/ui-uaf-phase-guards.

### Engine crash guards

- **Three more engine crash guards** in `engine_crash_guards`:
  - `clear_selection`: clicking a court seat whose court-screen selection list was already freed (exe+0x55b7e0, mod-manager report
    "Clicked on court tab"; same site as a "loading into a save" report). The detour on ClearSelection (exe+0x55b770) skips a list
    that has no vtable in the exe, an impossible count or an unreadable array.
  - `phase_record`: a battle unit's ability-phase walk reading a phase record that is no longer mapped (exe+0x2326aa4, sandbox battle,
    general starting to rout). The stub clears that list entry's record and continues on the engine's own NULL path.
  - `payload_walk`: the event UI's payload list (CcoCampaignPayloadInfo, FUN_1419f2370) walking a freed payload container
    (exe+0x19f23c2 / 0x19f23cd: "closing start of turn notifications" reports, and the 190E Emishi turn 1->2 hang/crash). The
    detour skips the walk when the container, its count (at most 0x400), its array or any payload (readable, vtable in the exe)
    is implausible, so the message shows no payload list instead of crashing. **It cannot stop a hang** when the freed
    container still holds a plausible count and plausible entries, and it is not the Emishi fix: that is the data fix to
    EVD C099's CREATE_AUTOMATIC_DIPLOMACY_DEAL payloads (190E, in progress).
  - `se.diag.engine_guards()` gains `clear_selection`, `clear_selection_hits`, `phase_record`, `phase_record_hits`,
    `payload_walk`, `payload_walk_hits`.
  - Unit tests: decision tests, stub layout, and an execution test that runs the phase stub on both paths. NOT run in game.

## 0.47.0-beta.3 (pre-release; content of 0.47.0-beta.2 plus the following)

- **AI armies now assault settlements they are sitting outside** (single player; cfg `siege_force_assault`, on by
  default, in the version lock). At each AI faction's turn start, an army that has besieged for at least one turn
  assaults when the odds are good: unwalled settlements from 1.5x, walled ones only from 6x raw strength and with
  finished siege equipment (otherwise it queues a ram). It may still wait a turn: 25 % at 1.5-3x, 10 % at 3-6x,
  2.5 % from 6x (a fixed roll from turn, army and target, the same on reload). A stance that blocks attacks is reset
  to the default stance first. Human factions are never touched; multiplayer campaigns skip it (not yet verified in
  lockstep). Counters: `se.diag.siege()`; Lua `se.siege.*`.
- **Battle crash guards**: null checks for the unchecked `weak_ptr.lock()` in the battle effect/audio handle updater
  (exe+0x26a3e68, exe+0x26a47ea and siblings), added to `engine_crash_guards` (`weak_lock`).
- **Engine crash guards** (cfg `engine_crash_guards`, on): four null guards for crashes seen in mod-manager reports
  (inactive string, CEO owner, text font, building browser).
- **CAI null guard** (cfg `cai_null_guard`, on): null checks on three AI call sites.
- **Faster battle instance transform** (cfg `battle_xform_fast`, on): bit-identical, unrolled routine
  (`se.diag.sepatches`).
- The experimental `siege_odds_gate` (odds-scaled AI assault roll) is now opt-in and off by default: its first
  version crashed on the first AI turn.
- **Credit: Sleeping Rowlet**, whose packs (`z_ngc_cai_null_guard.pack`, `z_ngc_battle_crash_fix.pack`) gave the
  ideas for the CAI null guard and the battle transform (own code).

Tested 6 Nov 2026 on 190E (user's mod stack): all 60 patches installed, 3 AI turns without a crash, a forced assault
captured Shangdang, battle transform 10,791 calls with 0 fallbacks.

## 0.46.0-siege.5 (branch feat/siege-assault, not released)

- **AI siege assault** (cfg `siege_force_assault`, default on, in the version lock): at an AI
  faction's turn start, besieging armies assault when the odds are good: unwalled from 1.5x,
  walled from 6x raw strength with finished siege equipment (otherwise a ram is queued); wait
  chance 25 % / 10 % / 2.5 % by odds band, deterministic roll; a blocking stance is reset to
  DEFAULT. Single player only for now. Natives `se_siege_assault`, `se_siege_equipment`;
  Lua `se.siege.*`; counters in `se.diag.siege()`. Tested 5 Nov 2026: Wang Kuang took
  Shangdang by assault; Dong Zhuo at walled Chang'an (3.8x) correctly left alone.
- `siege_odds_gate` (0.46.0-siege.1) is now opt-in: its stub crashed on the first AI turn
  (writes into an execute-read page). Do not enable until fixed.
