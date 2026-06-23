# screeps-combat-decision

> The tactical seam plus the pure combat decisions — one tactics brain shared by the live Screeps bot and the self-play sim, with no fork.

`screeps-combat-decision` is the JS-free boundary that lets a Screeps bot's *real* combat
decision code run in two places without duplication: on the live server (over `game::*`) and
inside an in-process combat micro-sim (`screeps-combat-engine`). A decision reads a
[`CombatView`] — value-type DTOs, never live game objects — and emits [`CombatIntent`]s, so there
is exactly one implementation of target selection, focus-fire, healing, kiting, and squad
engage/retreat logic. It is a component extracted from the
[screeps-ibex](https://github.com/Azaril/screeps-ibex) workspace and is the second layer of the
combat crate family (engine → **decision** → agent/eval).

The crate boundary mechanically enforces the design rule "no `game::*` below the seam": this crate
cannot reach the live game at all. The live bot and the sim each supply a thin per-tick adapter
that builds the view; the pure functions here do the rest.

## Installation

Add it as a git dependency:

```toml
[dependencies]
screeps-combat-decision = { git = "https://github.com/Azaril/screeps-combat-decision" }
```

It depends on two sibling crates by path inside the screeps-ibex workspace
([`screeps-combat-engine`](https://github.com/Azaril/screeps-combat-engine) for verified damage
formulas and [`screeps-rover`](https://github.com/Azaril/screeps-rover) for the headless
pathfinder), and on `screeps-game-api` for value types (`Part`, `Position`, `RawObjectId`,
`StructureType`). It declares no Cargo features.

## Usage

The library is split into three layers, each a pure function over DTOs.

### The seam types

You build a view from your own world (live `game::*` or a sim) and translate the returned intents
back into actions:

- [`CombatView`] — the read seam from one creep's perspective: `me`, the `squad` state, optional
  per-creep `orders`, and the visible `friends` / `hostiles` / `structures`.
- [`CombatIntent`] — the write seam: `Attack`, `RangedAttack`, `RangedMassAttack`, `Heal`,
  `RangedHeal`, `Dismantle`, `MoveTo`, `Flee`, `Idle`. Each combat variant carries the target
  `Position` plus the target's `id` when it is a creep (`None` for a structure).
- [`TacticalAgent`] — a swappable brain trait (`fn decide(&mut self, view: &CombatView) ->
  Vec<CombatIntent>`). The bot's real logic implements it by calling the pure decisions below;
  scripted opponents implement it too, so self-play runs the same `decide` contract on both sides.

The DTOs ([`CombatCreepDto`], [`CombatStructureDto`], [`SquadStateDto`], [`SquadMemberView`], …)
are plain value types. `CombatCreepDto::working_parts` / `has_working` are the body primitives every
tactic derives from.

### Per-creep combat: `decide_combat`

Given a view, [`decide_combat`] returns one creep's attack + heal intents for the tick. With
`view.orders` set it follows the squad's shared focus and assigned heal target; with `orders ==
None` it runs the body-part-aware fallback (healer-first focus, mass-attack when stacked,
heal-best-nearby). Intents are emitted in the live pipeline order (melee, then ranged, then heal).

```rust
use screeps_combat_decision::{decide_combat, CombatView, CombatIntent};

fn run_creep(view: &CombatView) {
    for intent in decide_combat(view) {
        match intent {
            CombatIntent::Attack { target, id } => { /* creep.attack(...) */ }
            CombatIntent::RangedMassAttack       => { /* creep.ranged_mass_attack() */ }
            CombatIntent::Heal { target, id }    => { /* creep.heal(...) */ }
            _ => {}
        }
    }
}
```

[`decide_movement`] is the matching per-creep movement decision (kite / engage / heal-follow),
returning `MoveTo` / `Flee` / nothing. The executor turns those into a path step via the rover
pathfinder.

### Squad-level decision: `decide_squad` / `decide_squad_with_pathing`

[`select_focus_target`] picks the squad's shared focus as an expected-value choice: among hostiles
it can actually kill (not out-healed, not rampart-shielded), the one whose death removes the most
enemy capability per tick (`threat / ttk`), with structure fallbacks by rank.

[`decide_squad`] is the squad analog one layer up — it picks that focus, resolves engage-vs-retreat
with coupled hysteresis and a Lanchester winnability gate (including a hard veto for enemy safe
mode), and returns a [`SquadDecision`] (state, focus, movement directive, centroid, cohesion radius,
per-member heal + focus-fire assignments, and formation orientation). The per-creep `decide_combat`
/ `decide_movement` then consume that decision.

[`decide_squad_with_pathing`] additionally runs one bounded pathfinder search to produce a scored
kite / engage goal tile (see *How it works*). [`build_room_layers`] precomputes the per-room shared
threat layers so every squad in a room reuses one flood.

```rust
use screeps_combat_decision::{decide_squad, SquadView, SquadOrderState};

fn run_squad(view: &SquadView) {
    let decision = decide_squad(view);
    // decision.focus            -> hand to each member's CreepOrders
    // decision.heal_assignments -> resolve member indices to creeps
    // decision.movement / state -> drive formation movement
    let _ = (decision.state == SquadOrderState::Engaged, decision.focus);
}
```

### Cohesion geometry: `cohesion`

The [`cohesion`] module holds pure squad-cohesion measures — [`cohesion::measure`] returns a
[`cohesion::CohesionSample`] (`max_pairwise`, `centroid_spread`, `in_formation_rate`) and
[`cohesion::centroid`] computes the shared coordinate frame. This is the validation instrument for
the movement workstream and the basis for the colony-health military score, shared so the sim and
the live bot measure the same way.

### Kite / position pricing: `kite`

The [`kite`] module is the per-tile *pricing* the pathfinder consumes (it is not a search — the
search is `screeps-rover`'s `LocalPathfinder`). [`kite::score_tile`] returns a cost (lower is
better) blending normalized safety / future-threat / cohesion / proximity / openness / edge / focus-
damage terms; *flee* and *stand* emerge from one scorer via two weight presets
([`kite::KiteScoreParams::default`] vs [`kite::KiteScoreParams::engage`]).
[`kite::plan_kite_anchor`] runs one bounded scored search to pick the squad's goal tile, and
[`kite::PositionLayers`] caches the expensive per-room floods so every consumer reuses them.

## How it works

The crate is **parity-first**: the live shim must emit byte-identical combat intents to the prior
inline bot logic. The decisions are pure logic over `screeps-game-api` value types — the same
profile as the combat engine — so the sim harness can depend on this small crate instead of the
whole bot, and the boundary guarantees no tactics drift between live play and self-play.

Positioning is a single unified utility. Rather than separate "approach", "kite", and "engage"
movement branches, the squad runs one bounded `search_scored` from its centroid and prices each
reached tile with `kite::score_tile`. Every term is normalized into a common band, so an objective
preset is just a different weight vector: the kite preset lets safety dominate (flee), the engage
preset lets the focus-damage reward dominate (stand and fight and advance into weapon range). The
expensive shared computation — the threat field and the threat-chaser reachability flood — is built
once per room via `PositionLayers` / `build_room_layers` and reweighted per use.

Damage math is delegated, not re-transcribed: tower falloff, heal power, and related formulas come
from `screeps-combat-engine`, the single source of truth, so the decision layer can never disagree
with the engine the sim runs.

## Related crates

- [screeps-combat-engine](https://github.com/Azaril/screeps-combat-engine) — pure leaf crate with
  the verified combat/damage formulas and the in-process micro-sim this layer decides over.
- [screeps-combat-agent](https://github.com/Azaril/screeps-combat-agent) — the `IbexAgent`
  `TacticalAgent` implementation that calls these pure decisions; runs in self-play as `IbexAgent`
  vs `IbexAgent`.
- [screeps-combat-eval](https://github.com/Azaril/screeps-combat-eval) — the self-play tournament /
  evaluation harness built on top of the agent and this decision layer.
