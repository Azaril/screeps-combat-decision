//! Pure combat-body construction + sizing math (the force-sizing primitives). Lives here so the bot,
//! the sim, and the eval all build/size the SAME bodies with one implementation (no fork) — pure over
//! `screeps-game-api` value types + the engine's mechanics constants, no `game::*`.
//!
//! `CombatBodySpec` is the output of the force-sizing solver (`force_sizing`); `build_combat_body`
//! turns it into an ordered `Vec<Part>`. The static repeat-template bodies + their `BodyType` selector
//! live in `composition` (they reference these primitives).

use screeps::{Part, MAX_CREEP_SIZE};
use screeps_combat_engine::constants::HEAL_POWER;

/// Target part counts for a combat creep, BEFORE MOVE (which is derived from [`MoveProfile`]). The
/// output of the force-sizing solver; the input to [`build_combat_body`]. Serializable because the
/// sized body rides in a `BodyType::Sized` slot on the (persisted) squad composition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CombatBodySpec {
    pub tough: u32,
    pub attack: u32,
    pub ranged_attack: u32,
    pub work: u32,
    pub carry: u32,
    pub heal: u32,
}

impl CombatBodySpec {
    /// Fatigue-generating parts (everything except MOVE; CARRY counted conservatively as it may be
    /// laden in transit) — the input to the MOVE-ratio calc.
    pub fn non_move_parts(&self) -> u32 {
        self.tough + self.attack + self.ranged_attack + self.work + self.carry + self.heal
    }
}

/// MOVE provisioning for the intended travel terrain. Screeps fatigue: each non-MOVE (non-empty-CARRY)
/// part adds `terrain` fatigue per tile (1 road / 2 plain / 10 swamp); each MOVE removes 2 — so the
/// MOVE:non-MOVE ratio for 1 tile/tick is 1:2 (road), 1:1 (plain), 5:1 (swamp). Combat squads travel +
/// fight off-road, so `Plains` (full plain speed) is the combat default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveProfile {
    Plains,
    Road,
    Swamp,
}

impl MoveProfile {
    /// MOVE parts to move 1 tile/tick over this terrain with `non_move` fatigue-generating parts.
    pub fn move_parts(&self, non_move: u32) -> u32 {
        if non_move == 0 {
            return 0;
        }
        match self {
            MoveProfile::Plains => non_move,           // 1:1
            MoveProfile::Road => non_move.div_ceil(2), // 1:2
            MoveProfile::Swamp => non_move * 5,        // 5:1
        }
        .max(1)
    }
}

/// Build an ordered creep body from a part `spec` + MOVE for `move_profile`, or `None` if it can't fit
/// the 50-part cap or `max_energy` (the force-sizing primitive). The caller (the solver) chooses a spec
/// that fits — `None` is its "can't afford the required force ⇒ defer" signal, NOT a scale-to-fit.
/// Order: TOUGH front (the meat shield — parts are destroyed front-to-back, so TOUGH absorbs first),
/// then a round-robin of the remaining parts + MOVE so every capability (incl. mobility) degrades
/// gracefully instead of dropping all at once.
pub fn build_combat_body(spec: &CombatBodySpec, move_profile: MoveProfile, max_energy: u32) -> Option<Vec<Part>> {
    let moves = move_profile.move_parts(spec.non_move_parts());
    let total = spec.non_move_parts() + moves;
    if total == 0 || total > MAX_CREEP_SIZE {
        return None;
    }
    let cost = spec.tough * Part::Tough.cost()
        + spec.attack * Part::Attack.cost()
        + spec.ranged_attack * Part::RangedAttack.cost()
        + spec.work * Part::Work.cost()
        + spec.carry * Part::Carry.cost()
        + spec.heal * Part::Heal.cost()
        + moves * Part::Move.cost();
    if cost > max_energy {
        return None;
    }

    let mut body = Vec::with_capacity(total as usize);
    body.extend(std::iter::repeat_n(Part::Tough, spec.tough as usize));
    // Round-robin the remaining buckets (incl. MOVE) so capabilities degrade evenly behind the TOUGH.
    let mut buckets: [(Part, u32); 6] = [
        (Part::Move, moves),
        (Part::RangedAttack, spec.ranged_attack),
        (Part::Attack, spec.attack),
        (Part::Work, spec.work),
        (Part::Carry, spec.carry),
        (Part::Heal, spec.heal),
    ];
    let mut remaining: u32 = buckets.iter().map(|(_, n)| *n).sum();
    while remaining > 0 {
        for (part, n) in buckets.iter_mut() {
            if *n > 0 {
                body.push(*part);
                *n -= 1;
                remaining -= 1;
            }
        }
    }
    Some(body)
}

/// HEAL parts needed to sustain `incoming_dps` of damage (adjacent self/ally heal). Returns 0 at zero
/// DPS so a creep facing an unarmed threat is never given a wasted HEAL part. `boosted` ⇒ 48 HP/part
/// (T3 XLHO2, ×4). The inverse of the heal capability the force-sizing oracle needs.
pub fn defender_heal_parts_for_dps(incoming_dps: f32, boosted: bool) -> u32 {
    if incoming_dps <= 0.0 {
        return 0;
    }
    let per = if boosted { HEAL_POWER as f32 * 4.0 } else { HEAL_POWER as f32 };
    (incoming_dps / per).ceil().max(1.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(body: &[Part], part: Part) -> u32 {
        body.iter().filter(|&&p| p == part).count() as u32
    }

    #[test]
    fn move_profile_ratios() {
        assert_eq!(MoveProfile::Plains.move_parts(10), 10, "plains = 1:1");
        assert_eq!(MoveProfile::Road.move_parts(10), 5, "road = 1:2");
        assert_eq!(MoveProfile::Swamp.move_parts(2), 10, "swamp = 5:1");
        assert_eq!(MoveProfile::Plains.move_parts(0), 0, "no parts ⇒ no move");
    }

    #[test]
    fn build_combat_body_matches_spec_and_fronts_tough() {
        // A siege-ish duo member: 2 TOUGH + 6 WORK + 4 HEAL, plains move (1:1 → 12 move).
        let spec = CombatBodySpec { tough: 2, work: 6, heal: 4, ..Default::default() };
        let body = build_combat_body(&spec, MoveProfile::Plains, 5600).expect("fits");
        assert_eq!(count(&body, Part::Tough), 2);
        assert_eq!(count(&body, Part::Work), 6);
        assert_eq!(count(&body, Part::Heal), 4);
        assert_eq!(count(&body, Part::Move), 12, "1:1 move for 12 non-move parts");
        assert_eq!(body.len(), 24);
        assert!(body[0] == Part::Tough && body[1] == Part::Tough, "TOUGH is the front meat-shield");
    }

    #[test]
    fn build_combat_body_rejects_over_50_parts() {
        // 30 non-move parts × plains 1:1 = 60 parts > 50 → None (the solver must size smaller).
        let spec = CombatBodySpec { ranged_attack: 30, ..Default::default() };
        assert_eq!(build_combat_body(&spec, MoveProfile::Plains, 1_000_000), None);
    }

    #[test]
    fn build_combat_body_rejects_over_budget() {
        // 10 HEAL (2500) + 10 MOVE (500) = 3000 > a 1300 (RCL4) budget → None ("can't afford" signal).
        let spec = CombatBodySpec { heal: 10, ..Default::default() };
        assert_eq!(build_combat_body(&spec, MoveProfile::Plains, 1300), None);
        // …but affordable at RCL7 (5600).
        assert!(build_combat_body(&spec, MoveProfile::Plains, 5600).is_some());
    }

    #[test]
    fn defender_heal_parts_round_up_and_zero_at_zero() {
        assert_eq!(defender_heal_parts_for_dps(0.0, false), 0, "no DPS ⇒ no wasted HEAL");
        assert_eq!(defender_heal_parts_for_dps(120.0, false), 10, "120 ÷ 12 HEAL/part");
        assert_eq!(defender_heal_parts_for_dps(120.0, true), 3, "120 ÷ 48 boosted HEAL/part (ceil)");
    }
}
