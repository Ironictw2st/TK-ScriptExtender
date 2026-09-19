# What the AI recruitment policy did to AI armies, campaign turns 1-50

Unattended 50-turn campaign, mod set "190Expanded" (~100 factions), DLL 0.37.0-beta.2, policy on
from turn 1 with `se_ai_recruit_rules.lua` at shipped defaults (min_gain 1.5, caps 2/3/8, reserve
1500, income_turns 3, max_spend 4000, duplicate_penalty 0.92, min_strength 50, same_role_only
false). Player faction `3k_dlc04_faction_liang_rebels` held passive. Sources:
`notes/ai_traces/policy_run_turn1_to_50_0.37.0-beta.2.txt` and `..._full.txt`. Background:
`docs/AI_RECRUITMENT.md`.

**Turn segmentation.** I did not use the `#TURN n` markers. Those mark the *human* faction's
`FactionTurnStart`, and the player is not first in the round order, so a `#TURN n` block mixes the
tail of turn n with the head of turn n+1. I segmented the full log on the mod's round autosave line
(`[190E mirror] snapshot N_N ... turn N`) instead, giving 50 clean campaign turns. The 104 order
lines before `#TURN 1` are turn-1 AI factions that acted before the player did, and are counted in
turn 1. The `[se_ai_rules] ... listening` line precedes every `ai_recruit` line, but turn 1 shows
only 55 acting factions against 73 and 69 in turns 2 and 3, so part of the turn-1 AI phase probably
ran before the console script loaded. **Read turn 1 as a lower bound; turns 2-50 are complete.**

---

## 1. Executive summary

- **3,069 orders** (2,257 replace, 812 empty-slot fill) over **1,369 AI faction turns**, **101
  factions**, **588 characters**, **2,301 retinue slots**. No order reported `-> false`.
- **812,624 gold** spent: mean 265 per order (median 232, p90 516, max 1,674), median 498 per
  acting faction turn, roughly 16,250 per campaign turn across the whole AI.
- **Money is almost never the binding constraint.** Median spend/budget 0.57; 11.4% of faction
  turns spent >=90% of budget; 7.5% hit the 4,000 `max_spend` cap. The real throttle is
  `max_per_character = 2`, hit in **43.3%** of character turns, plus candidate supply (80.4% of
  faction turns had more candidates than orders, and 35% of those spent under half their budget).
- **Activity tapers but does not stop.** Orders 700 (T1-5) -> 194 (T46-50), -72%; spend 185k ->
  44.5k; acting factions 96 -> 38. Replacements stay 65-70% of orders throughout: new armies and
  battle losses keep feeding the pass.
- **Placed quality roughly triples.** Median placed score 1,650 -> 4,274; share scoring >=4,000
  7.6% -> 52.1%. Median *removed* score barely moves (250 -> 319) - the AI keeps making militia.
- **Gains are large, not marginal:** median replacement x5.5, only 10.9% under x2. `min_gain = 1.5`
  is nowhere near binding.
- **95.8% of orders verified.** The DLL's `after 1 s slot N unit=` check finds the ordered unit in
  the intended slot for 2,940 of 3,069 (96.9% counting any slot of that character); the gap is
  mostly retinue re-ordering, 6 left the slot empty.
- **272 orders (8.9%) cost 0 gold, median score 5,607** - free elites, 80% of them in five factions
  (Dong Zhuo 65, Yuan Shu 47, Gongsun Zan 43, Ma Teng 36, Yuan Shao 27). Dong Zhuo issued 155
  orders for 4,319 gold total. Worth a look (section 4).
- **No scoring instability.** 27% of touched slots were touched again, but there are **zero**
  A->B-then-B->A flip-flops and **zero** second placements scoring below the first. Median gap 12
  turns, median second gain x3.6: a ratchet, not an oscillation.
- **Duplicate penalty 0.92 is too weak.** 467 character/unit pairs placed 3+ times, 315 placed 4+
  times, one character got 12 copies of one unit. Retinues are converging on a single key.

---

## 2. Volume over time

"fac-turns" = AI faction turns that produced at least one order (zero-order faction turns are not
logged, so the true denominator is unknown). "binding" = spent >=90% of the computed budget.

| turns | orders | replace | recruit | spend | spend/turn | fac-turns | factions | mean budget | med budget | mean spent | binding >=90% | budget = 4000 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1-5   | 700 | 455 | 245 | 185,003 | 37,001 | 301 | 96 | 1,348 | 1,033 | 615 | 42 (14%) | 16 |
| 6-10  | 492 | 370 | 122 | 141,632 | 28,326 | 205 | 83 | 1,759 | 1,574 | 691 | 22 (11%) | 22 |
| 11-15 | 374 | 290 |  84 | 104,390 | 20,878 | 161 | 67 | 1,541 | 1,233 | 648 | 27 (17%) |  9 |
| 16-20 | 258 | 190 |  68 |  66,950 | 13,390 | 118 | 60 | 1,474 | 1,102 | 567 | 17 (14%) | 11 |
| 21-25 | 263 | 210 |  53 |  62,359 | 12,472 | 117 | 57 | 1,602 | 1,208 | 533 |  7 (6%)  | 14 |
| 26-30 | 256 | 209 |  47 |  58,868 | 11,774 | 107 | 48 | 1,536 | 1,323 | 550 |  9 (8%)  |  6 |
| 31-35 | 208 | 156 |  52 |  56,198 | 11,240 | 107 | 44 | 1,467 | 1,125 | 525 |  7 (7%)  |  5 |
| 36-40 | 175 | 132 |  43 |  49,487 |  9,897 |  79 | 38 | 1,532 | 1,288 | 626 |  7 (9%)  |  4 |
| 41-45 | 149 | 111 |  38 |  43,265 |  8,653 |  80 | 38 | 1,379 | 1,115 | 541 |  7 (9%)  |  9 |
| 46-50 | 194 | 134 |  60 |  44,472 |  8,894 |  94 | 38 | 1,384 |   859 | 473 | 11 (12%) |  6 |

A decay, not a plateau, driven by **fewer factions acting** (96 -> 38), not by fewer orders per
acting faction (2.33 -> 2.06). Two causes are mixed and the log cannot separate them: conquest
(heavy early consolidation on a 100-faction map), and armies already upgraded to the best their
pool offers. Empty-slot fills collapse fastest (245 -> 60): by turn 10 most retinues are full.
Replacements hold at 20-45 per turn to the end, so the AI keeps producing new armies and new
militia for the policy to work on. Budgets *shrink* at the tail (median 1,033 -> 859, faction turns
with budget < 500 rising 12% -> 29%): the survivors are mostly poor minors.

---

## 3. What got replaced

2,257 replacements, 129 distinct units removed, 193 distinct placed. Top 12 pairs:

| n | old -> new |
|---|---|
| 58 | nanman_slingers -> nanman_warriors |
| 32 | water_archer_militia -> metal_jian_swordguards |
| 29 | metal_axe_band -> metal_jian_swordguards |
| 23 | water_archer_militia -> water_repeating_crossbowmen |
| 20 | wood_peasant_band -> metal_jian_swordguards |
| 18 | fire_mounted_lancer_militia -> fire_raider_cavalry |
| 18 | dlc05 wood_bandit_gang -> unit_metal_zu_lang_shanyue_bandits |
| 18 | wood_ji_militia -> water_repeating_crossbowmen |
| 15 | water_archer_militia -> fire_raider_cavalry |
| 15 | wood_peasant_band -> unit_metal_zu_lang_shanyue_bandits |
| 15 | nanman_warriors -> nanman_wuling_fighters |
| 15 | water_archer_militia -> ep_unit_metal_chu_infantry |

| n | most removed (stock q) | n | most placed (stock q) |
|---|---|---|---|
| 262 | water_archer_militia (225) | 235 | dlc06 nanman_warriors (480) |
| 155 | wood_peasant_band (400) | 148 | metal_jian_swordguards (875) |
| 150 | wood_ji_militia (450) | 124 | unit_metal_zu_lang_shanyue_bandits (mod) |
| 126 | metal_axe_band (450) | 117 | rew_iro_regional_yang_wood_yang_guardians (mod) |
|  99 | metal_sabre_militia (450) | 114 | water_repeating_crossbowmen (440) |
|  75 | earth_mounted_sabre_militia (1000) | 81 | rew_iro_regional_jing_earth_northern_nobles (mod) |
|  73 | water_repeating_crossbowmen (440) | 79 | earth_jian_swordguard_cavalry (2000) |
|  69 | fire_mounted_lancer_militia (1100) | 75 | ep_unit_metal_chu_infantry (2300) |
|  66 | nanman_slingers (250) | 66 | fire_raider_cavalry (1000) |
|  53 | dlc05 wood_bandit_gang (2000) | 61 | 3k_ironic_unit_metal_untried_blades (mod) |

`water_repeating_crossbowmen` and `metal_jian_swordguards` appear on both lists: the turn-2-to-10
upgrade target becomes the turn-15+ victim. **49.5% of all placements are units absent from the
stock quality table**, i.e. mod units, rising from 28% in turns 1-5 to 55-70% after turn 20. The
regional `rew_iro_regional_*` units are the mid-game ceiling.

Gain factor (new_score / old_score) per bucket:

| turns | n | p25 | median | p75 | p90 | max | >= x3 |
|---|---|---|---|---|---|---|---|
| 1-5   | 455 | 2.45 | 6.00 | 10.12 | 21.73 |  75.0 | 68% |
| 6-10  | 370 | 2.45 | 5.14 |  9.92 | 20.03 |  83.3 | 64% |
| 11-15 | 290 | 3.27 | 6.61 | 15.83 | 26.15 |  67.5 | 81% |
| 16-20 | 190 | 2.42 | 4.24 | 15.23 | 27.60 |  74.6 | 65% |
| 21-25 | 210 | 2.95 | 5.53 | 18.22 | 30.81 |  84.4 | 75% |
| 26-30 | 209 | 2.57 | 5.66 | 17.02 | 34.67 |  95.8 | 69% |
| 31-35 | 156 | 2.53 | 4.77 | 12.03 | 24.00 |  76.7 | 66% |
| 36-40 | 132 | 2.26 | 3.67 | 13.63 | 29.55 | 104.9 | 58% |
| 41-45 | 111 | 2.36 | 4.09 | 15.33 | 27.19 |  95.8 | 64% |
| 46-50 | 134 | 2.61 | 6.02 | 20.72 | 35.24 | 104.9 | 66% |

The gain distribution is **stable across 50 turns** and the p90 rises. Armies are *not* converging
on the top of the roster: the median removed score sits at 250-400 from turn 1 to turn 50 while the
median placed score climbs. The AI keeps re-acquiring militia (its own planner buys the cheapest
item for every empty slot, and every new army starts with the starting roster), so the policy has a
permanent supply of x5-to-x20 swaps. A treadmill, not a completed upgrade.

---

## 4. Quality trajectory

Scores are the log's own values (quality x element weight x duplicate penalty; `was` also includes
the old unit's experience scaling), so they reflect the mod's live quality table.

| turns | placed mean | placed median | placed p90 | removed mean | removed median | militia-tier placed | militia-tier removed | score >= 2000 | score >= 4000 | zero cost |
|---|---|---|---|---|---|---|---|---|---|---|
| 1-5   | 1,620 | 1,650 | 3,501 |   299 | 250 |  9.4% | 77.8% | 29.9% |  7.6% |  3.6% |
| 6-10  | 2,138 | 1,693 | 5,520 |   467 | 250 | 11.0% | 59.7% | 35.0% | 14.8% |  5.1% |
| 11-15 | 3,675 | 3,237 | 7,176 |   793 | 325 |  9.1% | 51.7% | 63.9% | 45.7% |  2.9% |
| 16-20 | 3,083 | 2,200 | 6,900 |   822 | 325 |  8.1% | 52.1% | 53.5% | 34.1% |  5.0% |
| 21-25 | 3,547 | 2,392 | 7,176 |   795 | 416 |  6.8% | 43.8% | 60.5% | 42.6% |  6.8% |
| 26-30 | 3,930 | 3,934 | 7,176 |   813 | 325 | 10.9% | 50.2% | 67.2% | 49.6% | 20.3% |
| 31-35 | 3,647 | 2,990 | 7,380 |   850 | 334 |  7.7% | 47.4% | 64.4% | 42.8% | 20.2% |
| 36-40 | 3,993 | 4,548 | 7,200 | 1,130 | 406 | 10.3% | 48.5% | 69.1% | 53.1% | 14.9% |
| 41-45 | 3,538 | 3,439 | 6,662 |   826 | 306 |  6.0% | 46.8% | 66.4% | 43.0% |  9.4% |
| 46-50 | 4,099 | 4,274 | 7,193 | 1,112 | 319 |  4.1% | 47.8% | 73.7% | 52.1% | 23.7% |

"militia-tier" = key contains militia / peasant / band / levy / conscript. The policy places almost
no militia (9.4% -> 4.1%, mostly empty-slot fills in poor factions with nothing better
recruitable) and is still removing militia in half of all replacements at turn 50.

### Zero-cost placements

272 orders (8.9%), 38 distinct units, **median score 5,607, 244 of 272 scoring >= 2,000**, max
8,775; frequency rises sharply after turn 25 (20-24% of orders in three of the last five buckets).
Top free units: `ep_unit_metal_chu_infantry` 29 (median score 6,000),
`rew_iro_regional_sili_wood_brave_protectors` 29, `rew_iro_regional_jing_earth_northern_nobles` 18
(7,038), `3k_main_unit_water_white_horse_raiders` 17,
`rew_iro_regional_ji_metal_jizhou_elite_swordguards` 16, `3k_main_unit_earth_qiang_raiders` 9
(6,900), `3k_main_unit_fire_heavy_xiliang_cavalry` 8 (6,613).

**Flag for the modder.** Not a policy bug - the engine's own recruit item list reported cost 0, so
a recruitment-cost reduction is reaching or exceeding 100% for those units in those factions. The
consequence differs from vanilla: because the budget never bites on a free unit, Dong Zhuo issued
155 orders (the most of any faction) for 4,319 gold total - 28 gold per order - while fielding
score-8,775 Jade Dragons. Vanilla's planner would also buy them cheaply, but it only fills *empty*
slots with the *cheapest* item, so it would never convert a whole retinue. Check
`unit_recruitment_cost` effect bundles for Xiliang and the Yuan factions.

---

## 5. Element and class mix

| turns | wood | metal | water | earth | fire | cavalry |
|---|---|---|---|---|---|---|
| 1-5   | 15.9% | 42.4% | 23.1% |  8.9% | 9.7% | 20.7% |
| 6-10  | 17.7% | 42.5% | 18.9% | 13.6% | 7.3% | 23.6% |
| 11-15 | 23.8% | 48.9% | 11.5% |  8.0% | 7.8% | 10.2% |
| 16-20 | 14.7% | 51.2% | 21.3% |  5.8% | 5.8% | 16.3% |
| 21-25 | 16.3% | 40.3% | 24.7% | 10.3% | 7.2% | 16.7% |
| 26-30 | 21.9% | 42.6% | 21.1% |  6.6% | 7.0% | 11.3% |
| 31-35 | 30.8% | 38.9% | 17.8% |  4.3% | 6.7% | 11.5% |
| 36-40 | 26.3% | 40.0% | 13.1% | 15.4% | 5.1% | 14.9% |
| 41-45 | 18.8% | 42.3% | 21.5% | 12.1% | 5.4% | 19.5% |
| 46-50 | 16.5% | 50.0% | 21.6% | 10.3% | 0.5% |  9.3% |

Rules used: element = first `_`-separated token among wood/metal/water/earth/fire in the key (13 of
3,069 placements have none); class = cavalry if the key contains
cavalry/horse/rider/lancer/cataphract/raiders or any stock quality group of that unit contains
"cavalry".

Overall: metal 43.9%, water 19.7%, wood 19.4%, earth 9.5%, fire 7.1%; cavalry 16.7% of placements
against 18.7% of removals. **509 of 2,257 replacements (22.6%) keep the old unit's element**; metal
is the largest sink (318 water -> metal, 250 wood -> metal). Class transitions: inf->inf 1,536,
cav->inf 307, inf->cav 299, cav->cav 115 - a net cavalry change of -8 units over 50 turns, so the
policy is **not** building all-cavalry doomstacks. Water cavalry (the `water:cavalry` entry fire
and earth generals rank third) appears in **71 placements, 2.3% of orders**:
`ep_unit_water_xianbei_horse_archers` 28, `white_horse_raiders` 22, `water_qiang_hunters` 11,
`white_horse_fellows` 7. Present, but a niche.

**The general's element is not in this log** (the build that logs it per order is newer), so "did
the policy follow the general's preference" **cannot be answered from this run**. Indirect
evidence: characters with >= 4 placements (n = 371) have a mean dominant-element share of
**0.787**, and 40% of them received a single element for every placement; a within-faction
permutation control (shuffling element labels among that faction's own placements, 200 draws)
gives **0.688, sd 0.004**. The observed concentration is ~25 sd above baseline, so something is
per-character, not just per-faction - consistent with the element weight working, but equally
consistent with per-character regional recruitment pools. This data cannot separate them.

---

## 6. Per-faction view

101 factions acted; 1,369 of roughly 5,000 possible AI faction turns produced orders (27%); mean
8,046 gold per acting faction over the run.

| faction | orders | spend | acting turns | gold/turn | first | last |
|---|---|---|---|---|---|---|
| 3k_main_faction_dong_zhuo | 155 | 4,319 | 32 | 135 | T1 | T50 |
| 3k_main_faction_yuan_shu | 93 | 12,860 | 30 | 429 | T1 | T50 |
| 3k_main_faction_gongsun_zan | 71 | 4,041 | 18 | 224 | T1 | T46 |
| 3k_dlc05_faction_white_tiger_yan | 68 | 5,430 | 22 | 247 | T1 | T48 |
| 3k_main_faction_liu_biao | 66 | 23,127 | 17 | 1,360 | T2 | T44 |
| 3k_main_faction_yuan_shao | 62 | 6,514 | 24 | 271 | T1 | T42 |
| 3k_main_faction_liu_yan | 59 | 11,672 | 15 | 778 | T1 | T25 |
| 3k_dlc05_faction_yang_feng | 59 | 26,145 | 23 | 1,137 | T1 | T50 |
| 3k_main_faction_zhang_yan | 57 | 8,901 | 24 | 371 | T6 | T48 |
| ironic_faction_zhuge_xuan | 56 | 10,561 | 23 | 459 | T1 | T50 |

Top spenders: yang_feng 26,145; liu_biao 23,127; ironic_faction_zhang_xian 16,637;
3k_dlc06_faction_jiuzhen 16,570; kong_rong 16,129; nanman_king_shamoke 15,952; sun_jian 15,834.
The two orderings differ sharply: Dong Zhuo and Gongsun Zan top the order list on free units, while
Liu Biao and Yang Feng pay 1,100-1,400 per turn.

**No faction spends the cap every turn.** 102 faction turns (7.5%) had `budget == 4000`, led by
Dong Zhuo (19), Liu Biao (12), Shi Xie (8), Tao Qian (8). The highest sustained spend/budget ratios
over >= 8 acting turns are `ironic_faction_wang_sheng` 0.84, `sheng_xian` 0.79, `cai_mao` 0.78,
`huang_zu` 0.77 - nobody is pinned against the budget.

**49 of 101 factions stop acting by turn 40** (last-acting turn: 8 in T1-5, 7 in T6-10, 5 in
T11-15, 4 in T16-20, 7 in T21-25, 6 in T26-30, 6 in T31-35, 6 in T36-40, 14 in T41-45, 38 still
acting in T46-50). The earliest exits are Nanman minors (tu_an, jianning, yongchang, zangke all
last act T4-T5) and Yellow Turban splinters (anding T4, taishan T7, rebels T8) - consistent with
conquest. The log carries no faction-death event, so "stopped acting" cannot be separated from
"went broke" or "had no x1.5 candidate". Cao Cao is the interesting case: 19 orders, 865 gold, last
order T23, still alive - he ran out of x1.5 upgrades in his pool.

Characters with the most orders:

| char | faction | orders | turns touched | converted to |
|---|---|---|---|---|
| 136 | dong_zhuo | 22 | 14 | yong_metal_defenders_of_capitals x8, fire_raider_cavalry x6, ep_metal_chu_infantry x5, heavy_xiliang_cavalry x3 |
| 925 | dlc05 shi_huang | 21 | 12 | water_repeating_crossbowmen x11, jiao_wood_headcrushers x4, shi_huang_metal_cangwu_axes x4 |
| 304 | dlc04 prince_liu_chong | 19 | 12 | wood_spear_warriors x7, yu_metal_yuzhou_assault_infantry x6, dlc04 water_chen_royal_guard x5 |
| 469 | wang_lang | 18 | 10 | water_repeating_crossbowmen x6, earth_shanyue_volunteer_cavalry x6, yang_wood_yang_guardians x6 |
| 711 | kong_zhou | 17 | 11 | unit_water_ironic_scholar_archers x11, water_repeating_crossbowmen x5 |

Char 136's retinue was rebuilt about twice over; char 711 ended up with eleven copies of one archer
unit.

---

## 7. Repeated churn

- 2,301 distinct (character, slot) pairs touched; **622 (27.0%) touched more than once**, 123 more
  than twice, maximum 5.
- 768 orders (25.0% of all) landed in an already-touched slot, costing **213,616 gold (26.3% of
  spend)**.
- Median gap between two orders on the same slot: **12 turns** (mean 14.5); 30 pairs 1 turn apart,
  122 within 3, **none within the same campaign turn**.
- Of the 768, **447 (58%) removed the unit the policy itself had placed there**; the other 321
  found something else in the slot (engine recruitment, or a loss and refill between passes).
- **Zero A -> B then B -> A flip-flops; zero second placements scoring below the first.** Median
  second/first score ratio **x3.59**. The median `was` at the second order is only 1.17x the score
  the unit was placed at, so experience scaling is not inflating old scores enough to matter.

So there is **no scoring instability** - no duplicate-penalty or experience-scaling loop making the
policy undo itself. Even the fastest self-undos are genuine jumps:

```
char 1075 (zhu_hao) slot 1: T7 water_archer_militia -> wood_spear_warriors (score 598)
                            T8 wood_spear_warriors  -> yang_wood_yang_guardians (was 650, score 7800, x12.00)
char 214  (liu_biao) slot 1: T7 earth_mounted_sabre_militia -> earth_jian_swordguard_cavalry (1397)
                             T9 earth_jian_swordguard_cavalry -> jing_earth_northern_nobles (was 1950, score 7800, x4.00)
```

These are unlock ramps: a new building or region makes a much better unit recruitable a turn or two
after the first upgrade. Tightening `min_gain` will not stop them (x3 to x12); only a per-slot
cooldown would, and the policy has none.

The one pattern worth a look is regression after losses - four orders re-placed a unit previously
removed from that same slot, all on char 925 (Shi Huang):

```
char 925 slot 3: T3 (empty) -> repeating_crossbowmen | T12 repeating_crossbowmen -> jiao_wood_headcrushers
                 T24 (empty) -> repeating_crossbowmen | T28 repeating_crossbowmen -> shi_huang_metal_cangwu_axes
```

The slot was empty at T24, so the headcrushers were destroyed and the best then-recruitable item
had fallen back to crossbowmen: the policy paying twice for the same upgrade after a battle loss,
not a scoring fault - but that is where the 26% repeat spend goes.

Separate concern: **duplicate saturation.** 467 character/unit pairs were placed 3+ times and 315
were placed 4+ times, topped by char 326 with 12 copies of `unit_metal_zu_lang_shanyue_bandits`.
`duplicate_penalty ^ copies` at 0.92 gives 0.66 after five copies - not enough to beat a x2-x5
quality gap. Retinues are becoming mono-unit.

---

## 8. Comparison with the vanilla baselines

| | vanilla early (0.36.1) | vanilla mid (0.36.2) | policy run T1-50 |
|---|---|---|---|
| purchases / orders | 664 over 206 faction turns | 298 over 144 faction turns | 3,069 over 1,369 faction turns |
| into an occupied slot | 162 (directionless: 67 better / 52 worse / 8 same) | 171 (48 / 31 / 8) | 2,257, **100% better by construction**, median x5.5 |
| top buys | archer_militia 57, sabre_militia 46, ji_militia 44, nanman_spearmen 42, peasant_band 37 | nanman_spearmen 25, nanman_warriors 23, nanman_slingers 19 | nanman_warriors 235, jian_swordguards 148, zu_lang_shanyue_bandits 124, yang_guardians 117 |
| stock quality of top buy | 225 | 450 | live score median 2,000, p75 5,078 |
| upgradable occupied slots left on the table | 388 of 2,348 (all affordable) | 1,371 of 5,037 = 27% (all affordable) | - |

Vanilla's five most-bought units are the cheapest item of each role, exactly as the decompile of
`FUN_141ced9a0` predicts. Under the policy those same keys are the **removed** list (archer_militia
262, peasant_band 155, ji_militia 150, axe_band 126, sabre_militia 99) while the placed list is
regional/elite. The 27% "affordable upgrade sitting unused" measured on the mid-game save is what
the 2,257 replacements consume.

---

## 9. Risks and tuning

1. **The 4,000 cap and the 1,500 reserve are not the limiting factor** - median spend/budget 0.57,
   7.5% of faction turns at the cap. What the policy does *not* model is upkeep: 3,069 higher-tier
   units now sit in AI armies unaccounted for. **Check in game:** AI treasuries and net income at
   turn 30-40 against a no-policy control on the same save. If factions go negative the lever is
   `income_turns` (3 -> 1), not `max_spend`.
2. **Zero-cost elites.** 8.9% of orders, ~24% late, median score 5,607, 80% in five factions. Check
   the recruitment-cost effects for Dong Zhuo / Ma Teng / Gongsun Zan / the Yuan factions. If they
   are intended, guard the policy: price cost-0 items at a nominal amount so the budget still
   rations them, or exclude the specific keys.
3. **Elite saturation and mono-unit retinues.** `duplicate_penalty = 0.92` is too weak. Move to
   **0.75-0.80** (0.75^4 = 0.32, enough to beat a x3 gap by the fourth copy), optionally with
   `same_role_only = true`. Single change with the most visible in-game effect.
4. **Churn costs 26% of spend.** Median 12 turns between orders on the same slot, so a 5-8 turn
   per-slot cooldown would remove very little real upgrading. The cheap version is `min_gain`
   1.5 -> **3.0**: drops 714 of 2,257 replacements (31.6%) and 190,555 gold (23.4% of spend) while
   keeping every x3+ jump. 2.0 only removes 10.9% and is not worth it.
5. **Throughput is capped per character, not by money.** `max_per_character = 2` bound 43.3% of
   character turns; `max_per_faction = 8` bound 9 faction turns in 50. For a slower drip lower
   `max_per_character` to 1; lowering `max_per_faction` does almost nothing.
6. **Replacement health cost.** All 2,257 replacements cost the slot 15%
   (`retinue_slot_health_percent_reduction_when_replacing_a_unit`), unmodelled. With
   `min_strength = 50` a unit can be replaced at 50% and land at 35%. If AI armies look chronically
   under-strength, raise `min_strength` to 80.
7. **4% of orders silently did not land** in the intended slot despite the engine returning true
   (129 of 3,069) - mostly retinue re-ordering, 6 left the slot empty. Harmless, but order counts
   overstate army changes by a few percent.

---

## 10. Limitations of this data, and what to log next time

- **The general's element is absent**, so preference-following is unmeasurable (section 5). Biggest
  single gap.
- **Zero-order faction turns are not logged.** Every "per faction turn" mean is conditional on at
  least one order, so budgets and spends are biased upward, and "stopped acting" cannot be told
  apart from "died", "fell below `min_income`" and "had no x1.5 candidate".
- **No treasury, income or upkeep** anywhere in the log. The `budget` field is the only economic
  signal and is already the min of three terms, so it cannot be inverted back to a treasury.
- **No army strength, battle outcomes or unit counts**, and no no-policy control campaign on the
  same seed and mod set. Nothing here shows whether the AI actually fights better.
- **Stock quality is only approximate** (523 rows vs 925 live) and 49.5% of placed units are absent
  from it, so all quality statements use the log's own `score` / `was` values.
- **Score is not comparable across characters** (it embeds element weight and duplicate penalty),
  so cross-faction score comparisons are indicative only.
- One campaign, one mod set, one seed, 50 turns.

Next run should log per order: the general's element and subtype key, the unit's role group, and
the faction treasury and projected net income at the start of the pass. Log a faction-turn line
even when zero orders were issued, with the candidate count and why the list was empty. Every 5
turns, dump an army snapshot (per faction: forces, filled slots, mean unit score) so the quality
trajectory can be measured on armies instead of on orders.
