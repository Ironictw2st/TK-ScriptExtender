# Changelog

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
