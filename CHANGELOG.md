# Changelog

## Unreleased (local branch fix/ui-uaf-phase-guards, not tagged)

- **Two more engine crash guards** in `engine_crash_guards`:
  - `clear_selection`: clicking a court seat whose court-screen selection list was already freed (exe+0x55b7e0, mod-manager report
    "Clicked on court tab"; same site as a "loading into a save" report). The detour on ClearSelection (exe+0x55b770) skips a list
    that has no vtable in the exe, an impossible count or an unreadable array.
  - `phase_record`: a battle unit's ability-phase walk reading a phase record that is no longer mapped (exe+0x2326aa4, sandbox battle,
    general starting to rout). The stub clears that list entry's record and continues on the engine's own NULL path.
  - `se.diag.engine_guards()` gains `clear_selection`, `clear_selection_hits`, `phase_record`, `phase_record_hits`.
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
