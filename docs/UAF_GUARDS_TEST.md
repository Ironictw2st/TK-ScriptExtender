# In-game test: clear_selection, phase_record, payload_walk (branch fix/ui-uaf-phase-guards)

Not run yet. Needs a game slot. Build: `cargo build --release` in `Z:\Claude\ScriptExtender_uafguards`.
Test with `target\release\script_extender.dll` (+ injector), staged as a local test DLL. Never publish it.
All three guards are part of `engine_crash_guards` (default 1), which is in the multiplayer version lock.

## 1. Installed (any campaign, turn 1)
- The SE log has `engine crash guards installed: ... clear_selection, phase_record, payload_walk`. No
  `engine guard <name> not installed` line for the three. A "bytes differ" message there means another exe build.
- Console:
  ```lua
  local g = se.diag.engine_guards(); return g.clear_selection, g.phase_record, g.payload_walk,
    g.clear_selection_hits, g.phase_record_hits, g.payload_walk_hits
  ```
  Expect `true, true, true, 0, 0, 0`.
- `return se.diag.engine_guards_selftest()` -> `true` and a message containing
  `clear_selection: stale list skipped=true; phase_record: jump in place=true; payload_walk: stale container skipped=true`.
  After the selftest, `clear_selection_hits` and `payload_walk_hits` are 1 each. The selftest passes stale objects built
  inside the DLL; no game object is touched.

## 2. No regressions (the hit counters must stay at their post-selftest values)
- **Court:** open the court, select a post, stage a character (Confirm/Cancel shows), Cancel, stage again, Confirm.
  Switch Court / Family Tree / Characters / Offspring with an appointment staged, close and reopen the court, click
  seats. The appointment still lands and staging/unstaging still works. `clear_selection_hits` is unchanged.
- **Start-of-turn notifications:** end 2-3 turns as a faction that gets incidents/dilemmas with rewards, e.g. Cao Cao
  190. Every event message still lists its payloads (gold, effects, etc.), and the tooltips still show payload info.
  `payload_walk_hits` is unchanged.
- **Battle:** fight a battle with generals that have active abilities/auras (e.g. Lu Bu, Guan Yu) until one side
  routs. Ability buffs still apply and expire normally (unit stats panel). The battle ends normally.
  `phase_record_hits` is 0.

## 3. Crash paths (best effort; the original crashes are not reproducible on demand)
- **payload_walk:** the 190E Emishi 190 campaign, T1 -> T2, on a build WITHOUT wafinish's C099 data fix (baseline:
  4 of 5 runs hang or crash). With the guard, expect EITHER turn 2 to arrive with a log line
  `engine guard: payload info on a stale payload container (... #1); list left empty` and `payload_walk_hits >= 1`
  (one turn-start message shows no payload list), OR still a hang. **The guard cannot stop a hang** whose freed container
  still holds a plausible count and entries. The real fix is the C099 data fix; this guard is only a backstop.
- **clear_selection:** no known repro (exe+0x55b7e0; Epic player, not reproduced on our stack). If it fires in
  play, the log shows `engine guard: ClearSelection on a stale UI selection list (this ..., #n); skipped`, and the
  click clears nothing.
- **phase_record:** no known repro (exe+0x2326aa4; sandbox battle, general starting to rout). If it fires, the log
  shows nothing per hit (the counter is bumped by the stub itself); `phase_record_hits` > 0 after the battle and
  the battle continues.

## 4. Off switch
`engine_crash_guards=0` in script_extender.cfg -> none of the eight guards installed;
`se.diag.engine_guards()` shows all false.
