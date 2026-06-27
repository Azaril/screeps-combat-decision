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
use crate::force_sizing::RequiredForce;
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

/// Formation for an assembled force of `count` members — the shape is DERIVED from the member count: a lone
/// member roams loose, a 2-member force holds a strict line ([`FormationShape::Line`]), ≥3 hold a strict box
/// ([`FormationShape::Box2x2`]). (Line/Box2x2 are the current [`FormationShape`] variants in use here,
/// pending the deferred footprint cleanup.)
fn formation_for(count: usize) -> (FormationShape, FormationMode) {
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
    let demands: [(SquadRole, u32); 4] = [
        (SquadRole::Healer, req.heal_parts),
        (SquadRole::Dismantler, req.dismantle_parts),
        (SquadRole::RangedDPS, req.immune_struct_parts + req.anti_creep_parts),
        (SquadRole::Tank, req.tough_parts),
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

/// Fighters in the winnability CEILING (the strongest single squad the oracle judges against — the
/// assembler can field up to [`MAX_SIZED_MEMBERS`], so a "winnable" verdict from this ceiling stays
/// conservative). 3 fighters + 5 healers = 8 (the eval's long-standing `siege_ceiling` shape).
const CEILING_FIGHTERS: usize = 3;
const CEILING_HEALERS: usize = 5;

/// The template-free winnability CEILING (ADR 0031 P4) — the BUDGET source the driver uses:
/// `force_ceiling(energy, fighter).force_budget(..)` is the oracle's `ForceBudget` with NO catalog
/// constructor in sight. `fighter` is the kill weapon role (`Dismantler` for
/// dismantle-able rings, `RangedDPS` for immune cores / creep clear). Each member is sized at the SAME
/// per-member cap the ASSEMBLER uses (`min(energy, PREFERRED_MEMBER_ENERGY)`) — so the ceiling represents a
/// force the assembler can actually field within [`MAX_SIZED_MEMBERS`], NOT an over-stated full-energy blob
/// that `assess` would size a required force past the 8-member cap to match. Conservative: the assembler can
/// still grow toward this ceiling, so a "winnable" verdict stays safe.
pub fn force_ceiling(member_energy: u32, fighter: SquadRole) -> SquadComposition {
    let probe = member_energy.min(PREFERRED_MEMBER_ENERGY);
    let fighter_cap = single_role_cap(fighter, probe);
    let heal_cap = single_role_cap(SquadRole::Healer, probe);
    let mut slots = Vec::new();
    if fighter_cap > 0 {
        for _ in 0..CEILING_FIGHTERS {
            slots.push(SquadSlot { role: fighter, body_type: BodyType::Sized(single_role_spec(fighter, fighter_cap)) });
        }
    }
    if heal_cap > 0 {
        for _ in 0..CEILING_HEALERS {
            slots.push(SquadSlot { role: SquadRole::Healer, body_type: BodyType::Sized(single_role_spec(SquadRole::Healer, heal_cap)) });
        }
    }
    let (formation_shape, formation_mode) = formation_for(slots.len());
    SquadComposition { label: "Force Ceiling".into(), slots, formation_shape, formation_mode, retreat_threshold: default_retreat_threshold() }
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

    /// `force_ceiling` (the template-free budget source, ADR 0031 P4) builds the conservative ceiling:
    /// CEILING_FIGHTERS fighters + CEILING_HEALERS healers, each force-Sized + maxed, with a sane budget.
    /// `Dismantler` fields WORK; `RangedDPS` fields RANGED — and the budget's structure DPS reflects it.
    #[test]
    fn force_ceiling_builds_the_budget_source() {
        let siege = force_ceiling(5600, SquadRole::Dismantler);
        assert_eq!(siege.slots.iter().filter(|s| s.role == SquadRole::Dismantler).count(), CEILING_FIGHTERS);
        assert_eq!(siege.slots.iter().filter(|s| s.role == SquadRole::Healer).count(), CEILING_HEALERS);
        assert!(siege.slots.iter().all(|s| matches!(s.body_type, BodyType::Sized(_))), "ceiling is all force-Sized (no catalog)");
        let b = siege.force_budget(5600, 1400);
        assert!(b.max_heal_per_tick > 0.0 && b.max_dismantle_dps > 0.0, "siege ceiling budget: {b:?}");
        // Ranged ceiling fields RANGED structure DPS (immune cores / creep clear).
        let ranged = force_ceiling(5600, SquadRole::RangedDPS);
        assert!(ranged.slots.iter().any(|s| matches!(s.body_type, BodyType::Sized(spec) if spec.ranged_attack > 0)), "ranged ceiling fields RANGED");
        assert!(ranged.force_budget(5600, 1400).max_dismantle_dps > 0.0, "ranged ceiling has structure DPS via RANGED");
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
}
