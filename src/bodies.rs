//! Pure combat-body construction + sizing math (the force-sizing primitives). Lives here so the bot,
//! the sim, and the eval all build/size the SAME bodies with one implementation (no fork) — pure over
//! `screeps-game-api` value types + the engine's mechanics constants, no `game::*`.
//!
//! Force-sized bodies: `CombatBodySpec` (the output of the `force_sizing` solver) → `build_combat_body`
//! turns it into an ordered `Vec<Part>`; the part-sizing helpers (`attack_parts_to_kill`,
//! `defender_heal_parts_for_dps`, `drain_heal_parts_for_dps`) translate a threat picture into part
//! counts; `assemble_combat_body`/`sized_defender_body`/`sized_healer_body` build threat-matched
//! defender bodies, and `boosts` is the compound table. (The static template catalog was deleted in
//! ADR 0031 Phase 4b — every fielded body is now force-`Sized`.)

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

// ── Part-sizing helpers (threat picture → part counts) ──────────────────────

/// HEAL part output per tick when adjacent (used for sizing drain bodies).
pub const HEAL_PER_PART_ADJACENT: f32 = 12.0;

/// Minimum HEAL parts needed to sustain a given tower DPS (adjacent self-heal).
/// Used to pick drain body size from room tower damage.
pub fn drain_heal_parts_for_dps(dps: f32) -> u32 {
    if dps <= 0.0 {
        return 1;
    }
    (dps / HEAL_PER_PART_ADJACENT).ceil().max(1.0) as u32
}

/// Ticks within which a single defender should kill its worst target. Sizes the
/// offense floor: enough damage/tick to clear the target's effective HP AND
/// out-pace the heal the enemy can focus on it within this window.
pub const KILL_WINDOW_TICKS: u32 = 25;

/// Cap on offense parts a SINGLE defender is sized to. Beyond this the model
/// escalates squad COUNT (Duo/Quad — multiple defenders focus-fire) rather than
/// building an ever-larger solo that still can't out-damage the enemy heal.
pub const MAX_OFFENSE_PARTS: u32 = 25;

/// Offense parts for ONE defender to kill a hostile of `target_hp` effective HP
/// within `window_ticks`, net of `enemy_focus_heal` — the AGGREGATE enemy HEAL
/// output, because hostiles heal each other and concentrate all healers on the
/// creep under fire. So a defender must out-damage the whole enemy heal stack,
/// not just the target's self-heal.
///
/// `dmg_per_part` = 10 (RANGED_ATTACK) or 30 (ATTACK), ×4 if our creep is
/// boosted. Returns `None` when the kill needs more than [`MAX_OFFENSE_PARTS`] —
/// the caller then escalates squad COUNT so multiple defenders stack DPS and
/// focus-fire one target (the existing Solo→Duo→Quad path).
pub fn attack_parts_to_kill(target_hp: f32, enemy_focus_heal: f32, window_ticks: u32, dmg_per_part: f32) -> Option<u32> {
    if window_ticks == 0 || dmg_per_part <= 0.0 {
        return None;
    }
    // Total damage to land = the target's effective HP plus all the heal it
    // soaks over the window.
    let total = target_hp.max(0.0) + enemy_focus_heal.max(0.0) * window_ticks as f32;
    let dps_needed = total / window_ticks as f32;
    let parts = (dps_needed / dmg_per_part).ceil().max(1.0) as u32;
    if parts <= MAX_OFFENSE_PARTS {
        Some(parts)
    } else {
        None
    }
}


// ── Threat-matched sized defender bodies ────────────────────────────────────
//
// Unlike the repeat-template bodies (which `create_body` expands to fit
// `energy_capacity`), these build the FINAL `Vec<Part>` directly from the
// threat picture — so the part counts are exact and there is no `&'static`
// slice constraint. The spawn path passes the result straight to
// `SpawnRequest::new(.., &body, ..)`.

/// Assemble the final body for a defender/healer from desired offense + HEAL
/// counts within an energy `budget`. Adds MOVE (~1 per 2 other parts) and a
/// small TOUGH front when HEAL is present and it fits. Degrades to fit the
/// budget and the 50-part cap in priority order — drop TOUGH, then HEAL, then
/// trim offense — but never below the role floor (at least 1 offense for an
/// attacker, at least 1 HEAL for a pure healer), so it always returns a usable
/// body once the room can afford it. Parts are ordered TOUGH, offense, HEAL,
/// MOVE so TOUGH soaks damage first.
fn assemble_combat_body(budget: u32, offense_parts: u32, offense_kind: Part, heal_parts: u32) -> Vec<Part> {
    let off_floor: u32 = if offense_parts > 0 { 1 } else { 0 };
    let heal_floor: u32 = if offense_parts == 0 { 1 } else { 0 };

    let mut off = offense_parts.max(off_floor).min(MAX_CREEP_SIZE);
    let mut heal = heal_parts.max(heal_floor).min(MAX_CREEP_SIZE);
    let mut tough: u32 = if heal > 0 { 2 } else { 0 };

    let cfg = |off: u32, heal: u32, tough: u32| -> (u32, u32) {
        let work = off + heal + tough;
        let moves = work.div_ceil(2).max(1); // ~1 MOVE per 2 other parts, at least 1
        let parts = work + moves;
        let cost = off * offense_kind.cost() + heal * Part::Heal.cost() + tough * Part::Tough.cost() + moves * Part::Move.cost();
        (cost, parts)
    };

    loop {
        let (cost, parts) = cfg(off, heal, tough);
        if cost <= budget && parts <= MAX_CREEP_SIZE {
            break;
        }
        if tough > 0 {
            tough -= 1;
        } else if heal > heal_floor {
            heal -= 1;
        } else if off > off_floor {
            off -= 1;
        } else {
            // At the role floor and still over budget: emit the floor body. The
            // spawn queue won't fire it until the room can afford it (body_cost
            // > available ⇒ the request waits), so this never panics or returns
            // an empty body.
            break;
        }
    }

    let moves = (off + heal + tough).div_ceil(2).max(1);
    let mut body = Vec::with_capacity((off + heal + tough + moves) as usize);
    body.extend(std::iter::repeat_n(Part::Tough, tough as usize));
    body.extend(std::iter::repeat_n(offense_kind, off as usize));
    body.extend(std::iter::repeat_n(Part::Heal, heal as usize));
    body.extend(std::iter::repeat_n(Part::Move, moves as usize));
    body
}

/// Threat-matched defender body sized to an energy `budget`. Offense
/// (RANGED_ATTACK) is sized to kill the worst target within
/// [`KILL_WINDOW_TICKS`] net of the enemy's focused heal; HEAL is sized
/// to survive `incoming_dps` (0 against a zero-DPS threat such as a CLAIM creep)
/// and included only when it fits. Always returns at least `[RangedAttack, Move]`
/// (200e) so a bare RCL2 towerless room still gets an armed defender. `boosted` =
/// whether OUR creep is boosted (the enemy's boosts are already folded into the
/// threat figures by `threatmap`).
pub fn sized_defender_body(budget: u32, incoming_dps: f32, target_hp: f32, enemy_focus_heal: f32, boosted: bool) -> Vec<Part> {
    let ra_dmg = if boosted { 10.0 * 4.0 } else { 10.0 };
    let want_off = attack_parts_to_kill(target_hp, enemy_focus_heal, KILL_WINDOW_TICKS, ra_dmg)
        .unwrap_or(MAX_OFFENSE_PARTS)
        .max(1);
    let want_heal = defender_heal_parts_for_dps(incoming_dps, boosted);
    assemble_combat_body(budget, want_off, Part::RangedAttack, want_heal)
}

/// Threat-matched defender HEALER body (HEAL + MOVE, TOUGH front when it fits)
/// sized to sustain `incoming_dps`. Spawns even at RCL2 by dropping the TOUGH
/// front — fixing the old `duo_healer_body` 660e floor that produced no healer
/// below RCL3.
pub fn sized_healer_body(budget: u32, incoming_dps: f32, boosted: bool) -> Vec<Part> {
    let want_heal = defender_heal_parts_for_dps(incoming_dps, boosted).max(1);
    assemble_combat_body(budget, 0, Part::RangedAttack, want_heal)
}

/// Standard military boost compounds (T3).
pub mod boosts {
    use screeps::ResourceType;

    /// TOUGH damage reduction (70%) -- XGHO2.
    pub const TOUGH_BOOST: ResourceType = ResourceType::CatalyzedGhodiumAlkalide;
    /// HEAL effectiveness (300%) -- XLHO2.
    pub const HEAL_BOOST: ResourceType = ResourceType::CatalyzedLemergiumAlkalide;
    /// MOVE fatigue reduction (100%) -- XZHO2.
    pub const MOVE_BOOST: ResourceType = ResourceType::CatalyzedZynthiumAlkalide;
    /// RANGED_ATTACK damage (300%) -- XKHO2.
    pub const RANGED_ATTACK_BOOST: ResourceType = ResourceType::CatalyzedKeaniumAlkalide;
    /// ATTACK damage (300%) -- XUH2O.
    pub const ATTACK_BOOST: ResourceType = ResourceType::CatalyzedUtriumAcid;
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

    #[test]
    fn attack_parts_basic_and_focus_heal() {
        // 600 HP, no heal, 25-tick window, 10 dmg/part: 600/25=24 dps ⇒ 3 RA.
        assert_eq!(attack_parts_to_kill(600.0, 0.0, 25, 10.0), Some(3));
        // Focused enemy heal raises the requirement.
        let with_heal = attack_parts_to_kill(600.0, 120.0, 25, 10.0).unwrap();
        assert!(with_heal > 3, "focus heal must raise parts: {with_heal}");
        // Beyond MAX_OFFENSE_PARTS for one defender ⇒ None ⇒ caller escalates count.
        assert_eq!(attack_parts_to_kill(600.0, 480.0, 25, 10.0), None);
        // Boosted ranged (×4 ⇒ 40/part) needs fewer parts.
        assert_eq!(attack_parts_to_kill(600.0, 0.0, 25, 40.0), Some(1));
    }

    // ── Threat-matched sized bodies ─────────────────────────────────────────

    fn cost(body: &[Part]) -> u32 {
        body.iter().map(|p| p.cost()).sum()
    }

    /// Bare RCL2 vs an unarmed CLAIM creep (zero DPS): armed defender, NO HEAL
    /// forced, fits the budget. Preserves the live W11N57 fix.
    #[test]
    fn sized_defender_rcl2_vs_claim_is_armed_with_no_heal() {
        let body = sized_defender_body(300, 0.0, 700.0, 0.0, false);
        assert!(!body.is_empty());
        assert!(body.iter().any(|&p| p == Part::RangedAttack), "must be armed");
        assert!(body.iter().any(|&p| p == Part::Move), "must move");
        assert!(!body.iter().any(|&p| p == Part::Heal), "no HEAL vs a zero-DPS threat");
        assert!(cost(&body) <= 300, "cost {} > 300", cost(&body));
    }

    /// HEAL is dropped (not forced) when it doesn't fit a tight budget, but the
    /// defender is still armed.
    #[test]
    fn sized_defender_drops_heal_when_unaffordable() {
        let body = sized_defender_body(400, 90.0, 600.0, 0.0, false);
        assert!(body.iter().any(|&p| p == Part::RangedAttack));
        assert!(cost(&body) <= 400, "cost {} > 400", cost(&body));
    }

    /// A capable budget vs a real attacker ⇒ defender carries HEAL.
    #[test]
    fn sized_defender_carries_heal_when_affordable() {
        let body = sized_defender_body(2000, 120.0, 1000.0, 0.0, false);
        assert!(body.iter().any(|&p| p == Part::Heal), "should carry HEAL when affordable");
        assert!(body.iter().any(|&p| p == Part::RangedAttack));
        assert!(cost(&body) <= 2000);
    }

    /// Regression for the duo_healer 660e floor: a healer MUST build at RCL2
    /// (drops the TOUGH front).
    #[test]
    fn sized_healer_builds_at_rcl2() {
        let body = sized_healer_body(550, 90.0, false);
        assert!(!body.is_empty());
        assert!(body.iter().any(|&p| p == Part::Heal));
        assert!(body.iter().any(|&p| p == Part::Move));
        assert!(cost(&body) <= 550, "cost {} > 550", cost(&body));
    }

    /// Never exceed the 50-part engine cap, however large the budget/threat.
    #[test]
    fn sized_bodies_respect_part_cap() {
        let d = sized_defender_body(50_000, 5000.0, 1_000_000.0, 5000.0, false);
        assert!(d.len() <= 50, "defender len {}", d.len());
        let h = sized_healer_body(50_000, 5000.0, false);
        assert!(h.len() <= 50, "healer len {}", h.len());
    }
}
