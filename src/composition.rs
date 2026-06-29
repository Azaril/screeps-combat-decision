//! Squad composition + role model — the data-driven "what a squad should look like when fully
//! spawned" (slots × body types × formation), the body-type selector that maps each slot to a
//! concrete force-`Sized` body, and the deterministic assembler ([`assemble_force`]) that turns a
//! [`crate::force_sizing`] requirement vector directly into a fielded composition (no template, no catalog).
//!
//! Lives in the decision crate (not the bot) so the sim/eval field the bot's REAL composition with
//! one implementation (no replica) — pure over `screeps-game-api` value types + the engine's
//! mechanics, no `game::*`. `SquadComposition::estimated_combat_time`/`is_viable_from` take a
//! precomputed `travel_ticks` (the bot owns the `PathfinderService`; see `war::best_force_budget`).

use crate::bodies;
use crate::doctrine::{emit_requirement, DoctrineObjective, EnemyCoordination, EnemyForce};
use crate::force_sizing::{tower_dps_at_assault, win_probability, DefenseProfile, ForceBudget, RequiredForce, COORDINATED_DPS_MARGIN, HOLD_MARGIN};
use screeps::{Part, ResourceType};
use screeps_combat_engine::constants::{ATTACK_POWER, CREEP_LIFE_TIME, DISMANTLE_POWER, HEAL_POWER, RANGED_ATTACK_POWER};
use serde::{Deserialize, Serialize};

/// Ticks per body part to spawn (`CREEP_SPAWN_TIME`) — an engine constant not modeled in
/// `screeps_combat_engine::constants`; kept here for the spawn-time estimate.
const CREEP_SPAWN_TIME: u32 = 3;

/// Most members a single force-sized squad may grow to (D3 member-count scaling). Beyond this the
/// target needs the multi-squad **G4-HEAVY** path (P5), so [`assemble_force`] defers
/// rather than field an unmanageable blob. 8 members — enough to out-heal an L1-2 stronghold /
/// multi-keeper SK at RCL7+, bounded for formation + CPU sanity.
const MAX_SIZED_MEMBERS: usize = 8;

/// Most parts of ONE role-type a single sized member can carry: a pure single-part body on plains
/// (1:1 MOVE) is `2n` parts, so the 50-part engine cap bounds `n` at 25. The upper bound of the
/// per-member capacity search in [`single_role_cap`] (used by [`assemble_force`] / [`force_ceiling`]).
const MAX_SINGLE_ROLE_PARTS: u32 = 25;

/// Preferred per-member energy ceiling for force-sized members — kept BELOW the 50-part / 25-role-part
/// hard max so a sized member is reliably bankable at HIGH spawn priority while CRITICAL economy creeps
/// drain the home. This splits a force across MORE, SMALLER members instead of one un-spawnable ~5000e
/// blob that re-queues forever and never departs (the live W7N7 25-RANGED / W7N4 16-HEAL bug: a 5000e
/// member is ~90% of an RCL7 spawn's capacity and never accumulates while miners drain it). At ~3000 a
/// member is ~half an RCL7 capacity — easily banked — yet counts stay within [`MAX_SIZED_MEMBERS`] for
/// normal targets. The 50-part engine cap and [`MAX_SINGLE_ROLE_PARTS`] remain the hard CEILING; this
/// only ever LOWERS the capacity probe. (~3000 ⇒ 15 RANGED+15 MOVE = 3000e, or 10 HEAL+10 MOVE = 3000e.)
/// Also used by the spawn path (`queue_slot_spawn`) to cap member bodies, so every spawned member stays
/// bankable.
pub const PREFERRED_MEMBER_ENERGY: u32 = 3000;

/// Role a creep plays within a squad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SquadRole {
    /// Front-line damage sponge with TOUGH + ATTACK.
    Tank,
    /// Dedicated healer with HEAL parts.
    Healer,
    /// Ranged damage dealer with RANGED_ATTACK.
    #[default]
    RangedDPS,
    /// Melee damage dealer with ATTACK.
    MeleeDPS,
    /// Structure destroyer with WORK (dismantle).
    Dismantler,
    /// Resource hauler with CARRY.
    Hauler,
    /// Controller de-claimer with CLAIM — ADR 0027 v1.1 P2: `attackController`s a derelict controller to
    /// neutral (the `DeclaimAttack` doctrine). Undefended by construction, so it carries CLAIM + MOVE only.
    Declaimer,
}

/// Enum of body definition selectors. With the static catalog deleted (ADR 0031 Phase 4b), the only
/// remaining variant is the force-`Sized` body — explicit part counts from the force-sizing solver,
/// built via `bodies::build_combat_body`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyType {
    /// A force-SIZED body (R3, ADR 0020 §12.6): explicit part counts from the force-sizing solver,
    /// built via `bodies::build_combat_body`.
    Sized(bodies::CombatBodySpec),
}

impl BodyType {
    /// Build the spawn body for this body type at `max_energy` over `move_profile`: a `Sized` spec via
    /// the dynamic builder (R1). `None` ⇒ can't build / can't afford. The single body-producing entry
    /// point for the spawn path.
    pub fn build_body(&self, max_energy: u32, move_profile: bodies::MoveProfile) -> Option<Vec<Part>> {
        match self {
            BodyType::Sized(spec) => bodies::build_combat_body(spec, move_profile, max_energy),
        }
    }

    /// Estimate the body cost at a given energy capacity.
    pub fn estimated_cost(&self, _max_energy: u32) -> u32 {
        let BodyType::Sized(spec) = self;
        let moves = bodies::MoveProfile::Plains.move_parts(spec.non_move_parts());
        spec.tough * Part::Tough.cost()
            + spec.attack * Part::Attack.cost()
            + spec.ranged_attack * Part::RangedAttack.cost()
            + spec.work * Part::Work.cost()
            + spec.carry * Part::Carry.cost()
            + spec.heal * Part::Heal.cost()
            + moves * Part::Move.cost()
    }

    /// Estimate the number of body parts at a given energy capacity.
    pub fn estimated_part_count(&self, _max_energy: u32) -> u32 {
        let BodyType::Sized(spec) = self;
        spec.non_move_parts() + bodies::MoveProfile::Plains.move_parts(spec.non_move_parts())
    }

    /// Count of `part` in the expanded body at `max_energy` — the per-part-type input the force-sizing
    /// oracle needs (ADR 0020 §12.2).
    pub fn part_count(&self, _max_energy: u32, part: Part) -> u32 {
        let BodyType::Sized(spec) = self;
        match part {
            Part::Tough => spec.tough,
            Part::Attack => spec.attack,
            Part::RangedAttack => spec.ranged_attack,
            Part::Work => spec.work,
            Part::Carry => spec.carry,
            Part::Heal => spec.heal,
            Part::Move => bodies::MoveProfile::Plains.move_parts(spec.non_move_parts()),
            _ => 0,
        }
    }

    /// List the boost compounds required for this body type (if boosted). Force-`Sized` bodies are
    /// unboosted (v1), so this is always empty.
    pub fn required_boosts(&self) -> Vec<(ResourceType, u32)> {
        Vec::new()
    }
}

/// A single slot in a squad composition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SquadSlot {
    /// What role this slot fills.
    pub role: SquadRole,
    /// Which body definition to use for spawning.
    pub body_type: BodyType,
}

/// Base formation shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormationShape {
    #[default]
    None,
    Line,
    Box2x2,
    Triangle,
    WideLine,
}

/// Formation movement mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormationMode {
    /// Virtual position only advances when all living members are in formation.
    #[default]
    Strict,
    /// Virtual position advances based on member centroid.
    Loose,
}

/// Defines what a squad should look like when fully spawned.
/// Data-driven replacement for the Solo/Duo/Quad enums.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SquadComposition {
    /// Human-readable label for logging/visualization.
    pub label: String,
    /// The slots that need to be filled.
    pub slots: Vec<SquadSlot>,
    /// Base formation shape for this composition.
    pub formation_shape: FormationShape,
    /// Default formation mode.
    pub formation_mode: FormationMode,
    /// HP fraction below which the squad should retreat (0.0 - 1.0).
    /// Defaults to 0.3 for most compositions; higher for bursty combat (e.g. SK).
    #[serde(default = "default_retreat_threshold")]
    pub retreat_threshold: f32,
}

fn default_retreat_threshold() -> f32 {
    0.3
}

impl SquadComposition {
    // ─── Cost and timing estimation ─────────────────────────────────────

    /// Estimate the total energy cost to spawn this composition
    /// at a given energy capacity.
    pub fn estimated_cost(&self, energy_capacity: u32) -> u32 {
        self.slots.iter().map(|slot| slot.body_type.estimated_cost(energy_capacity)).sum()
    }

    /// Estimate total spawn time for this composition (ticks to spawn all members).
    /// Each body part takes CREEP_SPAWN_TIME (3) ticks. With N spawns available,
    /// members can be spawned in parallel.
    pub fn estimated_spawn_time(&self, energy_capacity: u32, available_spawns: u32) -> u32 {
        if available_spawns == 0 || self.slots.is_empty() {
            return u32::MAX;
        }

        let mut part_counts: Vec<u32> = self
            .slots
            .iter()
            .map(|slot| slot.body_type.estimated_part_count(energy_capacity))
            .collect();

        // Sort descending so longest spawns go first.
        part_counts.sort_unstable_by(|a, b| b.cmp(a));

        // Simulate parallel spawning across available_spawns.
        let mut spawn_lanes = vec![0u32; available_spawns as usize];
        for parts in &part_counts {
            // Assign to the lane that finishes earliest.
            let min_lane = spawn_lanes.iter_mut().min().unwrap();
            *min_lane += parts * CREEP_SPAWN_TIME;
        }

        spawn_lanes.into_iter().max().unwrap_or(0)
    }

    /// Estimate useful combat time for this composition given a precomputed `travel_ticks` from a home
    /// room to the target. Accounts for spawn time, travel time, and CREEP_LIFE_TIME. The caller owns
    /// the route lookup (the bot's `PathfinderService`); this stays a pure scalar calc so the sim/eval
    /// can drive it with a synthetic budget.
    pub fn estimated_combat_time(&self, travel_ticks: u32, energy_capacity: u32, available_spawns: u32) -> u32 {
        let spawn_time = self.estimated_spawn_time(energy_capacity, available_spawns);
        CREEP_LIFE_TIME.saturating_sub(spawn_time + travel_ticks)
    }

    /// Check if launching from a home `travel_ticks` away gives enough combat time to be worthwhile:
    /// false if creeps would arrive with <40% lifetime remaining.
    pub fn is_viable_from(&self, travel_ticks: u32, energy_capacity: u32, available_spawns: u32) -> bool {
        let combat_time = self.estimated_combat_time(travel_ticks, energy_capacity, available_spawns);
        combat_time as f32 > CREEP_LIFE_TIME as f32 * 0.4
    }

    /// List all boost compounds required for this composition.
    pub fn required_boosts(&self) -> Vec<(ResourceType, u32)> {
        let mut boosts: Vec<(ResourceType, u32)> = Vec::new();
        for slot in &self.slots {
            for (compound, amount) in slot.body_type.required_boosts() {
                if let Some(existing) = boosts.iter_mut().find(|(c, _)| *c == compound) {
                    existing.1 += amount;
                } else {
                    boosts.push((compound, amount));
                }
            }
        }
        boosts
    }

    /// Number of creeps in this composition.
    pub fn member_count(&self) -> usize {
        self.slots.len()
    }

    /// This composition's combat capabilities at a given spawn energy — the [`crate::force_sizing`]
    /// oracle's `ForceBudget` inputs (ADR 0020 §12.2). Bodies auto-size to `max_energy` (the same
    /// sizing the spawner uses), so the assessment reflects what we'd actually field at this RCL.
    /// Unboosted (v1).
    pub fn capabilities(&self, max_energy: u32) -> SquadCapabilities {
        let mut heal_per_tick = 0u32;
        let mut structure_dps = 0u32;
        let mut tank_effective_hp = 0u32;
        for slot in &self.slots {
            let bt = slot.body_type;
            heal_per_tick += bt.part_count(max_energy, Part::Heal) * HEAL_POWER;
            // Structure damage: WORK dismantles (50/part), ATTACK (30/part), RANGED_ATTACK (10/part). All
            // breach ramparts + kill the core. A force-`Sized` body reports its ACTUAL ranged parts (an
            // assembled / `force_ceiling` force is already sized to its per-member cap), so the budget
            // reflects exactly what we'd field.
            let ranged_parts = bt.part_count(max_energy, Part::RangedAttack);
            structure_dps += bt.part_count(max_energy, Part::Work) * DISMANTLE_POWER
                + bt.part_count(max_energy, Part::Attack) * ATTACK_POWER
                + ranged_parts * RANGED_ATTACK_POWER;
            // The tank is the toughest single member (most total HP = parts × 100, unboosted).
            tank_effective_hp = tank_effective_hp.max(bt.estimated_part_count(max_energy) * 100);
        }
        SquadCapabilities { heal_per_tick, structure_dps, tank_effective_hp }
    }

    /// Build the force-sizing [`crate::force_sizing::ForceBudget`] for this composition at `member_energy`
    /// with `onsite_budget_ticks` of on-site time — the CEILING capabilities the oracle assesses against.
    /// Shared by the bot (`best_force_budget`, which picks the best home + supplies the onsite ticks) and
    /// the eval (from the scenario) so both build the budget identically (ADR 0026 §9 parity).
    pub fn force_budget(&self, member_energy: u32, onsite_budget_ticks: u32) -> crate::force_sizing::ForceBudget {
        let caps = self.capabilities(member_energy);
        crate::force_sizing::ForceBudget {
            max_heal_per_tick: caps.heal_per_tick as f32,
            max_dismantle_dps: caps.structure_dps as f32,
            tank_effective_hp: caps.tank_effective_hp as f32,
            onsite_budget_ticks,
        }
    }
}

/// A single-role part SPEC — the body of a member that carries only `n` of its role's weapon part (the
/// roles a [`RequiredForce`] covers: HEAL/WORK/RANGED/TOUGH; ATTACK/CARRY for exhaustiveness). Used by
/// [`assemble_force`] / [`force_ceiling`]. MOVE is added per-member by [`bodies::build_combat_body`].
fn single_role_spec(role: SquadRole, n: u32) -> bodies::CombatBodySpec {
    match role {
        SquadRole::Healer => bodies::CombatBodySpec { heal: n, ..Default::default() },
        SquadRole::Dismantler => bodies::CombatBodySpec { work: n, ..Default::default() },
        SquadRole::RangedDPS => bodies::CombatBodySpec { ranged_attack: n, ..Default::default() },
        SquadRole::MeleeDPS => bodies::CombatBodySpec { attack: n, ..Default::default() },
        SquadRole::Tank => bodies::CombatBodySpec { tough: n, ..Default::default() },
        SquadRole::Hauler => bodies::CombatBodySpec { carry: n, ..Default::default() },
        SquadRole::Declaimer => bodies::CombatBodySpec { claim: n, ..Default::default() },
    }
}

/// Largest single-role part count one member can carry at `probe_energy` — reverse-probed via the REAL
/// builder (incl. the per-member MOVE ratio + the 50-part cap) so the cap can never drift from what
/// actually spawns. 0 ⇒ can't field even one member of this role at this energy.
fn single_role_cap(role: SquadRole, probe_energy: u32) -> u32 {
    (1..=MAX_SINGLE_ROLE_PARTS)
        .rev()
        .find(|&n| bodies::build_combat_body(&single_role_spec(role, n), bodies::MoveProfile::Plains, probe_energy).is_some())
        .unwrap_or(0)
}

/// The COMPACT W×H footprint that holds `n` members (ADR 0031 D14) — the single source of truth for the
/// formation footprint, derived purely from the member count so every consumer (bot `box_formation`, agent
/// `footprint()`, rover `moving_maximum` input) sizes the same box. `width = ⌈√n⌉`, `height = ⌈n / width⌉`:
/// N=1→1×1, 2→2×1, 3-4→2×2, 5-6→3×2, 7-8→3×3 — generalizing the old hardcoded 2×2. Member offsets are the
/// first `n` cells of this box, row-major from the anchor (slot 0 = `(0,0)`), so members never overlap and
/// the footprint is exactly the bounding box. `n == 0` ⇒ `(1, 1)` (degenerate, no members).
pub fn box_footprint(n: usize) -> (u8, u8) {
    let n = n.max(1);
    let width = (1..).find(|w| w * w >= n).unwrap_or(1);
    let height = n.div_ceil(width);
    (width as u8, height as u8)
}

/// Formation for an assembled force of `count` members — the shape is DERIVED from the member count (ADR
/// 0031 D14): a lone member roams loose, a 2-member force holds a strict line ([`FormationShape::Line`]),
/// ≥3 hold a strict COMPACT box ([`FormationShape::Box2x2`], which now means "the ⌈√N⌉ × ⌈N/⌈√N⌉⌉ box from
/// [`box_footprint`]", NOT literally 2×2 — N=4→2×2, 5-6→3×2, 7-8→3×3). Total + explicit over
/// `0..=MAX_SIZED_MEMBERS`; the catch-all keeps the box for any larger count the caller might pass.
/// The downstream footprint is `box_footprint(count)` for the box arm (the geometry lives in ONE place).
pub fn formation_for(count: usize) -> (FormationShape, FormationMode) {
    match count {
        0 | 1 => (FormationShape::None, FormationMode::Loose),
        2 => (FormationShape::Line, FormationMode::Strict),
        _ => (FormationShape::Box2x2, FormationMode::Strict),
    }
}

/// A compact role tally for logging / viz, in slot order (slots are grouped by role) — e.g.
/// "Assembled 1×Dismantler 1×RangedDPS 2×Healer".
fn assembled_label(slots: &[SquadSlot]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < slots.len() {
        let role = slots[i].role;
        let n = slots[i..].iter().take_while(|s| s.role == role).count();
        parts.push(format!("{n}×{role:?}"));
        i += n;
    }
    format!("Assembled {}", parts.join(" "))
}

/// THE ASSEMBLER (ADR 0031 T2) — turn a capability vector ([`RequiredForce`]) DIRECTLY into a fielded
/// composition, with NO template and NO body catalog: each weapon role's member COUNT emerges continuously
/// from its demand, and each member's body is force-`Sized` per pick via the real builder. THE one
/// composition builder (no per-doctrine template, no body catalog — D14/D15).
///
/// The min-viable floor is a ROLE-SET (≥1 member per DEMANDED role), NEVER a template count — so the
/// Layer-B "can't add a role the template lacks" gap and the solo↔quad granularity snap are STRUCTURALLY
/// impossible (1..=[`MAX_SIZED_MEMBERS`] are all reachable, sized to exactly meet the requirement — winning
/// but efficient, no over-spend, D13). RANGED carries BOTH the immune-structure DPS AND the anti-creep
/// kill (the same physical part, additive demand — a siege facing a guard needs enough RANGED for BOTH).
///
/// This is the marginal-capability-per-energy fill specialized to the current 1:1 role↔dimension map: each
/// `RequiredForce` dimension is supplied by exactly one role, so the fill degenerates to "grow each role to
/// meet its demand" — there is no scarcest-dimension contention to arbitrate. (A future dimension a second
/// role could supply — e.g. structure DPS via WORK *or* RANGED — would generalize this to the full
/// scarcest-dimension auction; the frozen demand order below is that auction's tie-break.)
///
/// Returns `None` — a TERMINAL defer (D10: no G4-HEAVY failover; the higher-power response is a
/// strategy-layer call) — when a demanded role can't field even one member at this energy, the requirement
/// is empty, or the force would exceed [`MAX_SIZED_MEMBERS`]. Bit-deterministic: integer/ceil over a frozen
/// Vec-ordered demand list, no HashMap.
pub fn assemble_force(req: &RequiredForce, member_energy: u32) -> Option<SquadComposition> {
    // Probe per-member caps at the SMALLER of the home capacity and the preferred ceiling, so a force is
    // split into more, smaller, bankable members rather than one un-spawnable ~5000e blob (the W7N7 bug).
    let probe_energy = member_energy.min(PREFERRED_MEMBER_ENERGY);

    // The capability vector → weapon-role demands, in the ADR's frozen dimension order (= the slot order +
    // the determinism tie-break). RANGED = immune_struct + anti_creep (anti-structure AND anti-creep).
    // NOTE: this is the COMPOSITION order (the engaged formation expects the healer-anchored layout); the
    // FIGHTER-FIRST SPAWN ordering (deep-reach fix Break #1) is applied at the bot's Phase-B spawn loop
    // (`spawn_order_fighter_first`), NOT here — so the engaged force's positioning/formation is unchanged.
    let demands: [(SquadRole, u32); 5] = [
        (SquadRole::Healer, req.heal_parts),
        (SquadRole::Dismantler, req.dismantle_parts),
        (SquadRole::RangedDPS, req.immune_struct_parts + req.anti_creep_parts),
        (SquadRole::Tank, req.tough_parts),
        // ADR 0027 v1.1 P2: a DECLAIM objective fields CLAIM declaimers (set only by the `DeclaimAttack`
        // doctrine; 0 on every combat objective, so the demand list is inert for all existing objectives).
        (SquadRole::Declaimer, req.claim_parts),
    ];

    let mut slots: Vec<SquadSlot> = Vec::new();
    for (role, total) in demands {
        if total == 0 {
            continue; // no demand for this weapon
        }
        let cap = single_role_cap(role, probe_energy);
        if cap == 0 {
            return None; // can't field even one member of this role at this energy → defer
        }
        // Continuous member count: the role-set floor is ONE (never under-sized), grown by ceil so each
        // member's even share fits the cap. No template-count floor — Layer B cannot recur. `per_member`
        // is ceil so Σ over members ≥ total (the force never under-sizes); `per_member ≤ cap` always holds.
        let count = total.div_ceil(cap).max(1);
        let per_member = total.div_ceil(count);
        let spec = single_role_spec(role, per_member);
        for _ in 0..count {
            slots.push(SquadSlot { role, body_type: BodyType::Sized(spec) });
        }
    }

    if slots.is_empty() {
        return None; // an empty requirement fields nothing — the caller defers / no-ops
    }
    if slots.len() > MAX_SIZED_MEMBERS {
        // A bigger force is the STRATEGY layer's call (scale the blob / multi-squad / boost — a future
        // ADR), NOT a composition-layer failover (D10). The assembler terminates at the best single squad.
        return None;
    }

    let (formation_shape, formation_mode) = formation_for(slots.len());
    Some(SquadComposition {
        label: assembled_label(&slots),
        slots,
        formation_shape,
        formation_mode,
        // The objective-class retreat tuning (e.g. SK's bursty 0.5) is layered by the caller post-assembly;
        // the assembler is objective-agnostic (it sees only the vector), so it uses the standard threshold.
        retreat_threshold: default_retreat_threshold(),
    })
}

// ═══ ADR 0031 D16/D17 — the EV-MAXIMIZING composition optimizer ═══════════════════════════════════════
//
// `optimize_composition` SUPERSEDES `force_ceiling` (D16): it presumes NO reference squad. It enumerates
// candidate compositions over a bounded, bit-deterministic search (over-power ladder × TOUGH ladder), scores
// `EV(C) = P(win | C) · target_value − cost(C)` for each candidate computed from THAT candidate's OWN
// `capabilities()` vs the (dynamic-margin-inflated) threat, and commits the max-EV comp (gated doctrines
// only when `EV > commit_ev_threshold`). `emit_requirement` (T1) survives as the per-objective requirement
// + weapon-mix; `assemble_force` (D3) survives as the per-candidate comp BUILDER; `win_probability`
// survives as the probability model.

/// Minimal DEFAULT capability floor an always-field doctrine keeps on the requirement (mirrors
/// `doctrine::DEFAULT_FLOOR_PARTS`; D11) — never field below a survivable scout force.
const DEFAULT_FLOOR_PARTS: u32 = 4;

/// The over-power ladder (D17): scale `emit_requirement`'s required vector by `k` per candidate. `1.0` is
/// the minimal winning force; higher rungs over-invest (more, bigger members) so a high-value target can
/// trade cost for a higher P(win). Vec-ordered (the determinism tie-break prefers the lowest `k`).
const OVER_POWER_LADDER: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

/// The TOUGH ladder (D17): TOUGH parts = `ceil(t · fighter_parts)` added to the requirement as EHP front
/// armor (was hardwired 0). Vec-ordered (the tie-break prefers the lowest `t`).
const TOUGH_LADDER: [f32; 3] = [0.0, 0.1, 0.2];

/// The tournament-tunable knobs for [`optimize_composition`] (ADR 0031 D16/D17 / 0031a §2). NOT
/// `Serialize` — a transient per-tick search input, never persisted, so it costs no `WORLD_FORMAT_VERSION`
/// bump. [`CompositionParams::default`] reproduces the current fielding seeds (`HOLD_MARGIN` 1.3,
/// `COORDINATED_DPS_MARGIN` 1.5, `PREFERRED_MEMBER_ENERGY` 3000, no dynamic inflation, EV floor 0), so
/// swapping `force_ceiling` for the optimizer at Default is behavior-preserving for the calibration gates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositionParams {
    /// Cost weight per ENERGY (spawn cost) in `cost(C)`. Small so a borderline-EV target still fields when
    /// it is actually winnable (the OracleCalibration FP/FN semantics are preserved at a sensible
    /// `target_value`); larger values down-weight a marginal siege under spawn contention.
    pub w_energy: f32,
    /// Cost weight per CREEP (member) in `cost(C)` — a tie-break nudge toward fewer, fatter members.
    pub w_creep: f32,
    /// Heal-surplus safety factor threaded into [`emit_requirement`] (the seed [`HOLD_MARGIN`]).
    pub hold_margin: f32,
    /// Coordinated square-law over-match threaded into [`emit_requirement`] (the seed
    /// [`COORDINATED_DPS_MARGIN`]).
    pub over_power_margin: f32,
    /// Inflate the OBSERVED hostile force (incoming dps, enemy hits) so a GROWING threat still loses. 1.0 =
    /// trust the snapshot (the seed); >1.0 sizes against a rising threat.
    pub dynamic_margin: f32,
    /// Per-member energy cap (the small-many-vs-few-big lever; the seed [`PREFERRED_MEMBER_ENERGY`]). The
    /// optimizer probes member caps at `min(member_energy, this)`.
    pub member_energy: u32,
    /// EV floor a GATED doctrine must clear to field at all (else `None` defer). Seed 0 (field any positive
    /// EV); >0 means "only commit with real EV headroom."
    pub commit_ev_threshold: f32,
}

impl Default for CompositionParams {
    fn default() -> Self {
        CompositionParams {
            // Small energy weight so a winnable target at a sensible `target_value` always clears
            // `EV > 0` — i.e. "EV > commit" ⇔ "winnable" (preserving the OracleCalibration FP/FN
            // semantics). w_creep 0 (pure tie-break role, off by default).
            w_energy: 0.001,
            w_creep: 0.0,
            hold_margin: HOLD_MARGIN,
            over_power_margin: COORDINATED_DPS_MARGIN,
            dynamic_margin: 1.0,
            member_energy: PREFERRED_MEMBER_ENERGY,
            commit_ev_threshold: 0.0,
        }
    }
}

/// THE EV OPTIMIZER (ADR 0031 D16/D17) — the EV-maximizing composition selector that SUPERSEDES
/// `force_ceiling`'s presumed 3+5 budget: it presumes NO reference squad. It runs ONE bounded,
/// bit-deterministic search — over-power ladder × TOUGH ladder — building each candidate via
/// [`assemble_force`] from [`emit_requirement`]'s (per-objective weapon-mixed) requirement scaled by the
/// rung, scores `EV(C) = P(win | C) · target_value − cost(C)` from the candidate's OWN
/// [`SquadComposition::capabilities`] vs the (dynamic-margin-inflated) threat, and returns the max-EV
/// candidate (deterministic tie-break: lowest k, then lowest tough, then fewest members).
///
/// `honor_verdict == true` (a GATED doctrine) → commit the max-EV comp iff `EV > commit_ev_threshold`, else
/// `None` (the honest unwinnable defer, D10). `honor_verdict == false` (always-field) → commit the max-EV
/// comp regardless (the caller already raised the requirement to the default floor).
///
/// Bit-deterministic: integer/ceil folds over the two Vec-ordered ladders, no HashMap.
#[allow(clippy::too_many_arguments)]
pub fn optimize_composition(
    objective: DoctrineObjective,
    defense: &DefenseProfile,
    enemy: Option<EnemyForce>,
    target_value: f32,
    onsite_window: u32,
    coordination: EnemyCoordination,
    importance: f32,
    honor_verdict: bool,
    confirmed_undefended: bool,
    params: &CompositionParams,
) -> Option<SquadComposition> {
    let member_energy = params.member_energy;
    // The winnability BUDGET emit_requirement assesses against = the per-member-capped CEILING force this
    // optimizer can actually field within MAX_SIZED_MEMBERS (the same role caps assemble_force uses), so the
    // structure/clear arms' `winnable` verdict stays conservative — but the COMMIT decision is the EV, not
    // the verdict (D16). A pure RANGED ceiling covers immune-core / creep-clear; the structure arm's WORK
    // budget is derived inside emit_requirement via the same parts.
    let budget = optimizer_ceiling_budget(objective, member_energy, onsite_window);

    // The per-objective requirement + weapon mix (T1). The optimizer scales THIS per rung; emit_requirement
    // already mixes WORK/RANGED/anti-creep/heal correctly for the objective + defense.
    let (_assessment, mut base_required) =
        emit_requirement(objective, defense, enemy, Some(&budget), coordination, importance, params.hold_margin, params.over_power_margin);
    // ALWAYS-FIELD doctrines (defense / operator intent / deny) keep the minimal default floor on the
    // requirement (D11) — never field below a survivable scout force, even for a tiny scouted threat.
    // EXCEPTION (ADR 0031 §2(d)): a CONFIRMED-undefended target (reliable intel, zero towers + zero
    // defenders — `confirmed_undefended`, the ONE predicate computed by the caller's
    // `EngagementContext::defense_confirmed_undefended`) suppresses the floor so the oracle's correct
    // `heal_parts = 0` survives (no wasted Healer slots). The floor is RETAINED for an UNSCOUTED room
    // (`!confirmed_undefended`, the hedge against a naked force into the unknown) and any defended target.
    if !honor_verdict && !confirmed_undefended {
        base_required.heal_parts = base_required.heal_parts.max(DEFAULT_FLOOR_PARTS);
        base_required.anti_creep_parts = base_required.anti_creep_parts.max(DEFAULT_FLOOR_PARTS);
    }

    // The (dynamic-margin-inflated) threat the candidate must survive + kill.
    let enemy = enemy.unwrap_or_default();
    let tower_dps = tower_dps_at_assault(&defense.towers);
    let incoming = (tower_dps + enemy.dps) * params.dynamic_margin;
    let required_kill =
        defense.objective_hits as f32 + defense.breach_hits as f32 + enemy.hits as f32 * params.dynamic_margin;

    // FIX 3 — UNDEFENDED, zero-attrition structure target (e.g. a level-0 invader core: towers=[], enemy
    // dps=0, no defenders). With NOTHING shooting back there is NO attrition risk: we ALWAYS win, for ANY
    // force that razes the structure before its real available deadline (the long `onsite_window`, which the
    // caller derives from the core's deploy/expiry window). The logistic `win_probability(deliverable,
    // required_kill)` below conflates kill-SPEED with win-PROBABILITY — it stays < 1.0 at the minimal force
    // even though the win is certain, so the optimizer climbs the over-power ladder and over-sizes a trivial
    // core to 4-5 RangedDPS. For a no-attrition target `p_kill` is BINARY: 1.0 iff the candidate clears
    // within the window (`deliverable >= required_kill`), else 0.0 (can't kill in time — not winnable in the
    // window). That removes the over-power climb (every in-time force is equally certain to win), so the
    // EV-max collapses to the MINIMAL clearing force via the cost term + the lowest-k / fewest-members
    // tie-break — the operator's "fewest creeps / most efficient" goal.
    //
    // SCOPE: strictly `tower_dps == 0` (no energized towers) AND `incoming == 0` (no defender DPS / no enemy
    // force). A DEFENDED target (energized towers OR enemy dps > 0) has real attrition, keeps `incoming > 0`,
    // and falls to the existing logistic `p_kill` + over-power headroom UNCHANGED — so the DEFENDED
    // calibration gates (OracleCalibration / SizingWins / CreepClearWins / the defended-core acceptance
    // tests) are untouched.
    let undefended = tower_dps == 0.0 && incoming == 0.0;

    let mut best: Option<(f32, f32, f32, usize, SquadComposition)> = None; // (ev, k, t, members, comp)
    for &k in OVER_POWER_LADDER.iter() {
        let scaled = base_required.scaled(k);
        // Fighter parts the TOUGH fraction is taken against (the weapons the body fields).
        let fighter_parts = scaled.dismantle_parts + scaled.immune_struct_parts + scaled.anti_creep_parts;
        for &t in TOUGH_LADDER.iter() {
            let mut req = scaled;
            if t > 0.0 && fighter_parts > 0 {
                req.tough_parts = (t * fighter_parts as f32).ceil() as u32;
            }
            let Some(comp) = assemble_force(&req, member_energy) else {
                continue; // unfieldable / > MAX_SIZED_MEMBERS at this rung
            };
            let caps = comp.capabilities(member_energy);
            let p_survive = win_probability(caps.heal_per_tick as f32, incoming);
            let deliverable = caps.structure_dps as f32 * onsite_window as f32;
            // No-attrition target: certain win for any force that clears within the window; otherwise the
            // logistic kill-speed → win-probability curve (DEFENDED, real attrition — unchanged).
            let p_kill = if undefended {
                if required_kill <= 0.0 || (deliverable >= required_kill && caps.structure_dps > 0) {
                    1.0
                } else {
                    0.0
                }
            } else {
                win_probability(deliverable, required_kill)
            };
            let p_win = p_survive * p_kill;
            let cost = params.w_energy * comp.estimated_cost(member_energy) as f32 + params.w_creep * comp.member_count() as f32;
            let ev = p_win * target_value - cost;
            let members = comp.member_count();
            // Deterministic tie-break: max EV, then lowest k, then lowest tough, then fewest members.
            let better = match &best {
                None => true,
                Some((bev, bk, bt, bm, _)) => {
                    ev > *bev + 1e-6
                        || ((ev - *bev).abs() <= 1e-6
                            && (k < *bk - 1e-6
                                || ((k - *bk).abs() <= 1e-6 && (t < *bt - 1e-6 || ((t - *bt).abs() <= 1e-6 && members < *bm)))))
                }
            };
            if better {
                best = Some((ev, k, t, members, comp));
            }
        }
    }

    let (ev, _, _, _, comp) = best?;
    // GATED doctrine: only commit with EV above the floor (the honest unwinnable defer, D10). ALWAYS-FIELD:
    // field the best regardless.
    if honor_verdict && ev <= params.commit_ev_threshold {
        return None;
    }
    Some(comp)
}

/// The winnability BUDGET the optimizer assesses emit_requirement against — the per-member-capped CEILING
/// force this optimizer can actually field (replacing `force_ceiling(member_energy, fighter).force_budget`).
/// IDENTICAL math to the deleted `force_ceiling`: 3 fighters + 5 healers, each at its `single_role_cap`,
/// with the kill weapon selected by the OBJECTIVE (the doctrine's old `fighter_role`): WORK for a
/// dismantle-able ring, RANGED for an immune core / creep clear / keeper. The `winnable` verdict stays
/// conservative (the EV commit, not the verdict, is the gate — D16). Bit-deterministic.
pub fn optimizer_ceiling_budget(objective: DoctrineObjective, member_energy: u32, onsite_window: u32) -> ForceBudget {
    let probe = member_energy.min(PREFERRED_MEMBER_ENERGY);
    const CEILING_FIGHTERS: u32 = 3;
    const CEILING_HEALERS: u32 = 5;
    // The kill weapon = the doctrine's former `fighter_role`: WORK razes a dismantle-able ring; everything
    // else (immune core / creep clear / keeper) kills with RANGED.
    let fighter = match objective {
        DoctrineObjective::DismantleStructure => SquadRole::Dismantler,
        _ => SquadRole::RangedDPS,
    };
    let fighter_cap = single_role_cap(fighter, probe);
    let heal_cap = single_role_cap(SquadRole::Healer, probe);
    let fighter_power = match fighter {
        SquadRole::Dismantler => DISMANTLE_POWER,
        _ => RANGED_ATTACK_POWER,
    };
    let max_heal_per_tick = (CEILING_HEALERS * heal_cap * HEAL_POWER) as f32;
    let max_dismantle_dps = (CEILING_FIGHTERS * fighter_cap * fighter_power) as f32;
    // The toughest single member's HP (a pure fighter body: 2n parts × 100, unboosted) — matches
    // `capabilities().tank_effective_hp` for the ceiling shape.
    let tank_effective_hp = (fighter_cap * 2 * 100) as f32;
    ForceBudget { max_heal_per_tick, max_dismantle_dps, tank_effective_hp, onsite_budget_ticks: onsite_window }
}

// ═══ ADR 0032 v1.1 — the EV-of-pairing helper (P(win) on the EXISTING squad caps) ═══════════════════════
//
// The auction (ADR 0032 §"EV of a (squad, objective) pairing") scores `EV(S, O) = P(win | caps(S) vs
// O.defense) · value_e(O) − cost`, reusing the ADR 0031 P(win) DECOMPOSITION (`win_probability` for survive
// + the undefended-binary `p_kill` branch from `optimize_composition`) — but fed the EXISTING squad's
// `capabilities()` (already-fielded surviving capability, read once), NOT an `optimize_composition` candidate
// search. This is the v1.1 lift: the same probability model `optimize_composition` uses internally, exposed
// over a fixed `SquadCapabilities` so the manager can rank reassign/claim/stay targets by EV. Travel is
// priced via the SHRINKING on-site window (a farther objective → fewer on-site ticks → less deliverable →
// lower `p_kill`) PLUS a small linear penalty (replacing the ad-hoc proximity tie-break, ADR 0032 line 37).

/// The tunables for the EV-of-pairing helper (ADR 0032 v1.1). Transient (never serialized — no WFV bump),
/// like [`CompositionParams`]. [`Default`] = trust the snapshot, a small travel penalty.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairingParams {
    /// Linear EV penalty per room of travel (the reach delay/exposure cost, ADR 0032 line 30) — added on top
    /// of the on-site-window shrink. Small so it only breaks near-EV ties (replacing the proximity tie-break).
    pub w_travel: f32,
    /// Inflate the observed incoming damage so a GROWING threat still loses (mirrors
    /// [`CompositionParams::dynamic_margin`]). 1.0 = trust the snapshot.
    pub dynamic_margin: f32,
}

impl Default for PairingParams {
    fn default() -> Self {
        PairingParams { w_travel: 1.0, dynamic_margin: 1.0 }
    }
}

/// P(win) for an EXISTING squad's `caps` against `defense` + `enemy`, over an `onsite_window` (ADR 0032
/// v1.1). REUSES the ADR 0031 decomposition VERBATIM (the same `p_survive · p_kill` split, the same
/// undefended-binary `p_kill` branch [`optimize_composition`] uses) — but on a FIXED capability vector, not
/// a candidate search. `p_survive = win_probability(heal, incoming)`; `p_kill` is BINARY (1.0 iff we clear
/// within the window) for a zero-attrition (undefended) target, else the logistic kill-speed curve for a
/// defended one. Pure + deterministic.
pub fn pairing_p_win(
    caps: SquadCapabilities,
    defense: &DefenseProfile,
    enemy: Option<EnemyForce>,
    onsite_window: u32,
    params: &PairingParams,
) -> f32 {
    if defense.safe_mode {
        return 0.0; // safe mode → zero damage possible → can never win (the assess() veto)
    }
    let enemy = enemy.unwrap_or_default();
    let tower_dps = tower_dps_at_assault(&defense.towers);
    let incoming = (tower_dps + enemy.dps) * params.dynamic_margin;
    let required_kill = defense.objective_hits as f32 + defense.breach_hits as f32 + enemy.hits as f32 * params.dynamic_margin;

    let p_survive = win_probability(caps.heal_per_tick as f32, incoming);
    let deliverable = caps.structure_dps as f32 * onsite_window as f32;
    // The SAME undefended-binary vs logistic split optimize_composition uses (FIX 3): a zero-attrition target
    // is a certain win for any force that clears in the window; a defended one uses the kill-speed logistic.
    let undefended = tower_dps == 0.0 && incoming == 0.0;
    let p_kill = if undefended {
        if required_kill <= 0.0 || (deliverable >= required_kill && caps.structure_dps > 0) {
            1.0
        } else {
            0.0
        }
    } else {
        win_probability(deliverable, required_kill)
    };
    p_survive * p_kill
}

/// THE EV of pairing the EXISTING squad (`caps`) with an objective (ADR 0032 §"EV of a (squad, objective)
/// pairing"): `EV = P(win | caps vs defense) · value_e − cost`. `value_e` is the energy-equivalent objective
/// value ([`crate::objective_value::value_e`]); `travel_rooms` prices reach via BOTH the caller's shrinking
/// `onsite_window` (folded into `pairing_p_win`) AND a small linear `w_travel · travel_rooms` penalty. The
/// squad's bodies are already spawned (a reassign/stay choice), so there is no spawn cost — the only `cost`
/// is the travel penalty. Pure + deterministic.
pub fn pairing_ev(
    caps: SquadCapabilities,
    defense: &DefenseProfile,
    enemy: Option<EnemyForce>,
    value_e: f32,
    onsite_window: u32,
    travel_rooms: u32,
    params: &PairingParams,
) -> f32 {
    let p_win = pairing_p_win(caps, defense, enemy, onsite_window, params);
    p_win * value_e - params.w_travel * travel_rooms as f32
}

/// Quantize an EV to a stable integer (ADR 0032 §Determinism / ADR 0020 §6 no-float-into-a-discrete-branch):
/// `(ev · 1000)` rounded, clamped to `i64`. The auction's max-by / threshold comparisons run on THIS, never
/// the raw `f32`, so a result-affecting branch is bit-reproducible.
pub fn quantize_ev(ev: f32) -> i64 {
    (ev * 1000.0).round() as i64
}

/// A composition's per-tick combat output + tank HP at a spawn energy — the force-sizing oracle's
/// `ForceBudget` inputs (ADR 0020 §12.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SquadCapabilities {
    /// Total heal/tick the squad can sustain (Σ HEAL parts × `HEAL_POWER`).
    pub heal_per_tick: u32,
    /// Structure damage/tick (Σ WORK × `DISMANTLE_POWER` + ATTACK × `ATTACK_POWER` + RANGED_ATTACK ×
    /// `RANGED_ATTACK_POWER`) — breach + core-kill (cores are dismantle-immune, so ranged/melee is what kills them).
    pub structure_dps: u32,
    /// Effective HP of the toughest single member (the tank that soaks a tower drain).
    pub tank_effective_hp: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::force_sizing::RequiredForce;

    // ── T2: assemble_force (ADR 0031 P3 — the marginal-fill assembler) ──

    fn member_count_of(req: RequiredForce, energy: u32) -> Option<usize> {
        assemble_force(&req, energy).map(|c| c.slots.len())
    }

    /// The assembler is a pure fold over a frozen Vec-ordered demand list — run-twice-equal (the P3
    /// determinism fence; the assembler is the shared composition path every squad is built through). (ADR 0031 §5.)
    #[test]
    fn assemble_force_is_deterministic() {
        for req in [
            RequiredForce { heal_parts: 12, dismantle_parts: 8, ..Default::default() },
            RequiredForce { heal_parts: 20, immune_struct_parts: 10, anti_creep_parts: 14, ..Default::default() },
            RequiredForce { heal_parts: 40, dismantle_parts: 30, anti_creep_parts: 18, tough_parts: 6, ..Default::default() },
        ] {
            let a = assemble_force(&req, 5600).map(|c| format!("{c:?}"));
            let b = assemble_force(&req, 5600).map(|c| format!("{c:?}"));
            assert_eq!(a, b, "assembler is deterministic for {req:?}");
        }
    }

    /// The Layer-B regression PIN: the assembler's floor is a ROLE-SET (≥1 per demanded role), NEVER a
    /// fixed bucket count — so a tiny dismantle+heal force is 2 members, not snapped to 4, the member
    /// count is MONOTONIC non-decreasing in the force, and 3 is reachable (no 2→4 snap). The former
    /// per-doctrine sizing floored the count at its template's member count (the snap this pins out).
    #[test]
    fn assemble_force_sizes_continuously_no_snap() {
        // A minimal one-of-each-weapon force fields exactly the role set — a DUO (1 Dismantler + 1 Healer),
        // not a 4-member quad.
        assert_eq!(member_count_of(RequiredForce { heal_parts: 5, dismantle_parts: 5, ..Default::default() }, 5600), Some(2), "minimal force is a duo, not a quad");

        // Monotonic non-decreasing as the force grows, and 3 is reachable (continuity, no 1→4 / 2→4 snap).
        let sweep: Vec<usize> = (1..=14)
            .map(|k| member_count_of(RequiredForce { heal_parts: 4 * k, dismantle_parts: 4 * k, ..Default::default() }, 5600).unwrap_or(99))
            .collect();
        for w in sweep.windows(2) {
            assert!(w[1] >= w[0], "member count is monotonic non-decreasing across the sweep: {sweep:?}");
        }
        assert!(sweep.contains(&2) && sweep.contains(&3), "intermediate counts 2 and 3 are reachable (no snap): {sweep:?}");
    }

    /// The role-set viability floor: a force demanding heal + dismantle + ranged fields ≥1 of EACH role —
    /// never "defenders present but no anti-creep" or "healing required but no healer".
    #[test]
    fn assemble_force_fields_the_full_role_set() {
        let req = RequiredForce { heal_parts: 6, dismantle_parts: 6, anti_creep_parts: 8, ..Default::default() };
        let comp = assemble_force(&req, 5600).expect("affordable at RCL7");
        for role in [SquadRole::Healer, SquadRole::Dismantler, SquadRole::RangedDPS] {
            assert!(comp.slots.iter().any(|s| s.role == role), "{role:?} present in {:?}", comp.label);
        }
        // Every member is force-Sized (no catalog body), and the fielded force meets-or-exceeds the demand.
        assert!(comp.slots.iter().all(|s| matches!(s.body_type, BodyType::Sized(_))), "all members are Sized");
        let caps = comp.capabilities(5600);
        assert!(caps.heal_per_tick >= req.heal_parts * HEAL_POWER, "fielded HEAL ≥ required");
    }

    /// RANGED carries BOTH the immune-structure DPS AND the anti-creep kill (additive) — a siege facing a
    /// guard fields enough ranged for both. (The sum the assembler sizes the RangedDPS role to.)
    #[test]
    fn assemble_force_ranged_covers_immune_struct_plus_anti_creep() {
        let req = RequiredForce { immune_struct_parts: 10, anti_creep_parts: 10, ..Default::default() };
        let comp = assemble_force(&req, 5600).expect("affordable");
        let ranged: u32 = comp.slots.iter().filter(|s| s.role == SquadRole::RangedDPS).map(|s| s.body_type.part_count(5600, Part::RangedAttack)).sum();
        assert!(ranged >= 20, "ranged covers immune_struct + anti_creep = 20 parts, got {ranged}");
    }

    /// `None` is a TERMINAL defer (D10): a force past `MAX_SIZED_MEMBERS`, an empty requirement, or a role
    /// that can't field even one member at this energy all return None (no G4-HEAVY failover, no under-size).
    #[test]
    fn assemble_force_defers_terminally() {
        // Empty requirement → nothing to field.
        assert!(assemble_force(&RequiredForce::default(), 5600).is_none(), "empty requirement → None");
        // A huge heal demand at low per-member energy exceeds MAX_SIZED_MEMBERS → None.
        assert!(assemble_force(&RequiredForce { heal_parts: 400, ..Default::default() }, 5600).is_none(), "force past the 8-member cap → None");
        // Energy below a single HEAL+MOVE member's cost → can't field even one → None.
        assert!(assemble_force(&RequiredForce { heal_parts: 4, ..Default::default() }, 100).is_none(), "unaffordable role → None");
    }

    // ── D14: formation footprint derivation (ADR 0031) ──

    /// `formation_for` is total + explicit over the fieldable member counts: 0|1 roams loose, 2 holds a
    /// strict Line, ≥3 hold a strict (compact-box) Box2x2 — pinned for every N=1..=MAX_SIZED_MEMBERS so a
    /// 5-8-member force gets the box intent (not the old "Default(None) ⇒ everyone stacks on one tile" hole).
    #[test]
    fn formation_for_is_total_over_fieldable_counts() {
        assert_eq!(formation_for(0), (FormationShape::None, FormationMode::Loose));
        assert_eq!(formation_for(1), (FormationShape::None, FormationMode::Loose));
        assert_eq!(formation_for(2), (FormationShape::Line, FormationMode::Strict));
        for n in 3..=MAX_SIZED_MEMBERS {
            assert_eq!(formation_for(n), (FormationShape::Box2x2, FormationMode::Strict), "N={n} holds the compact box");
        }
    }

    /// `box_footprint(n)` is the ONE source of the formation footprint: a compact ⌈√n⌉ × ⌈n/⌈√n⌉⌉ box that
    /// holds exactly n members (generalizing the old hardcoded 2×2). Pinned for N=1..=8: width ≥ height,
    /// the area holds n (`w*h >= n`), and the box is minimal (dropping a row would no longer hold n).
    #[test]
    fn box_footprint_is_a_compact_box_for_each_count() {
        let expected = [(1, (1u8, 1u8)), (2, (2, 1)), (3, (2, 2)), (4, (2, 2)), (5, (3, 2)), (6, (3, 2)), (7, (3, 3)), (8, (3, 3))];
        for (n, want) in expected {
            let (w, h) = box_footprint(n);
            assert_eq!((w, h), want, "box_footprint({n})");
            assert!((w as usize) * (h as usize) >= n, "box holds all {n} members");
            assert!(w >= h, "width ≥ height (anchor top-left, fills right then down)");
            // Minimal: a box one row shorter could not hold n.
            assert!((w as usize) * (h as usize - 1) < n, "box is minimal for {n}");
        }
        // n == 0 is the degenerate 1×1 (no members).
        assert_eq!(box_footprint(0), (1, 1));
    }

    // ── D16/D17: the EV composition optimizer ──

    use crate::force_sizing::TowerThreat;

    /// A winnable dismantle-able ring: a thin breach + a small core, one weak far tower.
    fn winnable_struct() -> DefenseProfile {
        DefenseProfile {
            towers: vec![TowerThreat { range_to_assault: 18, energy: 200 }],
            breach_hits: 10_000,
            objective_hits: 50_000,
            repair_per_tick: 0.0,
            safe_mode: false,
        }
    }

    /// The optimizer is a pure fold over two Vec-ordered ladders → run-twice-equal (the D16 determinism
    /// fence; the optimizer is the shared composition path every gated/always-field squad is built through).
    #[test]
    fn optimize_composition_is_deterministic() {
        let p = CompositionParams { member_energy: 5600, ..Default::default() };
        let defense = winnable_struct();
        let run = || {
            optimize_composition(
                DoctrineObjective::DismantleStructure,
                &defense,
                None,
                100_000.0,
                1400,
                EnemyCoordination::Individual,
                0.0,
                true,
                false,
                &p,
            )
            .map(|c| format!("{c:?}"))
        };
        assert_eq!(run(), run(), "the optimizer is deterministic");
        assert!(run().is_some(), "a winnable ring commits at Default");
    }

    /// A GATED doctrine defers (None) when EV ≤ the commit threshold: a tiny target value can't clear a
    /// raised threshold, so the optimizer returns None even though the target is technically winnable.
    #[test]
    fn optimize_composition_defers_below_commit_threshold() {
        let defense = winnable_struct();
        // A high commit threshold + a modest value ⇒ EV ≤ threshold ⇒ defer.
        let strict = CompositionParams { member_energy: 5600, commit_ev_threshold: 10_000.0, ..Default::default() };
        let deferred = optimize_composition(
            DoctrineObjective::DismantleStructure,
            &defense,
            None,
            1.0, // tiny target value
            1400,
            EnemyCoordination::Individual,
            0.0,
            true, // gated
            false,
            &strict,
        );
        assert!(deferred.is_none(), "gated + EV ≤ commit threshold → defer (None)");

        // The SAME bed at a high value + Default threshold commits (winnable → fielded), preserving the
        // OracleCalibration FP/FN semantics.
        let committed = optimize_composition(
            DoctrineObjective::DismantleStructure,
            &defense,
            None,
            100_000.0,
            1400,
            EnemyCoordination::Individual,
            0.0,
            true,
            false,
            &CompositionParams { member_energy: 5600, ..Default::default() },
        );
        assert!(committed.is_some(), "winnable target commits at Default");
    }

    /// A high-value target OVER-INVESTS vs a low-value one. On a creep-clear bed (a gated raid) the
    /// requirement is sized to the OBSERVED enemy (not the gross structure ceiling), so it leaves room below
    /// the 8-member cap for the over-power ladder to climb: a modest enemy with a near tower sits at ~0.82
    /// P(win) at the minimal force, so a HIGH value pays for the extra anti-creep + heal (more P(win)) while
    /// a LOW value picks the cheaper minimal force. (When the minimal force already wins ~certainly, NOT
    /// over-investing is correct — so the bed leaves P(win) headroom for the knob to bite.)
    #[test]
    fn optimize_composition_over_invests_for_high_value() {
        // A mid-range energized tower adds incoming the heal must out-pace; a modest defender force the kill
        // must out-power. The minimal force (×hold_margin / ×over_power) clears within 8 members.
        let defense = DefenseProfile {
            towers: vec![TowerThreat { range_to_assault: 14, energy: 1000 }],
            breach_hits: 0,
            objective_hits: 0,
            repair_per_tick: 0.0,
            safe_mode: false,
        };
        let enemy = Some(EnemyForce { dps: 60.0, heal: 0.0, hits: 3_000, count: 2, boosted: false });
        // A cost weight that makes the ladder rungs a real EV trade-off, but low enough that a modest value
        // still commits the minimal force.
        let p = CompositionParams { member_energy: 5600, w_energy: 0.01, ..Default::default() };
        let opt = |value: f32| {
            optimize_composition(DoctrineObjective::RaidCreeps, &defense, enemy, value, 1400, EnemyCoordination::Coordinated, 0.0, true, false, &p)
                .expect("winnable at both values")
        };
        // A small value: P(win) is already good at the minimal force, so the extra over-power doesn't pay
        // for its cost → minimal force. A huge value: the P(win) headroom is worth far more than the cost →
        // climb the over-power ladder.
        let low = opt(500.0);
        let high = opt(50_000_000.0);
        assert!(
            high.estimated_cost(5600) > low.estimated_cost(5600),
            "high value over-invests (more energy / parts): high {} vs low {}",
            high.estimated_cost(5600),
            low.estimated_cost(5600)
        );
    }

    /// A WINNABLE target commits at Default params (the calibration-preserving floor): EV > 0 at a sensible
    /// target value for a force that can actually take the bed.
    #[test]
    fn optimize_composition_commits_a_winnable_target_at_default() {
        let comp = optimize_composition(
            DoctrineObjective::KillImmuneStructure,
            &DefenseProfile { towers: vec![TowerThreat { range_to_assault: 15, energy: 200 }], breach_hits: 0, objective_hits: 100_000, repair_per_tick: 0.0, safe_mode: false },
            None,
            100_000.0,
            1400,
            EnemyCoordination::Individual,
            0.0,
            true,
            false,
            &CompositionParams { member_energy: 5600, ..Default::default() },
        )
        .expect("a winnable immune core commits at Default");
        // An immune core is killed by RANGED, not WORK.
        assert!(comp.slots.iter().any(|s| s.role == SquadRole::RangedDPS), "immune core fields RANGED: {}", comp.label);
    }

    /// FIX 3 — an UNDEFENDED, zero-attrition level-0 invader core (towers=[], enemy dps=0, hits~100k) MUST
    /// size to the MINIMAL clearing force (the operator's "fewest creeps / most efficient" goal), NOT climb
    /// the over-power ladder to the 4-5 RangedDPS over-size the EV optimizer used to pick. With nothing
    /// shooting back the win is certain for any force that razes the core within the on-site window, so
    /// `p_kill` is binary and the EV-max collapses to the cheapest in-time force.
    #[test]
    fn optimize_composition_sizes_an_undefended_core_to_a_minimal_force() {
        let undefended = DefenseProfile {
            towers: vec![], // no towers
            breach_hits: 0,
            objective_hits: 100_000,
            repair_per_tick: 0.0,
            safe_mode: false,
        };
        let comp = optimize_composition(
            DoctrineObjective::KillImmuneStructure,
            &undefended,
            None, // no enemy force
            100_000.0,
            1400,
            EnemyCoordination::Individual,
            0.0,
            true,
            false,
            &CompositionParams { member_energy: 5600, ..Default::default() },
        )
        .expect("an undefended core is trivially winnable");
        // MINIMAL: strictly fewer than the old 4-5-member over-size.
        assert!(
            comp.member_count() <= 3,
            "undefended core must size to a minimal force (≤3), got {} ({})",
            comp.member_count(),
            comp.label
        );
        // Still actually fields a RANGED kill (an immune core needs ranged) — i.e. it's a real clearing force,
        // not an empty one.
        assert!(
            comp.slots.iter().any(|s| s.role == SquadRole::RangedDPS),
            "undefended immune core still fields a RANGED kill: {}",
            comp.label
        );
    }

    /// STABLE undefended sizing (deep-reach fix — Break #1 / the roster-stall avoidance): an undefended core
    /// sizes to a STABLE single slot across the per-tick `onsite_window` jitter (CREEP_LIFE_TIME − spawn −
    /// travel, which shrinks as the squad travels). If the member count OSCILLATED with the window (1↔2),
    /// the requested-slot count would flap each reconcile → the rally gate + Phase-B spawn churn → the
    /// roster never completes (the live W9N8-style oscillation that re-introduces the stall). A small core
    /// that one RANGED member razes well within even the SHORTEST realistic window must stay a single slot.
    #[test]
    fn undefended_core_sizes_to_a_stable_single_slot_across_windows() {
        // A small undefended core a single capped RANGED member clears within ~hundreds of ticks — so it is
        // winnable across the whole realistic on-site window range (no window forces a 2nd member).
        let core = DefenseProfile { towers: vec![], breach_hits: 0, objective_hits: 8_000, repair_per_tick: 0.0, safe_mode: false };
        let p = CompositionParams { member_energy: 5600, ..Default::default() };
        let counts: Vec<usize> = [400u32, 600, 900, 1200, 1400]
            .iter()
            .map(|&window| {
                optimize_composition(DoctrineObjective::KillImmuneStructure, &core, None, 100_000.0, window, EnemyCoordination::Individual, 0.0, true, false, &p)
                    .expect("a small undefended core is winnable at every realistic window")
                    .member_count()
            })
            .collect();
        assert!(
            counts.iter().all(|&n| n == 1),
            "an undefended small core must size to a STABLE single slot across the on-site-window range (no 1↔2 oscillation), got {counts:?}"
        );
    }

    /// FIX 3 scoping guard — a DEFENDED high-value core MUST still OVER-INVEST via the over-power path
    /// (real attrition ⇒ p_kill headroom is correct), while the SAME bed UNDEFENDED sizes minimal. This is
    /// the DEFENDED-vs-UNDEFENDED distinction the FIX 3 scope hinges on: the over-power climb is preserved
    /// for defended targets (the calibration gates test this) and removed only for zero-attrition ones.
    #[test]
    fn over_invest_distinguishes_defended_from_undefended() {
        let p = CompositionParams { member_energy: 5600, w_energy: 0.01, ..Default::default() };
        let undefended = DefenseProfile {
            towers: vec![],
            breach_hits: 0,
            objective_hits: 100_000,
            repair_per_tick: 0.0,
            safe_mode: false,
        };
        let defended = DefenseProfile {
            towers: vec![TowerThreat { range_to_assault: 14, energy: 1000 }], // energized tower → real attrition
            objective_hits: 100_000,
            ..undefended.clone()
        };
        let opt = |d: &DefenseProfile| {
            optimize_composition(
                DoctrineObjective::KillImmuneStructure,
                d,
                None,
                50_000_000.0, // a huge value — the over-power knob bites hard IF there is attrition headroom
                1400,
                EnemyCoordination::Individual,
                0.0,
                true,
                false,
                &p,
            )
            .expect("winnable")
        };
        let undef = opt(&undefended);
        let def = opt(&defended);
        // Even at a huge value the UNDEFENDED core stays minimal (no attrition ⇒ no P(win) to buy), while the
        // DEFENDED core over-invests (real attrition ⇒ over-power pays). The defended force is strictly larger.
        assert!(
            def.estimated_cost(5600) > undef.estimated_cost(5600),
            "defended high-value over-invests vs undefended minimal: defended {} vs undefended {}",
            def.estimated_cost(5600),
            undef.estimated_cost(5600)
        );
        assert!(
            undef.member_count() <= 3,
            "undefended stays minimal even at huge value: {} members",
            undef.member_count()
        );
    }

    // ═══ ADR 0032 v1.1 — the EV-of-pairing helper + the EV-positive gate ═══════════════════════════════

    /// A real fielded squad's caps: assemble a ranged+heal force at `energy` and read its `capabilities`.
    fn squad_caps(ranged: u32, heal: u32, energy: u32) -> SquadCapabilities {
        let req = RequiredForce { immune_struct_parts: ranged, heal_parts: heal, ..Default::default() };
        assemble_force(&req, energy).expect("fieldable").capabilities(energy)
    }

    /// `pairing_p_win` REUSES the ADR 0031 decomposition: an undefended target the squad clears in the window
    /// is a CERTAIN win (1.0); a heavily-defended target the squad can't out-heal is a near-certain LOSS.
    #[test]
    fn pairing_p_win_reuses_the_decomposition() {
        let caps = squad_caps(10, 5, 5600);
        let p = PairingParams::default();
        // Undefended small core, ample window → certain win.
        let undef = DefenseProfile { objective_hits: 5_000, ..Default::default() };
        assert_eq!(pairing_p_win(caps, &undef, None, 1400, &p), 1.0, "undefended in-window = certain win");
        // Safe mode → can never win.
        let safe = DefenseProfile { objective_hits: 5_000, safe_mode: true, ..Default::default() };
        assert_eq!(pairing_p_win(caps, &safe, None, 1400, &p), 0.0, "safe mode vetoes the win");
        // Heavy tower fire the small squad can't out-heal → low survive probability.
        let towered = DefenseProfile { towers: vec![TowerThreat { range_to_assault: 5, energy: 1000 }, TowerThreat { range_to_assault: 5, energy: 1000 }], objective_hits: 5_000, ..Default::default() };
        assert!(pairing_p_win(caps, &towered, None, 1400, &p) < 0.5, "an out-healed squad has a low P(win)");
    }

    /// `pairing_ev` = P(win) · value_e − travel cost. A high-value winnable objective beats a low-value one;
    /// a farther objective scores lower (the travel penalty replaces the proximity tie-break).
    #[test]
    fn pairing_ev_ranks_value_and_penalizes_travel() {
        let caps = squad_caps(10, 5, 5600);
        let p = PairingParams::default();
        let undef = DefenseProfile { objective_hits: 5_000, ..Default::default() };
        let near_high = pairing_ev(caps, &undef, None, 100_000.0, 1400, 1, &p);
        let near_low = pairing_ev(caps, &undef, None, 1_000.0, 1400, 1, &p);
        assert!(near_high > near_low, "higher value_e ⇒ higher EV");
        let far_high = pairing_ev(caps, &undef, None, 100_000.0, 1400, 5, &p);
        assert!(near_high > far_high, "the same objective farther away scores lower (travel penalty)");
    }

    /// THE EV-POSITIVE GATE + the dps=0 fix (ADR 0032 §EV-positive gate): a HARMLESS / LOW-VALUE objective is
    /// NOT taken — its EV does not beat StayPut by the commit threshold. Using `value_e` for a dps=0 defend
    /// objective (≈0 value) the reassign EV is ~0, far below a real current fight's EV → the gate holds.
    #[test]
    fn ev_positive_gate_rejects_a_harmless_low_value_objective() {
        use crate::objective_value::{value_e, ObjectiveIntel, ObjectiveValueKind};
        let caps = squad_caps(10, 5, 5600);
        let p = PairingParams::default();
        let undef = DefenseProfile { objective_hits: 5_000, ..Default::default() };

        // A dps=0 harmless threat in an owned room: value_e ≈ 0 → the pairing EV is ~0 (minus travel).
        let harmless_value = value_e(ObjectiveValueKind::Defend, &ObjectiveIntel { asset_value: 1_000_000.0, threat_danger: 0.0, ..Default::default() });
        let ev_harmless = pairing_ev(caps, &undef, None, harmless_value, 1400, 1, &p);

        // StayPut on a genuinely dangerous current objective (high value_e).
        let dangerous_value = value_e(ObjectiveValueKind::Defend, &ObjectiveIntel { asset_value: 1_000_000.0, threat_danger: 300.0, ..Default::default() });
        let ev_stay = pairing_ev(caps, &undef, None, dangerous_value, 1400, 0, &p);

        let commit_ev_threshold = 1.0; // the ADR 0031 knob, reused
        let should_reassign = quantize_ev(ev_harmless) - quantize_ev(ev_stay) > quantize_ev(commit_ev_threshold);
        assert!(!should_reassign, "a harmless dps=0 objective must NOT pull the squad off a real fight (ev_harmless={ev_harmless}, ev_stay={ev_stay})");
        assert!(ev_harmless <= 0.0, "a dps=0 defend objective has ~zero EV (the over-response fix): {ev_harmless}");
    }

    /// The gate must NOT starve REAL defense: a genuinely dangerous (high-dps) threat with a high value_e
    /// IS taken over a StayPut on nothing (EV beats the threshold).
    #[test]
    fn ev_positive_gate_still_fields_a_genuinely_dangerous_threat() {
        use crate::objective_value::{value_e, ObjectiveIntel, ObjectiveValueKind};
        let caps = squad_caps(10, 5, 5600);
        let p = PairingParams::default();
        let undef = DefenseProfile { objective_hits: 5_000, ..Default::default() };
        let dangerous_value = value_e(ObjectiveValueKind::Defend, &ObjectiveIntel { asset_value: 1_000_000.0, threat_danger: 300.0, ..Default::default() });
        let ev_new = pairing_ev(caps, &undef, None, dangerous_value, 1400, 1, &p);
        let ev_idle = 0.0; // StayPut on no objective is worth nothing
        let commit_ev_threshold = 1.0;
        let should_take = quantize_ev(ev_new) - quantize_ev(ev_idle) > quantize_ev(commit_ev_threshold);
        assert!(should_take, "a dangerous threat must still field a defender (ev_new={ev_new})");
    }

    /// `quantize_ev` is stable + deterministic (ADR 0032 §Determinism / ADR 0020 §6).
    #[test]
    fn quantize_ev_is_stable() {
        assert_eq!(quantize_ev(1.2344), 1234);
        assert_eq!(quantize_ev(1.2345), quantize_ev(1.2345));
        assert_eq!(quantize_ev(0.0), 0);
    }

    /// The pairing helpers are deterministic (run twice → identical).
    #[test]
    fn pairing_is_deterministic() {
        let caps = squad_caps(8, 4, 4000);
        let p = PairingParams::default();
        let def = DefenseProfile { towers: vec![TowerThreat { range_to_assault: 10, energy: 500 }], objective_hits: 20_000, ..Default::default() };
        let a = pairing_ev(caps, &def, Some(EnemyForce { dps: 30.0, ..Default::default() }), 50_000.0, 900, 3, &p);
        let b = pairing_ev(caps, &def, Some(EnemyForce { dps: 30.0, ..Default::default() }), 50_000.0, 900, 3, &p);
        assert_eq!(quantize_ev(a), quantize_ev(b));
    }
}
