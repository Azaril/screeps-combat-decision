//! Offense force-sizing oracle (ADR 0020 §12.2) — the pure, host-tested model that decides whether a
//! single squad can take a defended target and, if so, HOW (direct breach vs tower-drain). It replaces
//! the tower-count winnability proxy: it accounts for tower ENERGY (a drained tower deals no damage),
//! tower range/damage (the engine's tower curve), the breach-corridor cost (§12.3 — the cheapest
//! corridor, NOT a room-wide rampart sum), out-heal feasibility, and the squad's ON-SITE budget
//! (`CREEP_LIFE_TIME − spawn − travel`, supplied by the caller).
//!
//! Inputs are scalars / plain data so the oracle is decoupled from the live composition and the game
//! API (host-testable, and the same math drives the sim). The bot builds the [`DefenseProfile`] from
//! `RoomThreatData` + the candidate objective, supplies a [`ForceBudget`] from the chosen composition +
//! RCL, and maps the verdict back to a sized composition.
//!
//! Conservative by construction (so a "yes" is safe to commit and a "no" defers, never the reverse):
//! unboosted, full tower DPS sustained across the whole drain, and **single squad only** — v1 has no
//! synchronized pre-spawn replacement, so a siege that can't finish within one creep lifetime is judged
//! unwinnable and deferred to the multi-squad **G4-HEAVY** path, not committed.

use crate::bodies::{defender_heal_parts_for_dps, CombatBodySpec};
use crate::damage::tower_attack_damage_at_range;
use screeps_combat_engine::constants::{DISMANTLE_POWER, RANGED_ATTACK_POWER, TOWER_ENERGY_COST};

/// HOLD margin (ADR 0020 §12.5/§12.6): size heal to out-heal the incoming damage by this factor, NOT
/// break-even — so the squad HEALS THROUGH transient / approach / focused damage instead of tripping the
/// runtime `assess_engage` retreat on the first hit. Also the commit gate: only field a squad whose
/// margin-heal is affordable (never commit a fragile break-even squad). Seed; tuned by the SK/sim
/// scenarios.
pub const HOLD_MARGIN: f32 = 1.3;

/// FIX 3 headroom for the MINIMAL undefended-target kill rate (zero attrition, zero repair). The force is
/// sized to raze the structure within the on-site budget with this small surplus, so the optimizer's
/// integer part-rounding (whole RANGED parts) still clears comfortably inside the window without climbing
/// to the gross ceiling. Only applies to undefended, no-repair targets (the scoped FIX 3 path); the
/// defended/repairing path is unchanged.
const UNDEFENDED_KILL_HEADROOM: f32 = 1.15;

/// Coordinated square-law over-match seed (ADR 0026 §9.4/§9.8): a player's creeps fight TOGETHER
/// (focus-fire + mutual heal), so to win the attrition race with survivors we must OUT-power them, not
/// merely match — field this multiple of the break-even creep-clear force. An `Individual` fight (NPCs
/// fought one at a time) uses `1.0` (just beat the worst single). Seed; tournament-tuned on the
/// Coordinated player-squad bed (ADR 0026 §9.8, deferred — see the §9.10 ledger).
pub const COORDINATED_DPS_MARGIN: f32 = 1.5;

/// One hostile tower's threat to the planned assault position.
#[derive(Clone, Copy, Debug)]
pub struct TowerThreat {
    /// Chebyshev range from the tower to the assault tile (the tower-damage curve's input).
    pub range_to_assault: u32,
    /// Current stored energy; a tower with `< TOWER_ENERGY_COST` can't fire (counts as 0 DPS).
    pub energy: u32,
}

/// The target's defense as the oracle sees it — built bot-side from `RoomThreatData` + the objective.
#[derive(Clone, Debug, Default)]
pub struct DefenseProfile {
    pub towers: Vec<TowerThreat>,
    /// Breach-corridor hits to the objective (ADR 0020 §12.3; 0 = already reachable without dismantling).
    pub breach_hits: u32,
    /// Objective structure hits to destroy once reached (e.g. the invader core itself).
    pub objective_hits: u32,
    /// Hostile creep damage/tick at the objective.
    pub enemy_dps: f32,
    /// Defensive repair/tick of the breach target (tower/creep repair of ramparts); 0 for cores.
    pub repair_per_tick: f32,
    /// Owner safe-mode active → zero damage possible → a hard veto.
    pub safe_mode: bool,
}

/// What ONE squad brings + how long it has on-site. `onsite_budget_ticks` =
/// `CREEP_LIFE_TIME − spawn − travel`.
#[derive(Clone, Copy, Debug)]
pub struct ForceBudget {
    pub max_heal_per_tick: f32,
    pub max_dismantle_dps: f32,
    /// Effective HP of the squad's tank / front member — the drain-survival reserve.
    pub tank_effective_hp: f32,
    pub onsite_budget_ticks: u32,
}

/// How a winnable target is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssaultMode {
    /// Out-heal the towers and dismantle straight through the breach.
    Breach,
    /// A tank soaks tower fire until the towers run dry (10 energy/shot), then the squad breaches the
    /// dead base. Still a SINGLE squad (the tank drains, then the same squad dismantles).
    Drain,
}

/// The oracle's verdict for one squad vs one defense.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForceAssessment {
    pub winnable: bool,
    pub mode: AssaultMode,
    /// Heal/tick the fielded squad must sustain (the binding survival constraint of the chosen mode).
    pub required_heal_per_tick: f32,
    /// GROSS dismantle DPS the fielded squad must bring — it must out-pace the defensive repair AND
    /// clear the core (NOT the net-of-repair breach rate; sizing to the net would let repair cancel it
    /// twice and the squad would stall at the wall).
    pub required_dismantle_dps: f32,
    /// Estimated ticks to win (drain + breach + kill) — for ROI / the war supervisor.
    pub est_ticks: u32,
    /// Why unwinnable, or the chosen mode — for logging.
    pub reason: &'static str,
}

/// Ticks to deliver `amount` at `rate`/tick (ceil). `rate <= 0` ⇒ never (`u32::MAX`).
fn ticks_for(amount: f32, rate: f32) -> u32 {
    if rate <= 0.0 {
        u32::MAX
    } else {
        (amount / rate).ceil() as u32
    }
}

/// Damage/tick of the ENERGIZED towers at the assault position (the engine tower curve; drained towers
/// contribute 0 — the fix the per-tower energy intel enables). Public so the EV optimizer
/// ([`crate::composition::optimize_composition`]) can compute a candidate's incoming damage.
pub fn tower_dps_at_assault(towers: &[TowerThreat]) -> f32 {
    towers
        .iter()
        .filter(|t| t.energy >= TOWER_ENERGY_COST)
        .map(|t| tower_attack_damage_at_range(t.range_to_assault) as f32)
        .sum()
}

/// Ticks for the energized towers to run dry under sustained fire (each fires once/tick, −10 energy);
/// the slowest tower (the last to go silent) bounds the drain.
fn drain_ticks(towers: &[TowerThreat]) -> u32 {
    towers
        .iter()
        .filter(|t| t.energy >= TOWER_ENERGY_COST)
        .map(|t| t.energy.div_ceil(TOWER_ENERGY_COST))
        .max()
        .unwrap_or(0)
}

/// The force-sizing oracle (ADR 0020 §12.2): can `budget` (a single squad) beat `profile`, and via
/// which mode? See the module docs for the conservatism contract.
pub fn assess(profile: &DefenseProfile, budget: &ForceBudget) -> ForceAssessment {
    let unwinnable = |reason| ForceAssessment {
        winnable: false,
        mode: AssaultMode::Breach,
        required_heal_per_tick: 0.0,
        required_dismantle_dps: 0.0,
        est_ticks: 0,
        reason,
    };

    if profile.safe_mode {
        return unwinnable("enemy safe mode — zero damage possible");
    }

    let net_dismantle = budget.max_dismantle_dps - profile.repair_per_tick;
    if profile.breach_hits > 0 && net_dismantle <= 0.0 {
        return unwinnable("repair out-paces our dismantle");
    }
    let breach_ticks = ticks_for(profile.breach_hits as f32, net_dismantle.max(1.0));
    let kill_ticks = ticks_for(profile.objective_hits as f32, budget.max_dismantle_dps.max(1.0));

    let tower_dps = tower_dps_at_assault(&profile.towers);
    let incoming = tower_dps + profile.enemy_dps;

    // Direct breach: out-heal towers + creeps the whole time (with the HOLD margin so HP recovers
    // through damage and the squad doesn't early-retreat), dismantle through.
    let required_heal = incoming * HOLD_MARGIN;
    if required_heal <= budget.max_heal_per_tick {
        let total = breach_ticks.saturating_add(kill_ticks);
        if total <= budget.onsite_budget_ticks {
            // FIX 3 — UNDEFENDED, zero-attrition, no-repair target (e.g. a level-0 invader core: towers=[],
            // enemy dps=0, repair=0). The GROSS-dismantle sizing below exists to out-pace defensive REPAIR
            // (so repair can't cancel the breach twice at runtime). With NO repair and NO attrition there is
            // nothing to out-pace and no risk of stalling — the only requirement is to raze
            // `objective_hits + breach_hits` within the on-site budget. Sizing to the gross ceiling here
            // over-sizes a trivial core to the full 4-5-fighter ceiling DPS; the EV optimizer then can't
            // size below it. So size to the MINIMAL rate that clears within the window (a tiny headroom
            // margin absorbs the ceil rounding), letting the optimizer field the fewest-creeps force the
            // operator wants. SCOPED to `incoming == 0.0 && repair_per_tick == 0.0`; a DEFENDED or
            // repairing target keeps the gross-dismantle sizing UNCHANGED (the calibration gates test
            // defended sizing and must not shift).
            let undefended_no_repair = incoming == 0.0 && profile.repair_per_tick == 0.0;
            let required_dismantle_dps = if undefended_no_repair {
                let window = budget.onsite_budget_ticks.max(1) as f32;
                let minimal_rate = (profile.objective_hits + profile.breach_hits) as f32 / window;
                // A small headroom factor so the optimizer's integer part-rounding still clears within the
                // window; clamped to the gross ceiling so we never size ABOVE the defended path.
                (minimal_rate * UNDEFENDED_KILL_HEADROOM).clamp(1.0, budget.max_dismantle_dps.max(1.0))
            } else {
                // GROSS dismantle the squad must FIELD — not the net-of-repair RATE. `breach_ticks` and
                // `kill_ticks` above were computed assuming the squad brings the full
                // `budget.max_dismantle_dps`; sizing to the net (`max − repair`) would have repair
                // subtract a SECOND time at runtime, so the fielded squad delivers `net − repair` and
                // stalls at the rampart. The squad must out-pace repair AND clear the core, so it fields
                // the gross (`net + repair == max_dismantle_dps`).
                budget.max_dismantle_dps.max(1.0)
            };
            return ForceAssessment {
                winnable: true,
                mode: AssaultMode::Breach,
                required_heal_per_tick: required_heal,
                required_dismantle_dps,
                est_ticks: total,
                reason: "breach: out-heal the towers and dismantle through",
            };
        }
        return unwinnable("breach too slow for one creep lifetime");
    }

    // Drain: a tank soaks tower fire until the towers run dry, then the squad breaches the dead base.
    let dt = drain_ticks(&profile.towers);
    let tank_sustain = budget.tank_effective_hp + budget.max_heal_per_tick * dt as f32;
    let drain_damage = tower_dps * dt as f32;
    if dt > 0 && tank_sustain >= drain_damage {
        // After the drain only the enemy creeps remain — they must be out-healed (with the HOLD margin)
        // for the breach phase.
        let required_heal = profile.enemy_dps.max(1.0) * HOLD_MARGIN;
        if required_heal <= budget.max_heal_per_tick {
            let total = dt.saturating_add(breach_ticks).saturating_add(kill_ticks);
            if total <= budget.onsite_budget_ticks {
                return ForceAssessment {
                    winnable: true,
                    mode: AssaultMode::Drain,
                    required_heal_per_tick: required_heal,
                    // GROSS dismantle to field (see the Breach branch) — the squad must out-pace repair
                    // through the breach and clear the core.
                    required_dismantle_dps: budget.max_dismantle_dps.max(1.0),
                    est_ticks: total,
                    reason: "drain: soak the towers dry, then breach",
                };
            }
            return unwinnable("drain + breach too slow for one creep lifetime");
        }
        return unwinnable("enemy creeps out-heal our damage after the drain");
    }

    unwinnable("towers out-damage a single squad — needs heavy assault (G4-HEAVY)")
}

/// Size a CREEP-CLEAR engagement (ADR 0026 §9.4 — the keystone for the `PlayerDefend`/`PlayerRaid` rungs).
/// Unlike a structure breach (where [`assess`] sizes the kill DPS to the squad's *gross* so repair can't
/// stall it), a creep-clear sizes the kill DPS to the **enemy**: enough to grind their HP net of their
/// heal within the on-site window AND to OUT-POWER them by `dps_margin` (the Lanchester square-law decisive
/// win), plus heal to out-heal the incoming. The caller (a `ForceDoctrine`) resolves the coordination axis:
/// - **Individual** (NPCs fought one at a time): pass the WORST SINGLE enemy + `dps_margin = 1.0`.
/// - **Coordinated** (a player squad fighting together): pass the AGGREGATE force + `dps_margin =`
///   [`COORDINATED_DPS_MARGIN`] (out-power, don't merely match).
///
/// This is the pure sizing primitive the creep-clear doctrines (`PlayerRaid`/`GatedPlayerRaid`/
/// `GarrisonDefense`/`HarassRemote`) use, so the bot and sim size a creep-clear through ONE path (the §9
/// parity, extended to creeps). The kill is anti-creep RANGED: it sets `RequiredForce.anti_creep_parts`,
/// which [`crate::composition::assemble_force`] distributes across the RangedDPS role. Unwinnable ⇒ all-zero required.
///
/// WIRED via [`crate::doctrine::emit_requirement`] (the unified emitter every creep-clear objective flows
/// through); the tournament bed that tunes `dps_margin` is the §9.10 ledger's next rung.
pub fn clear_force(
    towers: Vec<TowerThreat>,
    enemy_dps: f32,
    enemy_hits: u32,
    enemy_heal: f32,
    budget: &ForceBudget,
    dps_margin: f32,
    safe_mode: bool,
) -> (ForceAssessment, RequiredForce) {
    let unwinnable = |reason| {
        (
            ForceAssessment { winnable: false, mode: AssaultMode::Breach, required_heal_per_tick: 0.0, required_dismantle_dps: 0.0, est_ticks: 0, reason },
            RequiredForce::default(),
        )
    };
    if safe_mode {
        return unwinnable("enemy safe mode — zero damage possible");
    }
    // Out-heal the incoming (towers + their dps) with the hold margin.
    let incoming = tower_dps_at_assault(&towers) + enemy_dps;
    let required_heal = incoming * HOLD_MARGIN;
    if required_heal > budget.max_heal_per_tick {
        return unwinnable("can't out-heal the incoming");
    }
    // Kill DPS = enough to grind their HP (net of their heal) within the on-site window, AND to out-power
    // them by `dps_margin` (the decisive square-law win). The margin is baked in here, so it scales the
    // KILL parts (not the heal — heal is sized to the incoming regardless).
    let kill_in_time = enemy_hits as f32 / budget.onsite_budget_ticks.max(1) as f32 + enemy_heal;
    let required_kill_dps = kill_in_time.max(enemy_dps * dps_margin.max(1.0)).max(1.0);
    if required_kill_dps <= enemy_heal {
        return unwinnable("their heal out-paces a feasible kill");
    }
    if required_kill_dps > budget.max_dismantle_dps {
        return unwinnable("can't field enough kill dps for one squad");
    }
    let est_ticks = (enemy_hits as f32 / (required_kill_dps - enemy_heal)).ceil() as u32;
    let a = ForceAssessment {
        winnable: true,
        mode: AssaultMode::Breach,
        required_heal_per_tick: required_heal,
        required_dismantle_dps: required_kill_dps,
        est_ticks,
        reason: "clear: out-heal + out-power the enemy creeps",
    };
    // The kill is ANTI-CREEP (we clear enemy creeps, not structures), so the kill parts go to
    // `anti_creep_parts`, NOT the structure terms (`dismantle_parts`/`immune_struct_parts`) that
    // `from_assessment` sets. Heal matches the defense path (`defender_heal_parts_for_dps`, parity).
    // (ADR 0031 Layer C — this is what lets a siege facing a guard carry BOTH dismantle AND anti-creep.)
    let required = RequiredForce {
        heal_parts: defender_heal_parts_for_dps(required_heal, false),
        anti_creep_parts: parts_for_rate(required_kill_dps, RANGED_ATTACK_POWER),
        ..Default::default()
    };
    (a, required)
}

// ─── R2: required-force → part counts (ADR 0020 §12.6) ───────────────────────
//
// The inverse of the composition's `capabilities()`: turn the oracle's required CAPABILITIES into the
// total PARTS a squad must field. `assemble_force` distributes these across the squad's roles
// and builds member bodies via `build_combat_body`; the gate then becomes "can an in-range
// home afford these parts?". Reuses the heal-part math (`defender_heal_parts_for_dps`) so heal sizing
// is consistent across defense and offense.

/// WORK dismantle per part/tick (engine `DISMANTLE_POWER`).
/// Total parts a squad must field to satisfy a [`ForceAssessment`] (R2). `assemble_force` distributes these
/// across the squad's roles + builds bodies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequiredForce {
    /// Σ HEAL parts — out-heal the assault position (`required_heal_per_tick`).
    pub heal_parts: u32,
    /// Σ WORK parts — breach + kill a DISMANTLE-able structure (`required_dismantle_dps`).
    pub dismantle_parts: u32,
    /// Σ RANGED_ATTACK parts — the SAME required structure-DPS as `dismantle_parts`, expressed in RANGED
    /// parts (it takes `DISMANTLE_POWER/RANGED_ATTACK_POWER` = 5× as many). `assemble_force` sizes the
    /// RangedDPS role from this and the Dismantler (WORK) role from `dismantle_parts` — the emitter zeroes
    /// the weapon the objective can't use. REQUIRED for a dismantle-IMMUNE target (an invader core / a
    /// Source Keeper) that only ranged/melee can kill (R-attack, §12.6).
    pub immune_struct_parts: u32,
    /// Σ RANGED/ATTACK parts to KILL blocking DEFENDER CREEPS (ADR 0031 Layer C) — distinct from
    /// `immune_struct_parts` (anti-structure). A force facing a guarded structure needs BOTH at once (raze
    /// the core AND clear the guard), so they are separate, not `max`-ed. Sized via `clear_force` over the
    /// observed `enemy_force`; both feed `assemble_force`'s RangedDPS role (the SUM). Zero when no defenders seen.
    pub anti_creep_parts: u32,
    /// Σ TOUGH parts — the effective-HP buffer. v1 = 0 (role bodies carry their own HP); the
    /// margin-driven EHP buffer is R5/D2.
    pub tough_parts: u32,
}

impl RequiredForce {
    /// Map a winnable assessment to total part counts. Reuses `defender_heal_parts_for_dps` (incoming-
    /// dps → HEAL parts) so heal sizing matches the defense path. Unwinnable ⇒ all-zero.
    pub fn from_assessment(a: &ForceAssessment) -> Self {
        if !a.winnable {
            return Self::default();
        }
        RequiredForce {
            heal_parts: defender_heal_parts_for_dps(a.required_heal_per_tick, false),
            dismantle_parts: parts_for_rate(a.required_dismantle_dps, DISMANTLE_POWER),
            // Same required structure-DPS, in RANGED parts — for a dismantle-immune target. The emitter
            // zeroes one weapon per objective; `assemble_force` sizes the surviving one (WORK vs RANGED). (R-attack §12.6.)
            immune_struct_parts: parts_for_rate(a.required_dismantle_dps, RANGED_ATTACK_POWER),
            // assess() is structure-only — defender DPS folds into heal, not a kill req. The unified emitter
            // (P2) adds anti-creep via clear_force over enemy_force; here it stays 0.
            anti_creep_parts: 0,
            tough_parts: 0,
        }
    }

    /// As a single-creep [`CombatBodySpec`] — the single-member case + the round-trip seam. `assemble_force`
    /// splits the totals across the squad's members instead of stacking them on one creep.
    pub fn as_solo_spec(&self) -> CombatBodySpec {
        // The single-member case is a dismantler (WORK); `ranged_parts` is the ALTERNATIVE weapon a
        // ranged-attacker SQUAD role uses (sized via `assemble_force`), not stacked onto one member (that would double the kill
        // weapon + blow the 50-part cap). So the solo spec stays heal+work+tough.
        CombatBodySpec {
            heal: self.heal_parts,
            work: self.dismantle_parts,
            tough: self.tough_parts,
            ..Default::default()
        }
    }

    /// Scale every part count up by `factor` (ceil), keeping zeros at zero (R5 importance-weighted
    /// investment). `factor >= 1.0` over-invests for high-value targets; `factor == 1.0` is a no-op.
    pub fn scaled(self, factor: f32) -> RequiredForce {
        let s = |n: u32| if n == 0 { 0 } else { (n as f32 * factor.max(1.0)).ceil() as u32 };
        RequiredForce {
            heal_parts: s(self.heal_parts),
            dismantle_parts: s(self.dismantle_parts),
            immune_struct_parts: s(self.immune_struct_parts),
            anti_creep_parts: s(self.anti_creep_parts),
            tough_parts: s(self.tough_parts),
        }
    }
}

/// R4 — probability we win/hold the engagement given our sustained `heal` vs the `incoming` damage.
/// A logistic on the heal surplus (`heal/incoming - 1`): 0.5 at break-even, rising as heal exceeds
/// incoming, → 1 when nothing hits us. The principled reading of the [`HOLD_MARGIN`]: a 1.3× margin
/// (+30% surplus) lands ≈ 0.82, i.e. "field enough to win ~4 times in 5".
pub fn win_probability(heal: f32, incoming: f32) -> f32 {
    if incoming <= 0.0 {
        return 1.0;
    }
    let surplus = heal / incoming - 1.0;
    1.0 / (1.0 + (-WIN_PROB_STEEPNESS * surplus).exp())
}

/// Logistic steepness for [`win_probability`], tuned so break-even = 0.5 and the +30% [`HOLD_MARGIN`]
/// surplus ≈ 0.82.
const WIN_PROB_STEEPNESS: f32 = 5.0;

/// R5 — extra force multiplier for objective `importance` ∈ [0,1]: over-invest for high-value targets,
/// down to a no-op (1.0) for marginal ones. Multiplies the base hold-margin [`RequiredForce`].
pub fn importance_margin(importance: f32) -> f32 {
    1.0 + importance.clamp(0.0, 1.0) * IMPORTANCE_MAX_EXTRA
}

/// Most a fully-important objective adds on top of the base hold margin (a CRITICAL target fields
/// 1.5× the minimum winning force).
const IMPORTANCE_MAX_EXTRA: f32 = 0.5;

/// Parts to deliver `rate`/tick at `power`/part (ceil). 0 when nothing is required.
fn parts_for_rate(rate: f32, power: u32) -> u32 {
    if rate <= 0.0 || power == 0 {
        0
    } else {
        (rate / power as f32).ceil() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── R2: RequiredForce (capability → parts) ──
    fn assessment(winnable: bool, heal: f32, dps: f32) -> ForceAssessment {
        ForceAssessment {
            winnable,
            mode: AssaultMode::Breach,
            required_heal_per_tick: heal,
            required_dismantle_dps: dps,
            est_ticks: 50,
            reason: "test",
        }
    }

    #[test]
    fn required_force_inverts_capabilities_with_ceil() {
        let rf = RequiredForce::from_assessment(&assessment(true, 120.0, 300.0));
        assert_eq!(rf.heal_parts, 10, "120 dmg/tick ÷ 12 HEAL/part");
        assert_eq!(rf.dismantle_parts, 6, "300 dps ÷ 50 DISMANTLE/part");
        assert!(rf.heal_parts * 12 >= 120 && rf.dismantle_parts * DISMANTLE_POWER >= 300);
    }

    #[test]
    fn required_force_is_zero_when_unwinnable() {
        assert_eq!(RequiredForce::from_assessment(&assessment(false, 999.0, 999.0)), RequiredForce::default());
    }

    // ── clear_force (ADR 0026 §9.4 creep-clear sizing) ──
    fn clear_budget() -> ForceBudget {
        ForceBudget { max_heal_per_tick: 600.0, max_dismantle_dps: 600.0, tank_effective_hp: 5000.0, onsite_budget_ticks: 1000 }
    }

    #[test]
    fn clear_force_individual_beats_a_single() {
        // One weak melee creep (30 dps, 1000 HP, no heal), Individual margin 1.0 → winnable, sizes ranged.
        let (a, rf) = clear_force(vec![], 30.0, 1000, 0.0, &clear_budget(), 1.0, false);
        assert!(a.winnable, "{}", a.reason);
        assert!(rf.anti_creep_parts > 0 && rf.heal_parts > 0, "sized anti-creep kill + heal parts: {rf:?}");
    }

    #[test]
    fn clear_force_coordinated_oversizes_the_kill_vs_individual() {
        // Same enemy; the Coordinated margin out-powers (more KILL parts) but heal is sized to the incoming
        // either way (the square-law scales DPS, not heal).
        let (_, ind) = clear_force(vec![], 200.0, 4000, 0.0, &clear_budget(), 1.0, false);
        let (_, coord) = clear_force(vec![], 200.0, 4000, 0.0, &clear_budget(), COORDINATED_DPS_MARGIN, false);
        assert!(coord.anti_creep_parts > ind.anti_creep_parts, "coordinated out-powers: {} > {}", coord.anti_creep_parts, ind.anti_creep_parts);
        assert_eq!(coord.heal_parts, ind.heal_parts, "heal sized to the incoming, not the margin");
    }

    #[test]
    fn clear_force_out_healed_is_unwinnable() {
        // Their heal (700) pushes the needed kill dps above our budget (600) → can't grind them down.
        let (a, rf) = clear_force(vec![], 50.0, 4000, 700.0, &clear_budget(), 1.0, false);
        assert!(!a.winnable, "out-healed enemy is unwinnable for one squad");
        assert_eq!(rf, RequiredForce::default());
    }

    #[test]
    fn clear_force_safe_mode_is_vetoed() {
        let (a, _) = clear_force(vec![], 30.0, 100, 0.0, &clear_budget(), 1.0, true);
        assert!(!a.winnable, "safe mode → zero damage possible");
    }

    #[test]
    fn clear_force_sizes_more_heal_for_towers() {
        // L4-activate: a player base's TOWERS add to the incoming → more out-heal. (Far tower so it stays
        // winnable within the budget.)
        let no_tower = clear_force(vec![], 100.0, 0, 0.0, &clear_budget(), 1.5, false).1;
        let with_tower = clear_force(vec![TowerThreat { range_to_assault: 20, energy: 1000 }], 100.0, 0, 0.0, &clear_budget(), 1.5, false).1;
        assert!(with_tower.heal_parts > no_tower.heal_parts, "towers raise the out-heal: {} > {}", with_tower.heal_parts, no_tower.heal_parts);
    }

    #[test]
    fn required_force_spec_is_buildable_by_r1() {
        // R1∘R2 seam: the spec R2 produces builds into a real body at RCL7 energy.
        let rf = RequiredForce::from_assessment(&assessment(true, 120.0, 300.0));
        let spec = rf.as_solo_spec();
        assert_eq!(spec.heal, 10);
        assert_eq!(spec.work, 6);
        assert!(
            crate::bodies::build_combat_body(&spec, crate::bodies::MoveProfile::Plains, 5600).is_some(),
            "the required-force spec is affordable + fits at RCL7"
        );
    }

    /// A budget that can heal/dismantle a lot with a long on-site window — so the DEFENSE drives each
    /// test's outcome, not the budget.
    fn strong_budget() -> ForceBudget {
        ForceBudget {
            max_heal_per_tick: 900.0,
            max_dismantle_dps: 600.0,
            tank_effective_hp: 50_000.0,
            onsite_budget_ticks: 1400,
        }
    }

    fn tower(range: u32, energy: u32) -> TowerThreat {
        TowerThreat { range_to_assault: range, energy }
    }

    #[test]
    fn safe_mode_is_a_hard_veto() {
        let profile = DefenseProfile { safe_mode: true, ..Default::default() };
        assert!(!assess(&profile, &strong_budget()).winnable);
    }

    #[test]
    fn weak_single_tower_is_a_direct_breach() {
        let profile = DefenseProfile {
            towers: vec![tower(5, 1000)],
            breach_hits: 30_000,
            objective_hits: 100_000,
            ..Default::default()
        };
        let a = assess(&profile, &strong_budget());
        assert!(a.winnable);
        assert_eq!(a.mode, AssaultMode::Breach);
    }

    #[test]
    fn drained_towers_do_not_count() {
        let profile = DefenseProfile {
            towers: vec![tower(1, 5), tower(1, 0), tower(1, 9)],
            breach_hits: 10_000,
            objective_hits: 50_000,
            ..Default::default()
        };
        let weak_heal = ForceBudget { max_heal_per_tick: 50.0, ..strong_budget() };
        let a = assess(&profile, &weak_heal);
        assert!(a.winnable, "drained towers deal no damage, so this is winnable: {}", a.reason);
        assert_eq!(a.mode, AssaultMode::Breach);
    }

    #[test]
    fn strong_towers_force_the_drain_path() {
        let profile = DefenseProfile {
            towers: vec![tower(1, 100); 6],
            breach_hits: 20_000,
            objective_hits: 80_000,
            ..Default::default()
        };
        let a = assess(&profile, &strong_budget());
        assert!(a.winnable, "should be drainable: {}", a.reason);
        assert_eq!(a.mode, AssaultMode::Drain);
    }

    #[test]
    fn deep_energy_towers_are_unwinnable_for_one_squad() {
        let profile = DefenseProfile {
            towers: vec![tower(1, 100_000); 6],
            breach_hits: 20_000,
            objective_hits: 80_000,
            ..Default::default()
        };
        let a = assess(&profile, &strong_budget());
        assert!(!a.winnable);
        assert!(a.reason.contains("heavy assault"), "reason: {}", a.reason);
    }

    #[test]
    fn breach_too_slow_for_one_lifetime_is_unwinnable() {
        let profile = DefenseProfile {
            towers: vec![tower(5, 1000)],
            breach_hits: 10_000_000,
            objective_hits: 100_000,
            ..Default::default()
        };
        let a = assess(&profile, &strong_budget());
        assert!(!a.winnable);
        assert!(a.reason.contains("too slow"), "reason: {}", a.reason);
    }

    #[test]
    fn required_dismantle_is_gross_not_net_of_repair() {
        // A repairing rampart: the fielded squad must out-pace repair, so the required dismantle is the
        // GROSS the winnability was computed at (the budget's full dismantle), NOT the net-of-repair
        // rate — else `assemble_force` under-sizes WORK by `repair` and the squad stalls at the wall (the
        // oracle bug the offline oracle-calibration tournament caught).
        let profile = DefenseProfile { breach_hits: 50_000, objective_hits: 100_000, repair_per_tick: 200.0, ..Default::default() };
        let a = assess(&profile, &strong_budget());
        assert!(a.winnable, "600 gross out-paces 200 repair: {}", a.reason);
        assert_eq!(a.required_dismantle_dps, 600.0, "field the GROSS dismantle, not the net-of-repair rate (200 less)");
        let rf = RequiredForce::from_assessment(&a);
        assert!(
            rf.dismantle_parts as f32 * DISMANTLE_POWER as f32 - 200.0 > 0.0,
            "the fielded WORK ({} parts) out-paces the 200 repair with breach rate to spare",
            rf.dismantle_parts
        );
    }

    #[test]
    fn repair_locked_breach_target_defers() {
        let repair_locked = DefenseProfile {
            breach_hits: 50_000,
            objective_hits: 100_000,
            repair_per_tick: 700.0, // ≥ strong_budget's 600 max_dismantle_dps
            ..Default::default()
        };
        let a = assess(&repair_locked, &strong_budget());
        assert!(!a.winnable);
        assert!(a.reason.contains("repair out-paces"), "reason: {}", a.reason);
        let out_paced = DefenseProfile { repair_per_tick: 200.0, ..repair_locked.clone() };
        assert!(assess(&out_paced, &strong_budget()).winnable, "600 dismantle out-paces 200 repair");
    }

    #[test]
    fn undefended_room_is_a_no_breach_win() {
        let profile = DefenseProfile { objective_hits: 50_000, ..Default::default() };
        let a = assess(&profile, &strong_budget());
        assert!(a.winnable);
        assert_eq!(a.mode, AssaultMode::Breach);
        assert_eq!(a.required_heal_per_tick, 0.0, "nothing is shooting us");
    }

    // ── R4: P(win) model ──
    #[test]
    fn win_probability_reads_the_hold_margin() {
        assert_eq!(win_probability(100.0, 0.0), 1.0, "nothing hitting us → certain");
        assert!((win_probability(100.0, 100.0) - 0.5).abs() < 1e-3, "break-even → coin-flip");
        let p = win_probability(130.0, 100.0);
        assert!(p > 0.80 && p < 0.85, "the 1.3 hold margin ≈ 0.82 P(win), got {p}");
        assert!(win_probability(200.0, 100.0) > win_probability(130.0, 100.0));
    }

    // ── R5: importance-weighted investment ──
    #[test]
    fn importance_scales_the_invested_force() {
        assert_eq!(importance_margin(0.0), 1.0);
        let base = RequiredForce { heal_parts: 10, dismantle_parts: 6, immune_struct_parts: 0, anti_creep_parts: 0, tough_parts: 0 };
        assert_eq!(base.scaled(importance_margin(0.0)), base, "importance 0 → no over-invest");
        assert_eq!(importance_margin(1.0), 1.5);
        let crit = base.scaled(importance_margin(1.0));
        assert_eq!(crit.heal_parts, 15, "10 × 1.5");
        assert_eq!(crit.dismantle_parts, 9, "6 × 1.5");
        assert_eq!(crit.tough_parts, 0, "zero stays zero");
        assert!(crit.heal_parts >= base.heal_parts && base.scaled(0.5) == base, "factor < 1 is clamped to no-op");
    }
}
