//! Squad composition + role model — the data-driven "what a squad should look like when fully
//! spawned" (slots × body types × formation), the body-type selector that maps each slot to a
//! concrete body (static template or force-`Sized` spec), and the force-driven sizing (`sized_for`)
//! that turns a [`crate::force_sizing`] verdict into a fielded composition.
//!
//! Lives in the decision crate (not the bot) so the sim/eval field the bot's REAL composition with
//! one implementation (no replica) — pure over `screeps-game-api` value types + the engine's
//! mechanics, no `game::*`. `SquadComposition::estimated_combat_time`/`is_viable_from` take a
//! precomputed `travel_ticks` (the bot owns the `PathfinderService`; see `war::best_force_budget`).

use crate::bodies;
use crate::force_sizing::RequiredForce;
use crate::spawning::{create_body, SpawnBodyDefinition};
use screeps::{Part, ResourceType};
use screeps_combat_engine::constants::{ATTACK_POWER, CREEP_LIFE_TIME, DISMANTLE_POWER, HEAL_POWER, RANGED_ATTACK_POWER};
use serde::{Deserialize, Serialize};

/// Ticks per body part to spawn (`CREEP_SPAWN_TIME`) — an engine constant not modeled in
/// `screeps_combat_engine::constants`; kept here for the spawn-time estimate.
const CREEP_SPAWN_TIME: u32 = 3;

/// Most members a single force-sized squad may grow to (D3 member-count scaling). Beyond this the
/// target needs the multi-squad **G4-HEAVY** path (P5), so [`SquadComposition::sized_for`] defers
/// rather than field an unmanageable blob. 2× a quad — enough to out-heal an L1-2 stronghold /
/// multi-keeper SK at RCL7+, bounded for formation + CPU sanity.
const MAX_SIZED_MEMBERS: usize = 8;

/// Most parts of ONE role-type a single sized member can carry: a pure single-part body on plains
/// (1:1 MOVE) is `2n` parts, so the 50-part engine cap bounds `n` at 25. The upper bound of the
/// per-member capacity search in [`SquadComposition::sized_for`].
const MAX_SINGLE_ROLE_PARTS: u32 = 25;

/// Preferred per-member energy ceiling for force-sized members — kept BELOW the 50-part / 25-role-part
/// hard max so a sized member is reliably bankable at HIGH spawn priority while CRITICAL economy creeps
/// drain the home. This splits a force across MORE, SMALLER members instead of one un-spawnable ~5000e
/// blob that re-queues forever and never departs (the live W7N7 25-RANGED / W7N4 16-HEAL bug: a 5000e
/// member is ~90% of an RCL7 spawn's capacity and never accumulates while miners drain it). At ~3000 a
/// member is ~half an RCL7 capacity — easily banked — yet counts stay within [`MAX_SIZED_MEMBERS`] for
/// normal targets. The 50-part engine cap and [`MAX_SINGLE_ROLE_PARTS`] remain the hard CEILING; this
/// only ever LOWERS the capacity probe. (~3000 ⇒ 15 RANGED+15 MOVE = 3000e, or 10 HEAL+10 MOVE = 3000e.)
/// Also used by the spawn path (`queue_slot_spawn`) to cap TEMPLATE bodies (the `sized_for`-deferred
/// fallback shapes), so every spawned member — sized or template — stays bankable.
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

/// Enum of body definition selectors (maps to functions in [`crate::bodies`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyType {
    SoloDefender,
    DuoRangedAttacker,
    DuoMeleeAttacker,
    DuoHealer,
    QuadMember,
    Tank,
    Drain,
    Harasser,
    Dismantler,
    // Specialized roles
    SkRangedAttacker,
    SkHealer,
    PowerBankAttacker,
    PowerBankHealer,
    SiegeDismantler,
    CoreAttacker,
    Hauler,
    // Boosted variants
    BoostedQuadMember,
    BoostedDuoHealer,
    BoostedDuoRangedAttacker,
    BoostedTank,
    /// A force-SIZED body (R3, ADR 0020 §12.6): explicit part counts from the force-sizing solver,
    /// built via `bodies::build_combat_body` rather than a static template. APPENDED LAST so existing
    /// serialized variant discriminants are unchanged (forward-compatible decode).
    Sized(bodies::CombatBodySpec),
}

impl BodyType {
    /// Get the SpawnBodyDefinition for this body type at a given energy capacity.
    pub fn body_definition(&self, max_energy: u32) -> SpawnBodyDefinition<'static> {
        match self {
            BodyType::SoloDefender => bodies::solo_defender_body(max_energy),
            BodyType::DuoRangedAttacker => bodies::duo_ranged_attacker_body(max_energy),
            BodyType::DuoMeleeAttacker => bodies::duo_melee_attacker_body(max_energy),
            BodyType::DuoHealer => bodies::duo_healer_body(max_energy),
            BodyType::QuadMember => bodies::quad_member_body(max_energy),
            BodyType::Tank => bodies::tank_body(max_energy),
            BodyType::Drain => bodies::drain_body(max_energy),
            BodyType::Harasser => bodies::harasser_body(),
            BodyType::Dismantler => bodies::dismantler_body(max_energy),
            BodyType::SkRangedAttacker => bodies::sk_ranged_attacker_body(max_energy),
            BodyType::SkHealer => bodies::sk_healer_body(max_energy),
            BodyType::PowerBankAttacker => bodies::power_bank_attacker_body(max_energy),
            BodyType::PowerBankHealer => bodies::power_bank_healer_body(max_energy),
            BodyType::SiegeDismantler => bodies::siege_dismantler_body(max_energy),
            BodyType::CoreAttacker => bodies::core_attacker_body(max_energy),
            BodyType::Hauler => bodies::hauler_body(max_energy),
            BodyType::BoostedQuadMember => bodies::boosted_quad_member_body(max_energy),
            BodyType::BoostedDuoHealer => bodies::boosted_duo_healer_body(max_energy),
            BodyType::BoostedDuoRangedAttacker => bodies::boosted_duo_ranged_attacker_body(max_energy),
            BodyType::BoostedTank => bodies::boosted_tank_body(max_energy),
            BodyType::Sized(_) => unreachable!("Sized bodies build via BodyType::build_body, not body_definition"),
        }
    }

    /// Build the spawn body for this body type at `max_energy` over `move_profile`: a `Sized` spec via
    /// the dynamic builder (R1), else the static template through `create_body`. `None` ⇒ can't build /
    /// can't afford. The single body-producing entry point for the spawn path (handles both kinds).
    pub fn build_body(&self, max_energy: u32, move_profile: bodies::MoveProfile) -> Option<Vec<Part>> {
        match self {
            BodyType::Sized(spec) => bodies::build_combat_body(spec, move_profile, max_energy),
            other => create_body(&other.body_definition(max_energy)).ok(),
        }
    }

    /// Estimate the body cost at a given energy capacity.
    pub fn estimated_cost(&self, max_energy: u32) -> u32 {
        if let BodyType::Sized(spec) = self {
            let moves = bodies::MoveProfile::Plains.move_parts(spec.non_move_parts());
            return spec.tough * Part::Tough.cost()
                + spec.attack * Part::Attack.cost()
                + spec.ranged_attack * Part::RangedAttack.cost()
                + spec.work * Part::Work.cost()
                + spec.carry * Part::Carry.cost()
                + spec.heal * Part::Heal.cost()
                + moves * Part::Move.cost();
        }
        let def = self.body_definition(max_energy);
        let pre_cost: u32 = def.pre_body.iter().map(|p| p.cost()).sum();
        let post_cost: u32 = def.post_body.iter().map(|p| p.cost()).sum();
        let repeat_cost: u32 = def.repeat_body.iter().map(|p| p.cost()).sum();
        let fixed_cost = pre_cost + post_cost;
        let remaining = max_energy.saturating_sub(fixed_cost);

        if repeat_cost == 0 {
            return fixed_cost;
        }

        let fixed_len = def.pre_body.len() + def.post_body.len();
        let max_by_cost = remaining / repeat_cost;
        let max_by_size = if !def.repeat_body.is_empty() {
            (50usize.saturating_sub(fixed_len)) / def.repeat_body.len()
        } else {
            0
        };
        let repeats = max_by_cost.min(max_by_size as u32);
        let repeats = match def.maximum_repeat {
            Some(max) => repeats.min(max as u32),
            None => repeats,
        };

        fixed_cost + repeats * repeat_cost
    }

    /// Estimate the number of body parts at a given energy capacity.
    pub fn estimated_part_count(&self, max_energy: u32) -> u32 {
        if let BodyType::Sized(spec) = self {
            return spec.non_move_parts() + bodies::MoveProfile::Plains.move_parts(spec.non_move_parts());
        }
        let def = self.body_definition(max_energy);
        let pre_cost: u32 = def.pre_body.iter().map(|p| p.cost()).sum();
        let post_cost: u32 = def.post_body.iter().map(|p| p.cost()).sum();
        let repeat_cost: u32 = def.repeat_body.iter().map(|p| p.cost()).sum();
        let fixed_cost = pre_cost + post_cost;
        let remaining = max_energy.saturating_sub(fixed_cost);

        if repeat_cost == 0 {
            return (def.pre_body.len() + def.post_body.len()) as u32;
        }

        let fixed_len = def.pre_body.len() + def.post_body.len();
        let max_by_cost = remaining / repeat_cost;
        let max_by_size = if !def.repeat_body.is_empty() {
            (50usize.saturating_sub(fixed_len)) / def.repeat_body.len()
        } else {
            0
        };
        let repeats = max_by_cost.min(max_by_size as u32);
        let repeats = match def.maximum_repeat {
            Some(max) => repeats.min(max as u32),
            None => repeats,
        };

        (fixed_len as u32) + repeats * (def.repeat_body.len() as u32)
    }

    /// Count of `part` in the expanded body at `max_energy` — the per-part-type input the force-sizing
    /// oracle needs (ADR 0020 §12.2). Mirrors `estimated_part_count`'s repeat math but counts one type.
    pub fn part_count(&self, max_energy: u32, part: Part) -> u32 {
        if let BodyType::Sized(spec) = self {
            return match part {
                Part::Tough => spec.tough,
                Part::Attack => spec.attack,
                Part::RangedAttack => spec.ranged_attack,
                Part::Work => spec.work,
                Part::Carry => spec.carry,
                Part::Heal => spec.heal,
                Part::Move => bodies::MoveProfile::Plains.move_parts(spec.non_move_parts()),
                _ => 0,
            };
        }
        let def = self.body_definition(max_energy);
        let in_slice = |s: &[Part]| s.iter().filter(|p| **p == part).count() as u32;
        let fixed = in_slice(def.pre_body) + in_slice(def.post_body);
        let per_repeat = in_slice(def.repeat_body);
        if per_repeat == 0 {
            return fixed;
        }

        let repeat_cost: u32 = def.repeat_body.iter().map(|p| p.cost()).sum();
        let pre_cost: u32 = def.pre_body.iter().map(|p| p.cost()).sum();
        let post_cost: u32 = def.post_body.iter().map(|p| p.cost()).sum();
        let fixed_cost = pre_cost + post_cost;
        let fixed_len = def.pre_body.len() + def.post_body.len();
        let max_by_cost = max_energy.saturating_sub(fixed_cost) / repeat_cost.max(1);
        let max_by_size = (50usize.saturating_sub(fixed_len)) / def.repeat_body.len().max(1);
        let repeats = max_by_cost.min(max_by_size as u32);
        let repeats = match def.maximum_repeat {
            Some(max) => repeats.min(max as u32),
            None => repeats,
        };
        fixed + per_repeat * repeats
    }

    /// List the boost compounds required for this body type (if boosted).
    pub fn required_boosts(&self) -> Vec<(ResourceType, u32)> {
        match self {
            BodyType::BoostedQuadMember => vec![
                (bodies::boosts::TOUGH_BOOST, 6),
                (bodies::boosts::RANGED_ATTACK_BOOST, 10),
                (bodies::boosts::HEAL_BOOST, 10),
                (bodies::boosts::MOVE_BOOST, 10),
            ],
            BodyType::BoostedDuoHealer => vec![
                (bodies::boosts::TOUGH_BOOST, 8),
                (bodies::boosts::HEAL_BOOST, 20),
                (bodies::boosts::MOVE_BOOST, 6),
            ],
            BodyType::BoostedDuoRangedAttacker => vec![
                (bodies::boosts::TOUGH_BOOST, 6),
                (bodies::boosts::RANGED_ATTACK_BOOST, 20),
                (bodies::boosts::MOVE_BOOST, 6),
            ],
            BodyType::BoostedTank => vec![
                (bodies::boosts::TOUGH_BOOST, 12),
                (bodies::boosts::ATTACK_BOOST, 15),
                (bodies::boosts::MOVE_BOOST, 8),
            ],
            _ => Vec::new(),
        }
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
    // ─── Predefined compositions ────────────────────────────────────────

    /// 1 ranged+heal creep, no formation.
    pub fn solo_ranged() -> Self {
        SquadComposition {
            label: "Solo Ranged".into(),
            slots: vec![SquadSlot {
                role: SquadRole::RangedDPS,
                body_type: BodyType::SoloDefender,
            }],
            formation_shape: FormationShape::None,
            formation_mode: FormationMode::Loose,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 1 ranged attacker + 1 healer, line formation.
    pub fn duo_attack_heal() -> Self {
        SquadComposition {
            label: "Duo Attack+Heal".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::RangedDPS,
                    body_type: BodyType::DuoRangedAttacker,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Line,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 1 tank + 1 healer, line formation.
    pub fn duo_tank_heal() -> Self {
        SquadComposition {
            label: "Duo Tank+Heal".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::Tank,
                    body_type: BodyType::Tank,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Line,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 2 ranged + 2 healers, box formation, strict mode.
    pub fn quad_ranged() -> Self {
        SquadComposition {
            label: "Quad Ranged".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::RangedDPS,
                    body_type: BodyType::QuadMember,
                },
                SquadSlot {
                    role: SquadRole::RangedDPS,
                    body_type: BodyType::QuadMember,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Box2x2,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 2 dismantlers + 2 healers, box formation.
    pub fn quad_siege() -> Self {
        SquadComposition {
            label: "Quad Siege".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::Dismantler,
                    body_type: BodyType::Dismantler,
                },
                SquadSlot {
                    role: SquadRole::Dismantler,
                    body_type: BodyType::Dismantler,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Box2x2,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 2 drain creeps, strict formation so they stay adjacent and can heal each other while tanking.
    pub fn duo_drain() -> Self {
        SquadComposition {
            label: "Duo Drain".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::Tank,
                    body_type: BodyType::Drain,
                },
                SquadSlot {
                    role: SquadRole::Tank,
                    body_type: BodyType::Drain,
                },
            ],
            formation_shape: FormationShape::Line,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 1 cheap ranged, no formation.
    pub fn solo_harasser() -> Self {
        SquadComposition {
            label: "Solo Harasser".into(),
            slots: vec![SquadSlot {
                role: SquadRole::RangedDPS,
                body_type: BodyType::Harasser,
            }],
            formation_shape: FormationShape::None,
            formation_mode: FormationMode::Loose,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// 1 melee attacker + 1 healer, line formation (invader core / power bank).
    pub fn duo_melee_heal() -> Self {
        SquadComposition {
            label: "Duo Melee+Heal".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::MeleeDPS,
                    body_type: BodyType::DuoMeleeAttacker,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Line,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// Source Keeper farming duo: 1 ranged kiter + 1 healer, line formation.
    /// The ranged attacker kites at range 3 while the healer keeps it alive.
    /// Higher retreat threshold (0.5) since SK damage is bursty.
    pub fn duo_sk_farmer() -> Self {
        SquadComposition {
            label: "SK Farmer Duo".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::RangedDPS,
                    body_type: BodyType::SkRangedAttacker,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::SkHealer,
                },
            ],
            formation_shape: FormationShape::Line,
            formation_mode: FormationMode::Strict,
            retreat_threshold: 0.5,
        }
    }

    /// Power bank farming duo: heavy melee attacker + heavy healer.
    /// The attacker hits the bank while the healer outheals damage reflection.
    pub fn power_bank_duo() -> Self {
        SquadComposition {
            label: "Power Bank Duo".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::MeleeDPS,
                    body_type: BodyType::PowerBankAttacker,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::PowerBankHealer,
                },
            ],
            formation_shape: FormationShape::Line,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// Power bank hauler squad: multiple haulers to collect dropped power.
    /// Deployed after the bank is destroyed.
    pub fn power_bank_haulers(count: usize) -> Self {
        SquadComposition {
            label: format!("Power Bank Haulers x{}", count),
            slots: (0..count)
                .map(|_| SquadSlot {
                    role: SquadRole::Hauler,
                    body_type: BodyType::Hauler,
                })
                .collect(),
            formation_shape: FormationShape::None,
            formation_mode: FormationMode::Loose,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// Siege quad: 2 dismantlers + 2 healers in box formation.
    /// Uses heavy siege dismantler bodies for maximum wall/rampart damage.
    pub fn siege_quad() -> Self {
        SquadComposition {
            label: "Siege Quad".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::Dismantler,
                    body_type: BodyType::SiegeDismantler,
                },
                SquadSlot {
                    role: SquadRole::Dismantler,
                    body_type: BodyType::SiegeDismantler,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Box2x2,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// Siege ASSAULT quad (ADR 0031 P1b): a dismantler to raze the structure + a RANGED attacker to clear
    /// the blocking defenders + 2 healers, box formation. The fusion template `SiegeBreach` fields when
    /// defenders are observed — it has a `RangedDPS` slot so `sized_for` keeps the `anti_creep_parts`
    /// (`siege_quad`, dismantler-only, drops them — Layer B). Throwaway: the assembler (P3) derives this mix.
    pub fn siege_assault_quad() -> Self {
        SquadComposition {
            label: "Siege Assault Quad".into(),
            slots: vec![
                SquadSlot {
                    role: SquadRole::Dismantler,
                    body_type: BodyType::SiegeDismantler,
                },
                SquadSlot {
                    role: SquadRole::RangedDPS,
                    body_type: BodyType::QuadMember,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
                SquadSlot {
                    role: SquadRole::Healer,
                    body_type: BodyType::DuoHealer,
                },
            ],
            formation_shape: FormationShape::Box2x2,
            formation_mode: FormationMode::Strict,
            retreat_threshold: default_retreat_threshold(),
        }
    }

    /// Cheap solo attacker for level 0 invader cores.
    pub fn solo_core_attacker() -> Self {
        SquadComposition {
            label: "Solo Core Attacker".into(),
            slots: vec![SquadSlot {
                role: SquadRole::MeleeDPS,
                body_type: BodyType::CoreAttacker,
            }],
            formation_shape: FormationShape::None,
            formation_mode: FormationMode::Loose,
            retreat_threshold: default_retreat_threshold(),
        }
    }

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
        // CEILING of RANGED_ATTACK parts one member can field at this energy — what a `sized_for` ranged
        // member reaches. The winnability budget must use THIS for ranged-attacker roles, not the balanced
        // template `QuadMember`'s few ranged parts, or the oracle defers every dismantle-immune core/keeper
        // as "kill too slow" even though a ranged-sized squad clears it (R-attack §12.6 step 4). Probed via
        // the real builder (incl. the MOVE ratio + 50-cap) so the ceiling can't drift from what spawns.
        let max_ranged = (1..=MAX_SINGLE_ROLE_PARTS)
            .rev()
            .find(|&n| bodies::build_combat_body(&bodies::CombatBodySpec { ranged_attack: n, ..Default::default() }, bodies::MoveProfile::Plains, max_energy).is_some())
            .unwrap_or(0);
        for slot in &self.slots {
            let bt = slot.body_type;
            heal_per_tick += bt.part_count(max_energy, Part::Heal) * HEAL_POWER;
            // Structure damage: WORK dismantles (50/part), ATTACK (30/part), RANGED_ATTACK (10/part). All
            // breach ramparts + kill the core. For a ranged-attacker role use the ranged CEILING (above)
            // since `sized_for` builds it ranged-maximized; other roles use their template's ranged count.
            let ranged_parts = if matches!(slot.role, SquadRole::RangedDPS) { max_ranged } else { bt.part_count(max_energy, Part::RangedAttack) };
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

    /// Force-DRIVEN sizing (R3 + D3 member-count scaling, ADR 0020 §12.6 / ADR 0022 D3): return a copy
    /// of this composition sized to deliver `force`. Each role covered by `force` (Healer→HEAL,
    /// Dismantler→WORK, Tank→TOUGH) is sized to its even share of the required parts; when one member
    /// can't carry that share (the 50-part cap or `max_member_energy`), the role's member COUNT is
    /// GROWN (`ceil(parts / per-member-cap)`, never below the template count) and the parts
    /// re-distributed evenly across the grown count — so an UNDER-strength squad is never fielded and
    /// the runtime engage gate holds instead of retreating (the direct fix for the P2b / SK-trickle
    /// engage-retreat bug; size to hold from one calc). Returns `None` only when a required role can't
    /// field even ONE member at this energy, or the squad would exceed [`MAX_SIZED_MEMBERS`] (→ defer
    /// to the multi-squad G4-HEAVY path, P5). Roles not in `force` keep their template body; per-member
    /// MOVE is applied by [`bodies::build_combat_body`]. (Full role re-allocation across a blob is
    /// R8/0020-S5.)
    pub fn sized_for(&self, force: RequiredForce, max_member_energy: u32) -> Option<SquadComposition> {
        // A single-role part SPEC (the only roles `force` covers: HEAL / WORK / TOUGH).
        let spec_for = |role: SquadRole, n: u32| -> bodies::CombatBodySpec {
            match role {
                SquadRole::Healer => bodies::CombatBodySpec { heal: n, ..Default::default() },
                SquadRole::Dismantler => bodies::CombatBodySpec { work: n, ..Default::default() },
                SquadRole::Tank => bodies::CombatBodySpec { tough: n, ..Default::default() },
                // RangedDPS sizes RANGED_ATTACK (R-attack §12.6): kill a dismantle-immune target (an
                // invader core) that WORK can't touch.
                SquadRole::RangedDPS => bodies::CombatBodySpec { ranged_attack: n, ..Default::default() },
                _ => bodies::CombatBodySpec::default(),
            }
        };
        // Largest single-role part count one member can carry at this energy — reuses the real builder
        // (incl. the per-member MOVE ratio + 50-part cap) so the cap can't drift from what actually
        // spawns. 0 ⇒ can't field even one member of this role at this energy.
        // Probe at the SMALLER of the home's capacity and the preferred per-member ceiling, so a force is
        // split into more, smaller, bankable members rather than one ~5000e blob the home can never bank.
        let probe_energy = max_member_energy.min(PREFERRED_MEMBER_ENERGY);
        let cap_for = |role: SquadRole| -> u32 {
            (1..=MAX_SINGLE_ROLE_PARTS)
                .rev()
                .find(|&n| bodies::build_combat_body(&spec_for(role, n), bodies::MoveProfile::Plains, probe_energy).is_some())
                .unwrap_or(0)
        };
        let template_count = |r: SquadRole| self.slots.iter().filter(|s| s.role == r).count() as u32;

        // Decide member count + per-member spec for each required role present in the template. `RangedDPS`
        // draws from `immune_struct_parts + anti_creep_parts` — BOTH are RANGED (anti-immune-structure AND
        // anti-creep, ADR 0031 Layer C), so a force facing a guarded core sizes its ranged members to do both;
        // WORK roles draw `dismantle_parts`. Roles the template lacks are skipped (`template_count == 0`
        // below) — that ceiling is what the assembler (P3) removes; until then a template must carry a
        // RangedDPS slot to receive these (the `siege_assault_quad` fusion).
        let roles: [(SquadRole, u32); 4] = [
            (SquadRole::Healer, force.heal_parts),
            (SquadRole::Dismantler, force.dismantle_parts),
            (SquadRole::Tank, force.tough_parts),
            (SquadRole::RangedDPS, force.immune_struct_parts + force.anti_creep_parts),
        ];
        let mut sized_roles: Vec<(SquadRole, u32, bodies::CombatBodySpec)> = Vec::new();
        for (role, total) in roles {
            if total == 0 || template_count(role) == 0 {
                continue; // role not required by this force, or no slot to size → keep template
            }
            let cap = cap_for(role);
            if cap == 0 {
                return None; // can't field even one member of this role at this energy → defer
            }
            // Grow the member count so each member's even share fits; never below the template count.
            // (per_member = div_ceil(total, count) ≤ cap always holds, since count ≥ div_ceil(total, cap).)
            let count = total.div_ceil(cap).max(template_count(role));
            let per_member = total.div_ceil(count); // ceil ⇒ Σ over members ≥ total (never under-sizes)
            sized_roles.push((role, count, spec_for(role, per_member)));
        }

        // Total members = kept (non-sized-role) slots + the grown sized-role counts; bound the blob to
        // one squad (a bigger force is the multi-squad G4-HEAVY path, P5).
        let sized_set: Vec<SquadRole> = sized_roles.iter().map(|(r, _, _)| *r).collect();
        let kept = self.slots.iter().filter(|s| !sized_set.contains(&s.role)).count();
        let grown: usize = sized_roles.iter().map(|(_, n, _)| *n as usize).sum();
        if kept + grown > MAX_SIZED_MEMBERS {
            return None;
        }

        // Rebuild: size each role's existing slots in place (order-preserving), append the grown extras
        // by cloning the role's template slot.
        let mut sized = self.clone();
        for (role, count, spec) in &sized_roles {
            let mut placed = 0u32;
            for slot in sized.slots.iter_mut() {
                if slot.role == *role && placed < *count {
                    slot.body_type = BodyType::Sized(*spec);
                    placed += 1;
                }
            }
            let template = self.slots.iter().find(|s| s.role == *role).expect("required role present (guarded above)");
            while placed < *count {
                let mut slot = template.clone();
                slot.body_type = BodyType::Sized(*spec);
                sized.slots.push(slot);
                placed += 1;
            }
        }
        Some(sized)
    }
}

/// A single-role part SPEC — the body of a member that carries only `n` of its role's weapon part (the
/// roles a [`RequiredForce`] covers: HEAL/WORK/RANGED/TOUGH; ATTACK/CARRY for exhaustiveness). Shared by
/// [`assemble_force`] (and the [`SquadComposition::sized_for`] bridge has an identical local copy until P4
/// deletes it). MOVE is added per-member by [`bodies::build_combat_body`].
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

/// Formation for an assembled force of `count` members — matches the catalog's solo/duo/quad shapes (the
/// only precedent), generalized: a lone member roams loose, a duo holds a strict line, ≥3 hold a strict box
/// (a grown quad already does this under `sized_for`, so this introduces no new movement behavior).
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
/// from its demand, and each member's body is force-`Sized` per pick via the real builder. Replaces
/// `template() + sized_for` (P3).
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

/// The template-free winnability CEILING (ADR 0031 P4) — the BUDGET source that replaces
/// `doctrine.template().force_budget(..)`: `force_ceiling(energy, fighter).force_budget(..)` is the oracle's
/// `ForceBudget` with NO catalog constructor in sight. `fighter` is the kill weapon role (`Dismantler` for
/// dismantle-able rings, `RangedDPS` for immune cores / creep clear). Each member is maxed at `member_energy`
/// via the real builder (full-energy probe — the conservative ceiling the oracle is calibrated against,
/// matching the eval's `siege_ceiling`). Identical in shape to `siege_ceiling(energy)` for `Dismantler`, so
/// the calibration gates that judge against the ceiling are preserved.
pub fn force_ceiling(member_energy: u32, fighter: SquadRole) -> SquadComposition {
    let fighter_cap = single_role_cap(fighter, member_energy);
    let heal_cap = single_role_cap(SquadRole::Healer, member_energy);
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

    // ── R3: SquadComposition::sized_for (force-driven sizing) ──
    #[test]
    fn sized_for_distributes_required_force_across_roles() {
        // siege_quad = 2 Dismantler + 2 Healer. 20 heal + 12 dismantle parts, even split, fits RCL7.
        let sized = SquadComposition::siege_quad()
            .sized_for(RequiredForce { heal_parts: 20, dismantle_parts: 12, immune_struct_parts: 0, anti_creep_parts: 0, tough_parts: 0 }, 5600)
            .expect("affordable at RCL7");
        let dismantler = sized.slots.iter().find(|s| s.role == SquadRole::Dismantler).unwrap();
        assert!(
            matches!(dismantler.body_type, BodyType::Sized(spec) if spec.work == 6 && spec.heal == 0),
            "dismantler sized to 12/2 = 6 WORK"
        );
        let healer = sized.slots.iter().find(|s| s.role == SquadRole::Healer).unwrap();
        assert!(
            matches!(healer.body_type, BodyType::Sized(spec) if spec.heal == 10 && spec.work == 0),
            "healer sized to 20/2 = 10 HEAL"
        );
    }

    #[test]
    fn sized_for_defers_when_force_exceeds_one_squad() {
        // 200 heal parts at RCL4 (1300e ⇒ ≤4 HEAL/member) would need ~50 healer members — far past
        // MAX_SIZED_MEMBERS, so it defers to the multi-squad G4-HEAVY path rather than under-size.
        assert!(SquadComposition::siege_quad()
            .sized_for(RequiredForce { heal_parts: 200, dismantle_parts: 0, immune_struct_parts: 0, anti_creep_parts: 0, tough_parts: 0 }, 1300)
            .is_none());
    }

    #[test]
    fn sized_for_grows_member_count_when_template_count_is_insufficient() {
        // D3 (ADR 0022): a force needing more HEAL than the template's 2 healers can carry GROWS the
        // healer count instead of deferring. 40 heal parts at RCL7, capped to ≤10 HEAL/member by the
        // per-member energy ceiling ⇒ ceil(40/10)=4 healers, each 10 HEAL — the squad is fielded (not
        // deferred) and out-heals the requirement.
        let sized = SquadComposition::siege_quad()
            .sized_for(RequiredForce { heal_parts: 40, dismantle_parts: 12, immune_struct_parts: 0, anti_creep_parts: 0, tough_parts: 0 }, 5600)
            .expect("grows healers to meet the force at RCL7");
        let healers = sized.slots.iter().filter(|s| s.role == SquadRole::Healer).count();
        assert_eq!(healers, 4, "the 2-healer template grew to 4 to carry 40 HEAL parts");
        // The fielded force meets-or-exceeds the requirement (ceil distribution never under-sizes).
        assert!(sized.capabilities(5600).heal_per_tick >= 40 * 12, "fielded HEAL ≥ required (12 HEAL/part)");
        // Dismantlers stay at the template count (12 WORK fits 2 dismantlers at RCL7).
        assert_eq!(sized.slots.iter().filter(|s| s.role == SquadRole::Dismantler).count(), 2);
    }

    /// SK-setup scenario across keeper strengths (operator ask): an open SK-like room (keepers doing
    /// `keeper_dps`, no towers/walls). Run the end-to-end pipeline (assess → required force → size the
    /// squad). The composition we actually FIELD must out-heal the keepers WITH the hold margin — so it
    /// maintains/holds through damage and won't early-retreat — and when no single squad at this RCL can
    /// (the heal need exceeds the budget OR a member's 50-part cap), it must DEFER, never field an
    /// undersized squad that bails. Regression guard for the live SK failure that started P2b.
    #[test]
    fn sk_setup_fields_a_holding_composition_or_defers_never_undersizes() {
        use crate::force_sizing::{assess, DefenseProfile, ForceBudget, HOLD_MARGIN};

        // A strong RCL7-ish home's baseline budget (2 healers cap squad heal at ~600/tick, so very
        // strong keeper sets correctly exceed what one siege quad can field).
        let budget = ForceBudget {
            max_heal_per_tick: 900.0,
            max_dismantle_dps: 300.0,
            tank_effective_hp: 30_000.0,
            onsite_budget_ticks: 1400,
        };
        // The ACTUAL field decision is sized_for(): aggregate-winnable is necessary but not sufficient —
        // a member's 50-part AND per-member energy cost cap can still force a defer.
        let field = |keeper_dps: f32| -> Option<SquadComposition> {
            let a = assess(&DefenseProfile { enemy_dps: keeper_dps, ..Default::default() }, &budget);
            if a.winnable {
                SquadComposition::siege_quad().sized_for(RequiredForce::from_assessment(&a), 5600)
            } else {
                None
            }
        };

        // INVARIANT across keeper strengths: whatever we FIELD out-heals the keepers WITH the hold
        // margin → it maintains/holds through damage instead of early-retreating.
        for &keeper_dps in &[60.0f32, 180.0, 360.0, 600.0, 2000.0] {
            if let Some(comp) = field(keeper_dps) {
                assert!(
                    comp.capabilities(5600).heal_per_tick as f32 >= keeper_dps * HOLD_MARGIN,
                    "keeper dps {keeper_dps}: fielded composition must out-heal with the hold margin"
                );
            }
        }
        // Endpoints: a weak SK room IS fielded (the bot engages it); an overwhelming keeper set DEFERS
        // (no single siege quad can out-heal it at RCL7) rather than fielding an undersized squad.
        assert!(field(60.0).is_some(), "a weak SK room is fieldable");
        assert!(field(2000.0).is_none(), "an overwhelming keeper set defers, not an undersized squad");
    }

    /// R6: the SK suppression duo force-sizes its HEALER to out-heal a Source Keeper (168 melee DPS ×
    /// the hold margin) at a high-energy home, and defers (→ template fallback at the call site) when
    /// no home affords it. The ranged kiter always stays the proven template.
    #[test]
    fn sk_duo_sizes_healer_to_outheal_a_keeper() {
        use crate::force_sizing::HOLD_MARGIN;
        let required = RequiredForce {
            heal_parts: bodies::defender_heal_parts_for_dps(168.0 * HOLD_MARGIN, false),
            ..Default::default()
        };
        // RCL8 energy: the healer sizes up to out-heal the keeper with margin; the ranged kiter stays template.
        let sized = SquadComposition::duo_sk_farmer()
            .sized_for(required, 12_900)
            .expect("RCL8 affords the sized SK healer");
        // The healer role is force-sized (not the bare template) and — under the per-member energy cap —
        // SPLIT across multiple bankable members, so the squad AGGREGATE out-heals the keeper with margin.
        assert!(
            sized.slots.iter().any(|s| matches!(s.body_type, BodyType::Sized(spec) if spec.heal > 0)),
            "the SK healer role is force-sized, not the bare template"
        );
        assert!(
            sized.capabilities(12_900).heal_per_tick as f32 >= 168.0 * HOLD_MARGIN,
            "the sized SK healers aggregate-out-heal a keeper with the hold margin"
        );
        let ranged = sized.slots.iter().find(|s| s.role == SquadRole::RangedDPS).unwrap();
        assert_eq!(ranged.body_type, BodyType::SkRangedAttacker, "the ranged kiter stays the proven template");
        // Very low energy (RCL2, ~1 HEAL/member) → the keeper-holding heal needs more members than one
        // squad can field → defer (the mission falls back to the template duo). (At RCL4+ D3 instead
        // GROWS the healer count rather than deferring — see sized_for_grows_member_count_*.)
        assert!(SquadComposition::duo_sk_farmer().sized_for(required, 550).is_none(), "RCL2 defers (force > one squad)");
    }

    /// The oracle's structure-DPS must count RANGED_ATTACK: invader cores are dismantle-immune, so a
    /// ranged comp is what kills them. Without this the force oracle reads `quad_ranged` as 0
    /// structure-DPS and defers every core as "breach too slow" (the soak regression).
    #[test]
    fn quad_ranged_deals_structure_damage_via_ranged() {
        let caps = SquadComposition::quad_ranged().capabilities(5600);
        assert!(
            caps.structure_dps > 0,
            "quad_ranged must contribute structure damage through RANGED_ATTACK (got {})",
            caps.structure_dps
        );
    }

    /// R-attack §12.6 — a dismantle-immune core (100k hits, no ramparts/towers) is WINNABLE because the
    /// `quad_ranged` budget uses the RANGED CEILING (`capabilities` step 4): `kill_ticks = 100k / ranged
    /// DPS` fits a creep lifetime. The balanced template's few ranged parts made the oracle defer every
    /// core — the soak-confirmed regression this closes.
    #[test]
    fn r_attack_makes_a_dismantle_immune_core_winnable() {
        use crate::force_sizing::{assess, DefenseProfile, ForceBudget, RequiredForce};
        let caps = SquadComposition::quad_ranged().capabilities(5600);
        let budget = ForceBudget {
            max_heal_per_tick: caps.heal_per_tick as f32,
            max_dismantle_dps: caps.structure_dps as f32,
            tank_effective_hp: caps.tank_effective_hp as f32,
            onsite_budget_ticks: 1400, // ~a creep lifetime minus spawn/travel
        };
        let core = DefenseProfile { objective_hits: 100_000, ..Default::default() };
        let a = assess(&core, &budget);
        assert!(a.winnable, "a no-rampart 100k core is winnable once ranged is sized to the ceiling: {}", a.reason);
        assert!(RequiredForce::from_assessment(&a).immune_struct_parts > 0, "the winning force fields RANGED kill parts");
    }

    /// R-attack — a ranged comp sizes its `RangedDPS` members from `force.immune_struct_parts` (RANGED, not WORK).
    #[test]
    fn sized_for_sizes_ranged_attackers_from_ranged_parts() {
        use crate::force_sizing::RequiredForce;
        let force = RequiredForce { immune_struct_parts: 18, ..Default::default() };
        let sized = SquadComposition::quad_ranged().sized_for(force, 5600).expect("affordable at RCL7");
        let ranged = sized.slots.iter().find(|s| s.role == SquadRole::RangedDPS).expect("has RangedDPS");
        match ranged.body_type {
            BodyType::Sized(spec) => assert!(spec.ranged_attack > 0 && spec.work == 0, "sized to RANGED, not WORK: {spec:?}"),
            bt => panic!("RangedDPS not sized: {bt:?}"),
        }
    }

    /// R-attack for the SK duo (operator 2026-06-26): with `ranged_parts` set (kill the keeper), the SK
    /// kiter is SIZED to ranged — not left on the template that caps too low to kill at a low-energy home.
    /// (Heal-only force keeps it template — see `sk_duo_sizes_healer_to_outheal_a_keeper`.)
    #[test]
    fn sk_duo_sizes_kiter_to_kill_the_keeper() {
        use crate::force_sizing::RequiredForce;
        let force = RequiredForce { heal_parts: 10, immune_struct_parts: 15, ..Default::default() };
        let sized = SquadComposition::duo_sk_farmer().sized_for(force, 5600).expect("RCL7 affords it");
        let kiter = sized.slots.iter().find(|s| s.role == SquadRole::RangedDPS).expect("has RangedDPS");
        match kiter.body_type {
            BodyType::Sized(spec) => assert!(spec.ranged_attack > 0, "kiter sized to ranged kill parts: {spec:?}"),
            bt => panic!("kiter not sized: {bt:?}"),
        }
    }

    // ── T2: assemble_force (ADR 0031 P3 — the marginal-fill assembler) ──

    fn member_count_of(req: RequiredForce, energy: u32) -> Option<usize> {
        assemble_force(&req, energy).map(|c| c.slots.len())
    }

    /// The assembler is a pure fold over a frozen Vec-ordered demand list — run-twice-equal (the P3
    /// determinism fence; the standing sim fence only covers the hardcoded quad_ranged). (ADR 0031 §5.)
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
    /// template count — so a tiny dismantle+heal force is a DUO (2), not snapped to a quad (4), the member
    /// count is MONOTONIC non-decreasing in the force, and 3 is reachable (no 2→4 snap). Contrast
    /// `sized_for`'s `.max(template_count)` (composition.rs ~804), which floored the count at the template's.
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
    /// guard fields enough ranged for both. (The sum, matching the `sized_for` bridge.)
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
