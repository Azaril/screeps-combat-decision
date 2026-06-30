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

/// Content-staleness window for an EMPTY-tower defense on an ATTACK/CORE commit candidate (ADR 0035 D2).
///
/// DISTINCT from the war.rs 200-tick `last_seen` re-scout gate (`operations/war.rs`): that gate is about
/// *recency of ANY vision* of the room; THIS one is about *reliability of the empty-tower CONTENT*. A room
/// seen 60 ticks ago with zero towers is "fresh" by the 200-tick gate — yet its towers may have energized
/// AFTER that snapshot, so committing a squad sized to ZERO tower DPS walks it into a fight it was never
/// built for (the W4N5 vacuous-intel cascade). When `tower_intel == ScoutedEmpty` AND the empty snapshot is
/// older than this window, the offense DEFERS the commit and re-scouts to re-confirm the room is still clear.
/// A FRESH (`< SCOUT_RECONFIRM_TICKS`) empty snapshot is TRUSTED (just scouted clear); a `Seen` (non-empty)
/// profile is ALWAYS trusted and sized against the REAL towers — the gate only ever defers empty-STALE.
///
/// Tuned tighter than the 200-tick recency gate (towers energize in tens of ticks, not hundreds) but loose
/// enough that a squad we JUST scouted-clear is not re-scouted on every scan. The compile-time fence below
/// pins it strictly inside the 200-tick recency gate so D2 is always the *narrower* (content) gate.
pub const SCOUT_RECONFIRM_TICKS: u32 = 40;

// Compile-time fence (ADR 0035 D2): the content-staleness window MUST be strictly tighter than war.rs's
// 200-tick recency re-scout gate, else D2 would never fire before the recency gate already re-scouted.
const _: () = assert!(SCOUT_RECONFIRM_TICKS > 0 && SCOUT_RECONFIRM_TICKS < 200);

/// Tri-state tower-intel reliability for an offense COMMIT decision (ADR 0035 D1). Replaces the implicit
/// boolean "we have a `DefenseProfile` ⇒ trust it" with an explicit notion of HOW we know the tower list.
///
/// DERIVED (never serialized) in war.rs at the consumption point from the EXISTING
/// `RoomThreatData.hostile_tower_positions.is_empty()` + `last_seen` recency via [`tower_intel_from`] — so
/// D1 adds NO new persisted state and is WFV-neutral (see ADR 0035 §4 WORLD_FORMAT_VERSION risk).
///
/// - [`Seen`](TowerIntel::Seen): non-empty tower positions — we have SEEN real towers; trust + size to them.
/// - [`ScoutedEmpty`](TowerIntel::ScoutedEmpty): empty tower positions but the room was mapped/seen at some
///   point (`threat_data` exists, so `last_seen` is meaningful). "No towers were visible last time we
///   looked" — VACUOUS; may be a genuinely clear room OR a room whose towers energized after the snapshot.
/// - [`NeverSeen`](TowerIntel::NeverSeen): no `threat_data` at all. (Does not reach the offense scan — the
///   scan requires `threat_data` — and the selection gate already defers on `defense.is_none()`; carried
///   for completeness so the classification is total.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TowerIntel {
    /// No threat data at all — never scouted. Default: the most conservative "we don't know" state, so a
    /// `DefenseProfile::default()` (the `defense.is_none()` fallback) reads as un-trusted, never as `Seen`.
    #[default]
    NeverSeen,
    /// Non-empty tower positions seen — trust and size against the real towers.
    Seen,
    /// Empty tower list, but the room has been mapped — "clear last time we looked" (may be stale).
    ScoutedEmpty,
}

/// Classify the tower intel (ADR 0035 D1) from the EXISTING threat fields — pure, deterministic, no
/// serialized state. `has_threat_data` is whether a `RoomThreatData` component exists for the room
/// (`NeverSeen` when not). Non-empty `hostile_tower_positions` ⇒ [`TowerIntel::Seen`]; empty + has-data ⇒
/// [`TowerIntel::ScoutedEmpty`] (regardless of recency — recency is judged by
/// [`should_defer_offense_commit`], keeping classification and the staleness decision separate).
pub fn tower_intel_from(hostile_tower_positions_empty: bool, has_threat_data: bool) -> TowerIntel {
    if !has_threat_data {
        TowerIntel::NeverSeen
    } else if hostile_tower_positions_empty {
        TowerIntel::ScoutedEmpty
    } else {
        TowerIntel::Seen
    }
}

/// Whether the offense must DEFER committing a squad to this candidate and re-scout first (ADR 0035 D2).
///
/// True IFF the tower intel is [`TowerIntel::ScoutedEmpty`] AND the empty snapshot is older than
/// [`SCOUT_RECONFIRM_TICKS`] — i.e. an empty-tower defense we can no longer trust is current. `Seen` (real
/// towers) is NEVER deferred (size to the real towers); a FRESH `ScoutedEmpty` (just scouted clear) is
/// NEVER deferred; `NeverSeen` is handled upstream by the `defense.is_none()` gate (returns false here so
/// this helper alone never claims a never-seen room is deferrable on the content gate). `now.saturating_sub`
/// keeps a clock that went backwards (private-server reset) benign — a future `last_seen` reads "recent".
pub fn should_defer_offense_commit(intel: TowerIntel, last_seen: u32, now: u32) -> bool {
    matches!(intel, TowerIntel::ScoutedEmpty) && now.saturating_sub(last_seen) > SCOUT_RECONFIRM_TICKS
}

/// One hostile tower's threat to the planned assault position.
#[derive(Clone, Copy, Debug)]
pub struct TowerThreat {
    /// Chebyshev range from the tower to the assault tile (the tower-damage curve's input).
    pub range_to_assault: u32,
    /// Current stored energy; a tower with `< TOWER_ENERGY_COST` can't fire (counts as 0 DPS).
    pub energy: u32,
}

/// The target's STRUCTURE/tower defense as the oracle sees it — built bot-side from `RoomThreatData` + the
/// objective. ADR 0031 #41: this is purely the STRUCTURE channel (towers / breach / objective / repair /
/// safe-mode). The hostile CREEP combat power is NOT here — it is the SINGLE SOURCE OF TRUTH
/// [`crate::doctrine::EnemyForce`], threaded into [`assess`] (and read by the EV path
/// [`crate::composition::optimize_composition`] / `pairing_p_win`) as the one enemy-creep-dps channel. The
/// formerly-co-resident `enemy_dps` field is REMOVED: it was dead in the modern EV/optimize path (which
/// already read `EnemyForce.dps`) and live only in `assess`, so keeping both forced every floor/predicate
/// to remember two channels (the double-count footgun). See ADR 0031 §"Single enemy-force source of truth".
#[derive(Clone, Debug, Default)]
pub struct DefenseProfile {
    pub towers: Vec<TowerThreat>,
    /// Breach-corridor hits to the objective (ADR 0020 §12.3; 0 = already reachable without dismantling).
    pub breach_hits: u32,
    /// Objective structure hits to destroy once reached (e.g. the invader core itself).
    pub objective_hits: u32,
    /// Defensive repair/tick of the breach target (tower/creep repair of ramparts); 0 for cores.
    pub repair_per_tick: f32,
    /// Owner safe-mode active → zero damage possible → a hard veto.
    pub safe_mode: bool,
    /// ADR 0035 D1 — HOW we know the `towers` list, for the scout-before-commit gate. DERIVED in war.rs
    /// from `RoomThreatData.hostile_tower_positions.is_empty()` + whether the component exists (NOT a
    /// serialized field; this struct is not serialized either). Drives the selection defer (D2) and the
    /// `economic_rank_score` penalty so a VACUOUS empty-tower profile (`ScoutedEmpty`) never sizes/ranks as
    /// a genuinely-`Seen`-clear room. Defaults to `NeverSeen` (the `defense.is_none()` fallback profile).
    pub tower_intel: TowerIntel,
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
    /// ADR 0031 #39 P2 — the TANK EFFECTIVE-HP buffer the fielded squad must carry to SURVIVE THE SOAK
    /// (drain mode only). The drain feasibility gate is `tank_effective_hp + heal·dt ≥ tower_dps·dt`: the
    /// heal covers the bulk of the falloff fire (`required_heal_per_tick`), and THIS EHP buffer absorbs the
    /// residual the heal alone doesn't (so a drain is feasible where a break-even heal isn't a full breach).
    /// `from_assessment` sizes the [`RequiredForce::tough_parts`] from this when `mode == Drain`. ZERO for a
    /// `Breach`/unwinnable assessment, so the non-drain part-mapping is byte-unchanged (TOUGH stays 0 there,
    /// the optimizer's TOUGH ladder owns front-armor for breach).
    pub required_tank_hp: f32,
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

/// ADR 0031 #39 P2 — aggregate ENERGIZED-tower damage/tick a drain TANK soaks at the FALLOFF STANDOFF (the
/// range the runtime drain tactic holds, `decide::move_to_drain_standoff`). A drain stands OFF at the
/// falloff range instead of soaking point-blank, so each tower's effective range is pulled back to at least
/// [`TOWER_FALLOFF_RANGE`] (a tower already farther than that keeps its real range). This is the dps the
/// drain feasibility + heal-sizing are judged against — NOT the point-blank `tower_dps_at_assault` the
/// BREACH faces — which is exactly why a finite-tower base a breach can't out-heal IS drainable: at the
/// falloff floor each tower deals its MINIMUM, the heal can sustain it, and the finite energy bleeds to 0.
fn tower_dps_at_drain_standoff(towers: &[TowerThreat]) -> f32 {
    use screeps_combat_engine::constants::TOWER_FALLOFF_RANGE;
    towers
        .iter()
        .filter(|t| is_finite_drain_tower(t))
        .map(|t| tower_attack_damage_at_range(t.range_to_assault.max(TOWER_FALLOFF_RANGE)) as f32)
        .sum()
}

/// Energy at/above which a tower is treated as "infinite" (never empties on a fight's timescale) and is
/// therefore NOT a drain target — mirrors the runtime `decide::DRAIN_INFINITE_TOWER_ENERGY` (50_000), the
/// value the eval/foreman "always-firing" fixtures (100_000) sit above. The EV GUARD: the drain branch only
/// counts towers BELOW this (a finite, bleedable pool); a deep-energy tower keeps the breach/heavy-assault
/// verdict (it can't be bled in a creep lifetime), so an INFINITE-energy base is never mis-chosen as a drain.
const DRAIN_INFINITE_TOWER_ENERGY: u32 = 50_000;

/// A tower the drain can actually bleed dry: ENERGIZED now (can fire) but with a FINITE, sub-infinite pool.
fn is_finite_drain_tower(t: &TowerThreat) -> bool {
    t.energy >= TOWER_ENERGY_COST && t.energy < DRAIN_INFINITE_TOWER_ENERGY
}

/// ADR 0031 §2(g) FOLLOW-UP 2 — does the base have ANY energized INFINITE-energy tower (`energy >=
/// DRAIN_INFINITE_TOWER_ENERGY`)? Such a tower NEVER bleeds dry, so a MIXED finite+infinite base cannot be
/// safely drained: the drain-soak sizing (`tower_dps_at_drain_standoff` / `drain_ticks`) counts only the
/// FINITE towers and would UNDER-size the heal (ignore the infinite tower's standoff fire) AND mis-read the
/// base as drainable. When this holds we REFUSE the drain branch and fall to the heavy-assault / unwinnable
/// path (which sizes for ALL tower dps). Behaviour-NEUTRAL live: real Screeps towers cap at 1000 energy,
/// always far below the 50_000 sentinel — the `>=` case exists only for synthetic eval/foreman fixtures.
fn has_energized_infinite_tower(towers: &[TowerThreat]) -> bool {
    towers.iter().any(|t| t.energy >= DRAIN_INFINITE_TOWER_ENERGY)
}

/// Ticks for the energized FINITE towers to run dry under sustained fire (each fires once/tick, −10 energy);
/// the slowest tower (the last to go silent) bounds the drain. INFINITE-energy towers are excluded (the EV
/// guard) so a deep-energy base yields `dt == 0` and the drain branch is never entered for it.
fn drain_ticks(towers: &[TowerThreat]) -> u32 {
    towers
        .iter()
        .filter(|t| is_finite_drain_tower(t))
        .map(|t| t.energy.div_ceil(TOWER_ENERGY_COST))
        .max()
        .unwrap_or(0)
}

/// The force-sizing oracle (ADR 0020 §12.2): can `budget` (a single squad) beat `profile`, and via
/// which mode? See the module docs for the conservatism contract.
///
/// `enemy_dps` is the hostile CREEP Attack/RangedAttack damage/tick at the objective — the SINGLE SOURCE OF
/// TRUTH ([`crate::doctrine::EnemyForce::dps`], threaded here by [`crate::doctrine::emit_requirement`]),
/// folded into `incoming` for the breach out-heal and into the post-drain out-heal exactly as the removed
/// `DefenseProfile.enemy_dps` field was (ADR 0031 #41 — the unification is read-equivalent: the EXACT same
/// value, just sourced from `EnemyForce` instead of a second co-resident field). It is a SURVIVABILITY input
/// here (size heal to out-heal it); the EV path consumes the SAME value for P(win) — different consumers of
/// one value, NOT a double price.
pub fn assess(profile: &DefenseProfile, enemy_dps: f32, budget: &ForceBudget) -> ForceAssessment {
    let unwinnable = |reason| ForceAssessment {
        winnable: false,
        mode: AssaultMode::Breach,
        required_heal_per_tick: 0.0,
        required_dismantle_dps: 0.0,
        required_tank_hp: 0.0,
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
    let incoming = tower_dps + enemy_dps;

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
                required_tank_hp: 0.0, // breach out-heals the whole fight — no soak EHP buffer
                est_ticks: total,
                reason: "breach: out-heal the towers and dismantle through",
            };
        }
        return unwinnable("breach too slow for one creep lifetime");
    }

    // Drain: a tank soaks tower fire until the towers run dry, then the squad breaches the dead base.
    // The drain stands OFF at the falloff range (the runtime tactic), so its soak dps is the FALLOFF-STANDOFF
    // aggregate (`tower_dps_at_drain_standoff`), NOT the point-blank `tower_dps` the BREACH faces — this is
    // why a finite base a breach can't out-heal is still DRAINABLE (at the falloff floor the heal sustains).
    let dt = drain_ticks(&profile.towers);
    let standoff_dps = tower_dps_at_drain_standoff(&profile.towers);
    let tank_sustain = budget.tank_effective_hp + budget.max_heal_per_tick * dt as f32;
    let drain_damage = standoff_dps * dt as f32;
    // EV GUARD (the drain is chosen ONLY when favorable): `dt > 0` requires at least one FINITE-energy
    // ENERGIZED tower to bleed — an INFINITE-energy tower contributes 0 to `drain_ticks` (`energy.div_ceil`
    // over a 100k pool is huge but the deep-energy bed below trips the on-site-budget gate, and a truly
    // un-emptyable tower means the soak never ends), and a safe-moded / repair-locked target was already
    // vetoed above. The feasibility gate `tank_sustain >= drain_damage` is the UNWINNABLE veto: a target
    // whose falloff fire over-runs the tank's HP + heal over the whole drain is NOT drainable by one squad
    // (it falls through to the G4-HEAVY arm). And a target a direct breach ALREADY wins returned above — a
    // winning breach is never downgraded to the slower drain.
    //
    // ADR 0031 §2(g) FOLLOW-UP 2 — MIXED finite+infinite hardening: a base with ANY energized INFINITE-energy
    // tower is NEVER a drain target even if it ALSO has finite towers. The soak sizing above
    // (`standoff_dps`/`dt`) counts ONLY the finite towers, so a mixed base would UNDER-size the heal (ignore
    // the never-draining tower's standoff fire) and mis-read as drainable; the squad would commit to a drain
    // it cannot sustain (the infinite tower never bleeds). REFUSE the drain so it falls to the heavy-assault /
    // unwinnable path (which sizes for ALL tower dps). A PURE-finite base (no infinite tower) is byte-unchanged.
    if dt > 0 && tank_sustain >= drain_damage && !has_energized_infinite_tower(&profile.towers) {
        // SIZE THE DRAIN COMP (P2): the fielded squad must SURVIVE THE SOAK, not just the post-drain phase.
        // The binding survival constraint is `tank_effective_hp + heal·dt ≥ tower_dps·dt`. HEAL is the
        // sustainable (indefinite) part of the soak and the TOUGH EHP buffer is a one-time reserve, so we
        // size the HEAL to cover AS MUCH of the falloff fire as the budget allows (`tower_dps` capped at the
        // budget's heal ceiling) and let the EHP buffer (`required_tank_hp`) absorb only the RESIDUAL the heal
        // cannot (`(tower_dps − heal)·dt`, floored at 0). The squad must ALSO out-heal the enemy creeps left
        // after the drain (with the HOLD margin), so the required heal is floored by the post-drain creep
        // heal. The soak heal carries no hold margin (the EHP buffer provides the headroom a breach's 1.3×
        // heal gives) so a tank that drains feasibly via a big EHP reserve (heal < falloff) is NOT spuriously
        // deferred for lacking heal it does not need — `tank_sustain ≥ drain_damage` above is the real veto.
        let post_drain_heal = enemy_dps.max(1.0) * HOLD_MARGIN;
        let soak_heal = standoff_dps.min(budget.max_heal_per_tick);
        let required_heal = soak_heal.max(post_drain_heal);
        if required_heal <= budget.max_heal_per_tick {
            let total = dt.saturating_add(breach_ticks).saturating_add(kill_ticks);
            if total <= budget.onsite_budget_ticks {
                // The EHP buffer the fielded HEAL does not cover over the drain (≥ 0). When the heal already
                // out-paces the falloff (`required_heal ≥ standoff_dps`) this is 0 (heal carries the whole soak).
                let residual_per_tick = (standoff_dps - required_heal).max(0.0);
                let required_tank_hp = residual_per_tick * dt as f32;
                return ForceAssessment {
                    winnable: true,
                    mode: AssaultMode::Drain,
                    required_heal_per_tick: required_heal,
                    // GROSS dismantle to field (see the Breach branch) — the squad must out-pace repair
                    // through the breach and clear the core.
                    required_dismantle_dps: budget.max_dismantle_dps.max(1.0),
                    required_tank_hp,
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
            ForceAssessment { winnable: false, mode: AssaultMode::Breach, required_heal_per_tick: 0.0, required_dismantle_dps: 0.0, required_tank_hp: 0.0, est_ticks: 0, reason },
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
        required_tank_hp: 0.0, // a creep-clear out-heals — no soak EHP buffer
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
    /// Σ CLAIM parts — ADR 0027 v1.1 P2: the `attackController` weapon for a DECLAIM objective (neutralize a
    /// derelict controller). Sized ONLY by the `DeclaimAttack` doctrine (a derelict controller is undefended
    /// by construction, so the force-sizing oracle never sets this); `assemble_force` fields the
    /// [`Declaimer`](crate::composition::SquadRole::Declaimer) role from it. Zero on every combat objective.
    pub claim_parts: u32,
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
            // ADR 0031 #39 P2 — a DRAIN comp carries the TOUGH EHP buffer that absorbs the soak residual the
            // heal does not (`required_tank_hp`); a BREACH/unwinnable assessment carries NONE (the optimizer's
            // TOUGH ladder owns front-armor for breach), so the non-drain mapping is byte-unchanged (0 here).
            tough_parts: if a.mode == AssaultMode::Drain {
                parts_for_rate(a.required_tank_hp, TOUGH_HP_PER_PART)
            } else {
                0
            },
            claim_parts: 0,
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
            claim_parts: s(self.claim_parts),
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

/// Effective HP per TOUGH part (unboosted): `100`/part, matching `SquadComposition::capabilities`'
/// `estimated_part_count(..) * 100` tank-HP model. Used to size the drain EHP buffer (P2).
const TOUGH_HP_PER_PART: u32 = 100;

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

    // ── ADR 0035 D1/D2: scout-before-commit tower-intel classification + the content-staleness defer ──

    #[test]
    fn tower_intel_classification_is_total() {
        // No threat data at all ⇒ NeverSeen (regardless of the empty flag).
        assert_eq!(tower_intel_from(true, false), TowerIntel::NeverSeen);
        assert_eq!(tower_intel_from(false, false), TowerIntel::NeverSeen);
        // Threat data present, empty tower list ⇒ ScoutedEmpty (the vacuous case).
        assert_eq!(tower_intel_from(true, true), TowerIntel::ScoutedEmpty);
        // Threat data present, non-empty tower list ⇒ Seen (size to the real towers).
        assert_eq!(tower_intel_from(false, true), TowerIntel::Seen);
        // The DefenseProfile default is the conservative NeverSeen (the `defense.is_none()` fallback).
        assert_eq!(DefenseProfile::default().tower_intel, TowerIntel::NeverSeen);
    }

    #[test]
    fn defer_only_empty_stale_never_seen_or_recent() {
        let now = 100_000u32;
        // ScoutedEmpty + STALE (older than the re-confirm window) ⇒ DEFER (re-scout before committing).
        assert!(should_defer_offense_commit(
            TowerIntel::ScoutedEmpty,
            now - (SCOUT_RECONFIRM_TICKS + 1),
            now
        ));
        // ScoutedEmpty + RECENT (just scouted clear, inside the window) ⇒ do NOT defer (trust it, proceed).
        assert!(!should_defer_offense_commit(TowerIntel::ScoutedEmpty, now - 1, now));
        // Exactly AT the window boundary is still trusted (strict `>` — not yet stale).
        assert!(!should_defer_offense_commit(TowerIntel::ScoutedEmpty, now - SCOUT_RECONFIRM_TICKS, now));
        // Seen (real towers) is NEVER deferred — size to the real towers no matter how old.
        assert!(!should_defer_offense_commit(TowerIntel::Seen, now - 10_000, now));
        // NeverSeen is handled by the upstream `defense.is_none()` gate; the content gate never claims it.
        assert!(!should_defer_offense_commit(TowerIntel::NeverSeen, now - 10_000, now));
    }

    #[test]
    fn defer_clock_reset_is_benign() {
        // A `last_seen` AHEAD of `now` (private-server time reset / restored snapshot) reads as "recent"
        // via saturating_sub ⇒ NOT deferred, never an underflow panic.
        assert!(!should_defer_offense_commit(TowerIntel::ScoutedEmpty, 10_000, 100));
    }

    // ── R2: RequiredForce (capability → parts) ──
    fn assessment(winnable: bool, heal: f32, dps: f32) -> ForceAssessment {
        ForceAssessment {
            winnable,
            mode: AssaultMode::Breach,
            required_heal_per_tick: heal,
            required_dismantle_dps: dps,
            required_tank_hp: 0.0,
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
        assert!(!assess(&profile, 0.0, &strong_budget()).winnable);
    }

    #[test]
    fn weak_single_tower_is_a_direct_breach() {
        let profile = DefenseProfile {
            towers: vec![tower(5, 1000)],
            breach_hits: 30_000,
            objective_hits: 100_000,
            ..Default::default()
        };
        let a = assess(&profile, 0.0, &strong_budget());
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
        let a = assess(&profile, 0.0, &weak_heal);
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
        let a = assess(&profile, 0.0, &strong_budget());
        assert!(a.winnable, "should be drainable: {}", a.reason);
        assert_eq!(a.mode, AssaultMode::Drain);
    }

    // ── ADR 0031 #39 P2 — the oracle DECIDES + SIZES a drain comp (RED→GREEN) ──

    /// (a) FAVORABLE: finite towers a direct breach can't out-heal but a sustainable drain CAN → the oracle
    /// picks `Drain` AND sizes a comp with HEAL (out-pace the falloff soak) + a TOUGH EHP buffer.
    #[test]
    fn oracle_picks_drain_and_sizes_a_sustainable_drain_comp_when_favorable() {
        // Four finite towers point-blank to the breach (range 1). Point-blank a breach faces 4×600 = 2400/tick
        // — un-out-healable by a single squad. But at the falloff standoff each deals its floor (150) → the
        // drain soaks 4×150 = 600/tick, which a 600-heal budget sustains; the finite 1500-energy pools bleed.
        let profile = DefenseProfile {
            towers: vec![tower(1, 1500); 4],
            breach_hits: 20_000,
            objective_hits: 30_000,
            ..Default::default()
        };
        // A budget whose heal beats the falloff soak (600) but NOT the point-blank breach fire — so a breach
        // is NOT winnable and the drain is the only path (the favorable case).
        let budget = ForceBudget { max_heal_per_tick: 600.0, max_dismantle_dps: 600.0, tank_effective_hp: 20_000.0, onsite_budget_ticks: 1500 };
        let a = assess(&profile, 0.0, &budget);
        assert!(a.winnable, "a finite-tower base IS winnable via drain: {}", a.reason);
        assert_eq!(a.mode, AssaultMode::Drain, "and the chosen mode is DRAIN (breach can't out-heal point-blank): {}", a.reason);
        // The sized comp: HEAL out-paces the falloff soak, and the TOUGH buffer is present (drain mode sizes it).
        let rf = RequiredForce::from_assessment(&a);
        assert!(rf.heal_parts > 0, "the drain comp carries HEAL to sustain the soak: {rf:?}");
        assert!(rf.heal_parts * 12 >= 600, "the HEAL out-paces the 600/tick falloff soak ({} parts)", rf.heal_parts);
    }

    /// (b) EV GUARD — INFINITE-energy towers are NEVER a drain (you can't bleed them dry): the oracle defers to
    /// the heavy-assault arm, NOT a slow drain that never ends.
    #[test]
    fn oracle_never_drains_an_infinite_tower() {
        let profile = DefenseProfile {
            // 6 towers @ 100_000 energy (above the infinite threshold) — effectively un-emptyable.
            towers: vec![tower(1, 100_000); 6],
            breach_hits: 20_000,
            objective_hits: 80_000,
            ..Default::default()
        };
        let a = assess(&profile, 0.0, &strong_budget());
        assert!(!a.winnable, "an infinite-tower base is NOT drainable by one squad");
        assert_ne!(a.mode, AssaultMode::Drain, "and it is NOT mis-classified as a drain");
        assert!(a.reason.contains("heavy assault"), "it defers to the heavy-assault arm: {}", a.reason);
    }

    /// ADR 0031 §2(g) FOLLOW-UP 2 (FIX A) — a MIXED finite+infinite base is NEVER a drain target: the soak
    /// sizing counts only the finite towers (under-sizing the heal + mis-reading drainability), so any
    /// energized infinite tower REFUSES the drain branch → it falls to the heavy-assault / unwinnable path.
    #[test]
    fn oracle_never_drains_a_mixed_finite_and_infinite_base() {
        // 3 finite (1500) + 1 infinite (100_000) tower nest. With only the 3 finite towers the falloff soak
        // (3×150 = 450) would be sustainable by the budget below — i.e. WITHOUT the mixed guard the drain
        // branch would fire (pre-fix bug). The infinite tower must veto the drain.
        let profile = DefenseProfile {
            towers: vec![tower(1, 1500), tower(1, 1500), tower(1, 1500), tower(1, 100_000)],
            breach_hits: 20_000,
            objective_hits: 30_000,
            ..Default::default()
        };
        let budget = ForceBudget { max_heal_per_tick: 600.0, max_dismantle_dps: 600.0, tank_effective_hp: 20_000.0, onsite_budget_ticks: 1500 };
        let a = assess(&profile, 0.0, &budget);
        assert_ne!(a.mode, AssaultMode::Drain, "a mixed finite+infinite base is NOT mis-classified as a drain: {}", a.reason);
        assert!(!a.winnable, "it falls to the heavy-assault / unwinnable path (the infinite tower can't be bled): {}", a.reason);
        assert!(a.reason.contains("heavy assault"), "it defers to the heavy-assault arm: {}", a.reason);
        // And the comparison case: the SAME 3 finite towers WITHOUT the infinite one DO pick Drain (the guard
        // is what flips the verdict, not the budget).
        let pure_finite = DefenseProfile { towers: vec![tower(1, 1500); 3], ..profile };
        assert_eq!(assess(&pure_finite, 0.0, &budget).mode, AssaultMode::Drain, "the pure-finite nest still drains");
    }

    /// (b) EV GUARD — an UNSUSTAINABLE finite drain (the falloff fire over-runs the tank's HP + heal over the
    /// whole drain) is unwinnable, NOT a drain: a tiny budget can't soak 4 deep-ish finite pools.
    #[test]
    fn oracle_does_not_drain_an_unsustainable_target() {
        let profile = DefenseProfile {
            // 6 finite towers @ 40_000 energy (below infinite, so they're drain CANDIDATES) — but a long drain.
            towers: vec![tower(1, 40_000); 6],
            breach_hits: 20_000,
            objective_hits: 80_000,
            ..Default::default()
        };
        // A FRAGILE budget: low heal + low EHP can't soak 6×150 = 900/tick over the ~4000-tick drain, and the
        // drain is far too slow for one creep lifetime regardless → NOT winnable, NOT a fielded drain.
        let fragile = ForceBudget { max_heal_per_tick: 100.0, max_dismantle_dps: 300.0, tank_effective_hp: 2_000.0, onsite_budget_ticks: 1400 };
        let a = assess(&profile, 0.0, &fragile);
        assert!(!a.winnable, "an unsustainable / too-slow finite drain is unwinnable for one squad: {}", a.reason);
    }

    /// (b) EV GUARD — a WINNING DIRECT BREACH is never downgraded to a slower drain (the breach branch returns
    /// first when the squad can out-heal the towers and clear in time).
    #[test]
    fn oracle_keeps_a_winning_breach_over_a_drain() {
        // One weak finite tower a strong budget out-heals easily → a breach wins; even though the tower is a
        // finite-energy drain CANDIDATE, the breach branch returns first (no downgrade).
        let profile = DefenseProfile { towers: vec![tower(5, 500)], breach_hits: 20_000, objective_hits: 80_000, ..Default::default() };
        let a = assess(&profile, 0.0, &strong_budget());
        assert!(a.winnable);
        assert_eq!(a.mode, AssaultMode::Breach, "a winning breach is NOT downgraded to a drain: {}", a.reason);
        assert_eq!(RequiredForce::from_assessment(&a).tough_parts, 0, "a breach carries no drain EHP buffer");
    }

    /// Determinism fence (ADR 0031 §5): `assess` + the drain part-mapping are pure integer/float folds over
    /// the Vec-ordered towers — run-twice-equal must hold for the drain path too.
    #[test]
    fn drain_assessment_and_sizing_are_deterministic() {
        let profile = DefenseProfile { towers: vec![tower(1, 1500); 4], breach_hits: 20_000, objective_hits: 30_000, ..Default::default() };
        let budget = ForceBudget { max_heal_per_tick: 600.0, max_dismantle_dps: 600.0, tank_effective_hp: 20_000.0, onsite_budget_ticks: 1500 };
        let run = || {
            let a = assess(&profile, 0.0, &budget);
            (a.mode, a.winnable, a.required_heal_per_tick, a.required_tank_hp, RequiredForce::from_assessment(&a))
        };
        assert_eq!(run().0, AssaultMode::Drain, "the drain bed picks Drain");
        assert_eq!(run(), run(), "the drain assess + sizing is deterministic");
    }

    /// P2 sizing seam: `from_assessment` maps a DRAIN assessment's `required_tank_hp` to TOUGH parts (and a
    /// BREACH assessment to NONE — the byte-unchanged non-drain mapping).
    #[test]
    fn from_assessment_sizes_tough_only_for_drain() {
        let drain = ForceAssessment {
            winnable: true,
            mode: AssaultMode::Drain,
            required_heal_per_tick: 300.0,
            required_dismantle_dps: 300.0,
            required_tank_hp: 5_000.0, // 50 TOUGH parts @ 100 HP
            est_ticks: 200,
            reason: "drain",
        };
        assert_eq!(RequiredForce::from_assessment(&drain).tough_parts, 50, "drain EHP buffer → TOUGH parts");
        let breach = ForceAssessment { mode: AssaultMode::Breach, required_tank_hp: 5_000.0, ..drain };
        assert_eq!(RequiredForce::from_assessment(&breach).tough_parts, 0, "a breach maps no TOUGH (byte-unchanged)");
    }

    #[test]
    fn deep_energy_towers_are_unwinnable_for_one_squad() {
        let profile = DefenseProfile {
            towers: vec![tower(1, 100_000); 6],
            breach_hits: 20_000,
            objective_hits: 80_000,
            ..Default::default()
        };
        let a = assess(&profile, 0.0, &strong_budget());
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
        let a = assess(&profile, 0.0, &strong_budget());
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
        let a = assess(&profile, 0.0, &strong_budget());
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
        let a = assess(&repair_locked, 0.0, &strong_budget());
        assert!(!a.winnable);
        assert!(a.reason.contains("repair out-paces"), "reason: {}", a.reason);
        let out_paced = DefenseProfile { repair_per_tick: 200.0, ..repair_locked.clone() };
        assert!(assess(&out_paced, 0.0, &strong_budget()).winnable, "600 dismantle out-paces 200 repair");
    }

    #[test]
    fn undefended_room_is_a_no_breach_win() {
        let profile = DefenseProfile { objective_hits: 50_000, ..Default::default() };
        let a = assess(&profile, 0.0, &strong_budget());
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
        let base = RequiredForce { heal_parts: 10, dismantle_parts: 6, immune_struct_parts: 0, anti_creep_parts: 0, tough_parts: 0, claim_parts: 0 };
        assert_eq!(base.scaled(importance_margin(0.0)), base, "importance 0 → no over-invest");
        assert_eq!(importance_margin(1.0), 1.5);
        let crit = base.scaled(importance_margin(1.0));
        assert_eq!(crit.heal_parts, 15, "10 × 1.5");
        assert_eq!(crit.dismantle_parts, 9, "6 × 1.5");
        assert_eq!(crit.tough_parts, 0, "zero stays zero");
        assert!(crit.heal_parts >= base.heal_parts && base.scaled(0.5) == base, "factor < 1 is clamped to no-op");
    }
}
