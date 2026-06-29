//! ADR 0026 §9 / ADR 0031 — the OBJECTIVE + FORCE-COMPOSITION selection layer, a pluggable **doctrine
//! registry** that is the structural twin of the strategy registry ([`crate::strategy`]) one layer up.
//! Each doctrine is a PURE CLASSIFIER (`applies` — the activator) + a few objective-shaping knobs; it
//! carries NO sizing code of its own (D14/D15). [`decide_doctrine`] returns the first doctrine whose
//! activator fires (collection order = priority). Adding / removing a doctrine is one entry — the bot's
//! `war.rs` and the eval are untouched.
//!
//! Pure + host-shared so the BOT (war.rs offense) and the SIM (the eval's force-fielding) select and
//! size compositions through THE SAME code — no divergent inline selection in either (the parity the ADR
//! requires). Each caller projects its world into an [`EngagementContext`] and a winnability ceiling
//! budget ([`ForceBudget`], from [`crate::composition::force_ceiling`]; bot: `best_force_budget` over
//! home rooms, eval: from the scenario), then runs the ONE shared driver [`plan_engagement`]: it emits
//! the capability vector + oracle verdict ([`emit_requirement`]), gates on winnability if the doctrine
//! honors the verdict, and ASSEMBLES the force ([`crate::composition::assemble_force`]). There is no
//! per-doctrine `plan()` and no template — composition is continuous and capability-driven.
//!
//! Doctrines (ADR 0026 §9.5/§9.10) — all sized by the assembler from the emitter's requirement (no
//! per-doctrine templates): OFFENSE (`default_doctrines`) = `NpcCore` (vs a dismantle-immune core,
//! RANGED), `SiegeBreach` (vs a dismantle-able structure ring, WORK), `PlayerRaid` (L4 — clear_force-
//! sized vs a player room's creeps, always-field), `GatedPlayerRaid` (ADR 0029 §7/D7 — the SIZED + GATED
//! resource-denial raid: same clear_force sizing, but DEFERS a hopeless room through the bot's
//! winnability + ROI gate), `HarassRemote` (a DYNAMIC anti-creep deny force scaled to the room's creeps,
//! always-field). DEFENSE (`defense_doctrines`) = `GarrisonDefense` (L3 — clear_force-sized defender).
//! SK (`sk_doctrines`) = `SkSuppression` (L7 — kiting out-heal + kill vs a keeper). The `coordination`/
//! `enemy_force` context fields feed the creep-clear sizing (`clear_force`).

use crate::bodies::defender_heal_parts_for_dps;
use crate::composition::{assemble_force, SquadComposition, SquadRole};
use crate::force_sizing::{
    assess, clear_force, importance_margin, AssaultMode, DefenseProfile, ForceAssessment, ForceBudget, RequiredForce,
};
use screeps_combat_engine::constants::RANGED_ATTACK_POWER;

/// How the opposing force fights — the axis that selects the sizing math (ADR 0026 §9.4, operator
/// 2026-06-26). Rung-1 doctrines are all `Individual`; the `Coordinated` square-law branch is rungs 2–3.
/// The classifier defaults to `Coordinated` UNLESS a positive NPC signal (Q1 confirmed) — the safe
/// (over-spend) side, since under-sizing a real player loses creeps while over-sizing an NPC only spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnemyCoordination {
    /// NPCs (invaders, SK keepers) + scattered defenders: fought ONE AT A TIME → size to the worst single.
    Individual,
    /// A player's creeps fight TOGETHER (focus-fire + mutual heal) → size to the aggregate (square law).
    Coordinated,
}

/// The bot-agnostic objective the doctrine classifier keys on — a projection of the bot's `ObjectiveKind`
/// and the target type. The decision crate stays bot/JS-free, so it receives the CLASS, not the bot enum;
/// the eval projects its scenario objective into the same enum, so the bot and sim classify identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoctrineObjective {
    /// Kill a dismantle-IMMUNE structure (an invader core) — RANGED only (R-attack §12.6).
    KillImmuneStructure,
    /// Breach + dismantle a dismantle-ABLE structure ring (a base raze) — WORK dismantlers.
    DismantleStructure,
    /// Clear hostile CREEPS from a room (an operator attack flag / secure).
    ClearCreeps,
    /// RAID a hostile PLAYER's remote to deny resources — the SIZED + GATED creep-clear (ADR 0029 §7/D7).
    /// Same `clear_force` sizing as `ClearCreeps`, but routed to [`GatedPlayerRaid`], which HONORS the
    /// oracle's unwinnable verdict so the bot's winnability + ROI gate DEFERS a hopeless / unaffordable
    /// room — vs `ClearCreeps`/`PlayerRaid`, which always-field operator intent.
    RaidCreeps,
    /// Harass / deny a hostile remote (don't hold).
    Harass,
    /// Suppress a farmable hazard creep (a Source Keeper) — kite + out-heal + kill, hold the source.
    Suppress,
    /// Neutralize a derelict controller (`attackController`) — ADR 0027 v1.1 P2. Fields a CLAIM declaimer
    /// (undefended by construction; ALWAYS-FIELD — the EV/admit decision stays in `SalvageOperation`).
    Declaim,
}

/// The resolved enemy creep force a creep-clear / defense sizes against (ADR 0026 §9.3/§9.4), derived from
/// OBSERVED bodies — NOT type constants. For an `Individual` fight it is the WORST SINGLE enemy; for a
/// `Coordinated` one, the AGGREGATE. `dps`/`hits`/`heal` drive `force_sizing::clear_force`; `count`/
/// `boosted` drive a defender's SHAPE selection (`GarrisonDefense`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnemyForce {
    pub dps: f32,
    pub heal: f32,
    pub hits: u32,
    pub count: u32,
    pub boosted: bool,
}

/// What a doctrine activator reads: the objective intent + the expected opposing force + the sizing
/// ceiling. Bot-agnostic — the bot and the eval each project their world into this (ADR 0026 §9.3/§9.6).
/// The [`ForceBudget`] is NOT here: the caller derives it from the template-free [`force_ceiling`] (or
/// lets [`plan_engagement`] derive it) and passes it to the driver.
#[derive(Clone, Debug)]
pub struct EngagementContext {
    pub objective: DoctrineObjective,
    pub coordination: EnemyCoordination,
    pub defense: DefenseProfile,
    /// The resolved enemy creep force (single for `Individual`, aggregate for `Coordinated`) — feeds the
    /// creep-clear sizing (`GarrisonDefense` / the future `PlayerDefend`/`PlayerRaid`). `None` for the
    /// structure arms (NpcCore/SiegeBreach), which size from `defense`.
    pub enemy_force: Option<EnemyForce>,
    /// Objective importance ∈ [0,1] → [`importance_margin`] over-investment (0 = base force, no scaling).
    pub importance: f32,
    /// Strongest in-range spawn energy — informational; the optimizer sizes each member to
    /// `params.member_energy`. Kept for callers / logging.
    pub member_energy: u32,
    /// The TARGET VALUE (ADR 0031 D16) — the EV upside of taking this objective. The optimizer maximizes
    /// `EV(C) = P(win)·target_value − cost(C)`; set high enough that "EV > commit" ⇔ "winnable" so a
    /// winnable target is never deferred for a low value (preserving the OracleCalibration FP/FN semantics).
    pub target_value: f32,
    /// On-site window (ticks) the candidate has to deliver its kill — `CREEP_LIFE_TIME − spawn − travel`
    /// (offense) or a defender lifetime (defense). Feeds the optimizer's `deliverable = structure_dps · window`.
    pub onsite_window: u32,
    /// The tournament-tunable optimizer knobs (ADR 0031 D16/D17). [`CompositionParams::default`] reproduces
    /// today's fielding.
    pub params: crate::composition::CompositionParams,
    /// Is the `defense` intel RELIABLE — i.e. a SCOUTED / MAPPED / live-visible room, not merely UNSEEN
    /// (ADR 0031 §2(d))? The caller sets this from the SAME intel-reliability source the rally fix uses
    /// (`CombatIntelSource::is_reliable` — `Cached` mapped `RoomData` OR `LiveVisible`; `None` = no vision).
    /// It is the ONLY new bit threaded for the CONFIRMED-undefended distinction: an empty `defense`
    /// (towers=[], enemy_dps=0) means *genuinely clear* only when this is `true`; when `false` it means
    /// merely *unseen*, so the always-field heal/anti-creep floor is RETAINED (don't field a naked force
    /// into the unknown). Per-tick / computed — NOT serialized (no `WORLD_FORMAT_VERSION` bump). DEFAULT
    /// `false` (the SAFE "not confirmed") for any caller that does not set it.
    pub defense_intel_reliable: bool,
}

impl EngagementContext {
    /// CONFIRMED-undefended (ADR 0031 §2(d)) — the ONE clean predicate that distinguishes a *genuinely
    /// clear* target from an *unscouted/unseen* one, so the always-field doctrine floor can be suppressed
    /// for the former while the hedge holds for the latter. True iff the intel is RELIABLE
    /// ([`defense_intel_reliable`](Self::defense_intel_reliable)) AND the reliable target shows ZERO threat:
    /// no ENERGIZED towers AND no defender DPS — mirroring the no-attrition detector
    /// [`crate::composition::optimize_composition`] uses for its FIX-3 gate (`tower_dps == 0 &&
    /// incoming == 0`). "No defender DPS" reads BOTH channels a caller can carry it in: `defense.enemy_dps`
    /// (the structure arms' folded defender dps) AND the `enemy_force` (the creep-clear arms' observed creep
    /// force) — so a player room with guard creeps but no towers (`defense.enemy_dps == 0` yet
    /// `enemy_force.dps > 0`) is correctly NOT confirmed-undefended (the floor is retained — no regression).
    /// Reliable-but-empty intel is required: an UNSEEN room (`defense_intel_reliable == false`) is never
    /// "confirmed undefended" even with an all-zero target, so the floor's hedge against fielding a naked
    /// force into the unknown is preserved (a strict regression-guard over the prior `incoming == 0` alone).
    pub fn defense_confirmed_undefended(&self) -> bool {
        let enemy_dps = self.enemy_force.map(|e| e.dps).unwrap_or(0.0);
        self.defense_intel_reliable
            && crate::force_sizing::tower_dps_at_assault(&self.defense.towers) == 0.0
            && self.defense.enemy_dps == 0.0
            && enemy_dps == 0.0
    }
}

/// The driver's output: the assembled force + the oracle verdict (ADR 0026 §9.3 / ADR 0031).
#[derive(Clone, Debug)]
pub struct ForcePlan {
    /// The ASSEMBLED composition to field. `None` = defer: the doctrine honors an unwinnable verdict, or
    /// no in-range home affords the required force (the caller skips / defers to a heavier path).
    pub composition: Option<SquadComposition>,
    /// The oracle verdict (winnable / mode / est_ticks / reason).
    pub assessment: ForceAssessment,
    /// The required capability vector the composition was assembled to (for the caller's win-confidence
    /// log); all-zero when the engagement is deferred unwinnable.
    pub required: RequiredForce,
}

impl ForcePlan {
    /// Convenience: did the oracle (or the fixed arm) clear this engagement?
    pub fn winnable(&self) -> bool {
        self.assessment.winnable
    }

}

/// The UNIFIED requirement emitter (ADR 0031 T1) — ONE place that derives the capability vector
/// ([`RequiredForce`]) + the oracle verdict ([`ForceAssessment`]) for an objective, folding the three
/// formerly divergent sizing maths: [`assess`] (structure breach/drain), [`clear_force`] (creep
/// square-law clear), and the SK kite terms. The shared [`plan_engagement`] driver calls this for every
/// objective and feeds the result straight to [`assemble_force`] — there is no per-doctrine sizing fork;
/// the sizing MATH lives HERE (the parity seam the bot's `war.rs` and the eval both run, so they size
/// identically). `budget` is `None` only for
/// the SK kite (it sizes directly from the keeper — no winnability-against-budget check); the structure +
/// creep-clear paths require it.
///
/// Exact-behavior-preserving (P2): the STRUCTURE objectives run `assess`, scale by `importance` (R5), and
/// overlay anti-creep ([`overlay_anti_creep`]) when defenders are observed (`KillImmuneStructure` keeps the
/// RANGED structure-alt `immune_struct_parts`; `DismantleStructure` zeroes it — a dismantle-able ring uses
/// WORK). The CREEP-CLEAR objectives size via `clear_force` at the coordinated over-match (the binding
/// constraint is out-powering the defenders, NOT importance, so they do not scale — preserving the prior
/// per-doctrine behavior); `RaidCreeps` additionally threads `enemy.hits` (the kill-in-time term).
/// `Suppress` sizes the keeper kill window. [`assemble_force`] consumes this capability vector directly.
#[allow(clippy::too_many_arguments)]
pub fn emit_requirement(
    objective: DoctrineObjective,
    defense: &DefenseProfile,
    enemy_force: Option<EnemyForce>,
    budget: Option<&ForceBudget>,
    coordination: EnemyCoordination,
    importance: f32,
    hold_margin: f32,
    over_power_margin: f32,
) -> (ForceAssessment, RequiredForce) {
    match objective {
        DoctrineObjective::KillImmuneStructure | DoctrineObjective::DismantleStructure => {
            let budget = budget.expect("a structure objective must be given a ForceBudget");
            let a = assess(defense, budget);
            if !a.winnable {
                return (a, RequiredForce::default());
            }
            // R5: over-invest by the objective's importance (a no-op at importance 0). The eval passes 0 to
            // match its base-force sizing; the bot passes the objective's priority-derived importance.
            let mut required = RequiredForce::from_assessment(&a).scaled(importance_margin(importance));
            // SELECT the structure weapon (the template used to do this; now the requirement does — D14).
            // `from_assessment` sets BOTH `dismantle_parts` (WORK) and `immune_struct_parts` (RANGED) for the
            // same structure DPS; the assembler would field BOTH, so zero the one this objective can't use.
            match objective {
                DoctrineObjective::DismantleStructure => required.immune_struct_parts = 0, // WORK razes a dismantle-able ring
                DoctrineObjective::KillImmuneStructure => required.dismantle_parts = 0,     // a dismantle-IMMUNE core needs RANGED
                _ => {}
            }
            overlay_anti_creep(&mut required, defense, enemy_force, budget, coordination, over_power_margin);
            (a, required)
        }
        // CREEP-CLEAR: out-heal the incoming (towers + defenders) × HOLD_MARGIN AND out-power the defenders
        // by the coordinated square-law margin. `ClearCreeps` (raid/garrison) sizes to out-power with
        // `hits = 0` (the binding constraint is the dps race, not grinding HP); `RaidCreeps` (the gated
        // resource-denial raid) also threads `enemy.hits` so an HP-rich room is sized to clear in the window.
        DoctrineObjective::ClearCreeps | DoctrineObjective::RaidCreeps => {
            let budget = budget.expect("a creep-clear objective must be given a ForceBudget");
            let f = enemy_force.unwrap_or_default();
            let hits = if matches!(objective, DoctrineObjective::RaidCreeps) { f.hits } else { 0 };
            clear_force(defense.towers.clone(), f.dps, hits, f.heal, budget, over_power_margin, defense.safe_mode)
        }
        // SK SUPPRESSION: kite + out-heal + kill the keeper in the kill window. NOT `clear_force` (kited, no
        // square-law over-power) — heal out-heals the keeper melee × HOLD_MARGIN (a slip recovers, not dies);
        // the ranged kill grinds the keeper's HP (a CREEP → `anti_creep_parts`) over the proven kill window.
        DoctrineObjective::Suppress => {
            let keeper = enemy_force.unwrap_or_default();
            let required = RequiredForce {
                heal_parts: defender_heal_parts_for_dps(keeper.dps * hold_margin, false),
                anti_creep_parts: keeper.hits.div_ceil(SK_KEEPER_KILL_TICKS * RANGED_ATTACK_POWER),
                ..Default::default()
            };
            let assessment = ForceAssessment {
                winnable: true,
                mode: AssaultMode::Breach,
                required_heal_per_tick: keeper.dps * hold_margin,
                required_dismantle_dps: 0.0,
                required_tank_hp: 0.0,
                est_ticks: SK_KEEPER_KILL_TICKS,
                reason: "sk: out-heal + kill the keeper",
            };
            (assessment, required)
        }
        // HARASS / deny a remote (D11): a DYNAMIC anti-creep force scaled to the room's observed creeps +
        // margin — same `clear_force` sizing as a creep-clear (it kills/denies), not a fixed solo. Its
        // distinction is purely TACTICAL (deny-don't-hold: retreat-happy, never gated), handled by the
        // driver's always-field path + the tactics layer. Unscouted (`dps == 0`) → the driver's default floor.
        DoctrineObjective::Harass => {
            let budget = budget.expect("harass needs a ForceBudget (the room-force ceiling)");
            let f = enemy_force.unwrap_or_default();
            clear_force(defense.towers.clone(), f.dps, 0, f.heal, budget, over_power_margin, defense.safe_mode)
        }
        // DECLAIM (ADR 0027 v1.1 P2): field a CLAIM declaimer. A derelict controller is undefended by
        // construction (the salvage mission aborts on re-arm), so there is no winnability search — the
        // requirement is a fixed CLAIM force the assembler fields directly (like `Suppress`, which is also
        // winnable-by-construction). One declaimer is fielded ([`DECLAIM_CLAIM_PARTS`] CLAIM parts); only one
        // `attackController` strike lands per 1000-tick upgrade-block cadence anyway, so a second would idle.
        DoctrineObjective::Declaim => {
            let required = RequiredForce { claim_parts: DECLAIM_CLAIM_PARTS, ..Default::default() };
            let assessment = ForceAssessment {
                winnable: true,
                mode: AssaultMode::Breach,
                required_heal_per_tick: 0.0,
                required_dismantle_dps: 0.0,
                required_tank_hp: 0.0,
                est_ticks: 0,
                reason: "declaim: neutralize the controller (attackController)",
            };
            (assessment, required)
        }
    }
}

/// ADR 0027 v1.1 P2 — CLAIM parts on the single declaimer the `DeclaimAttack` doctrine sizes. A CLAIM body
/// lives ~600 ticks and lands ~one strike before the 1000-tick upgrade-block clears, so a few CLAIM parts buy
/// a meaningful single strike (−300 ttd per CLAIM, engine-mechanics §2.12) without runaway cost. The
/// assembler caps the per-member count to what the home can afford, so this is the DESIRED, not guaranteed,
/// size. (The doctrine sizes a CLAIM squad; the EV/admit gate stays in `SalvageOperation::salvage_worthwhile`.)
pub const DECLAIM_CLAIM_PARTS: u32 = 4;

/// Anti-creep OVERLAY for a STRUCTURE objective (ADR 0031 Layer C): when defenders are OBSERVED, size
/// `anti_creep_parts` (via `clear_force` over `enemy_force`) to KILL them — `assess` only OUT-HEALS them
/// (folds their dps into `incoming`). A force facing a guarded structure needs BOTH the structure weapon
/// AND anti-creep at once (raze the core AND clear the guard), so they stay separate, not `max`-ed; the
/// out-heal is raised to cover the towers AND the defenders. INERT with no defenders (`enemy_force` absent
/// or `dps == 0`), so the creep-free calibration beds are unchanged (the OracleCalibration/SizingWins
/// invariant). `margin` over-powers for a `Coordinated` defender squad (square law), else 1.0.
fn overlay_anti_creep(
    required: &mut RequiredForce,
    defense: &DefenseProfile,
    enemy_force: Option<EnemyForce>,
    budget: &ForceBudget,
    coordination: EnemyCoordination,
    over_power_margin: f32,
) {
    if let Some(enemy) = enemy_force.filter(|e| e.dps > 0.0) {
        let margin = if coordination == EnemyCoordination::Coordinated { over_power_margin } else { 1.0 };
        let (clear, req) = clear_force(defense.towers.clone(), enemy.dps, enemy.hits, enemy.heal, budget, margin, defense.safe_mode);
        if clear.winnable {
            required.anti_creep_parts = req.anti_creep_parts;
            required.heal_parts = required.heal_parts.max(req.heal_parts);
        }
    }
}

/// Parts in the minimal DEFAULT capability floor an always-field doctrine fields when no winnable
/// requirement assembles (D11/D15).
const DEFAULT_FLOOR_PARTS: u32 = 4;

/// The minimal DEFAULT capability floor an always-field doctrine (defense / operator intent / deny) fields
/// when no winnable requirement assembles — an unscouted room (`dps == 0`) or a threat too large to fully
/// out-power. A small balanced force expressed as a capability VECTOR (NOT a named template — D14/D15) that
/// survives to scout + lets the bot re-size as intel arrives (D11). Gated doctrines never reach this (they
/// defer via `None` — D10).
fn default_floor_force() -> RequiredForce {
    RequiredForce { heal_parts: DEFAULT_FLOOR_PARTS, anti_creep_parts: DEFAULT_FLOOR_PARTS, ..Default::default() }
}

/// The SHARED engagement driver (ADR 0031 T3a, D15/D16) — THE one path every fielded combat squad is born
/// through: emit the per-objective requirement (T1, for the win-confidence log + the always-field floor) →
/// run the EV optimizer ([`crate::composition::optimize_composition`], D16) which presumes NO reference
/// squad, searches the over-power / tough ladders, and commits the max-EV candidate (gated doctrines defer
/// when EV ≤ `commit_ev_threshold`). No template, no `sized_for`, no `force_ceiling` budget, no per-doctrine
/// sizing fork. `budget` is an OPTIONAL caller-supplied report budget for the emitted assessment (the
/// optimizer derives its own internal ceiling); `Suppress` is assembled directly (winnable-by-construction +
/// kited, no EV search). A GATED doctrine (`honor_verdict() == true`) defers to `None` when EV-negative (D10); an ALWAYS-FIELD one
/// fields the best assembled force, falling to [`default_floor_force`] when nothing assembles (D11).
pub fn plan_engagement(doctrine: &dyn ForceDoctrine, ctx: &EngagementContext, budget: Option<ForceBudget>) -> ForcePlan {
    // Keep emit_requirement's (assessment, required) for the caller's win-confidence log + the always-field
    // floor; the COMPOSITION is now chosen by the EV optimizer (D16), which presumes no reference squad and
    // commits the max-EV candidate (gated doctrines defer when EV ≤ commit_ev_threshold).
    let params = ctx.params;
    let onsite_window = if ctx.onsite_window > 0 { ctx.onsite_window } else { CLEAR_ONSITE_TICKS };
    // Suppress sizes directly from the keeper (no budget); every other objective needs the ceiling budget —
    // the caller's if given, else the optimizer derives its own internal ceiling.
    let report_budget = budget.or_else(|| {
        (!matches!(ctx.objective, DoctrineObjective::Suppress))
            .then(|| crate::composition::optimizer_ceiling_budget(ctx.objective, params.member_energy, onsite_window))
    });
    let (assessment, mut required) = emit_requirement(
        ctx.objective,
        &ctx.defense,
        ctx.enemy_force,
        report_budget.as_ref(),
        ctx.coordination,
        ctx.importance,
        params.hold_margin,
        params.over_power_margin,
    );

    // Suppress is winnable-by-construction + kited (no EV search needed — the keeper kill is the requirement);
    // assemble it directly so its bursty retreat tuning + always-field floor flow through unchanged.
    if matches!(ctx.objective, DoctrineObjective::Suppress) {
        let retreat = doctrine.retreat_threshold();
        let composition = assemble_force(&required, params.member_energy).map(|mut c| {
            c.retreat_threshold = retreat;
            c
        });
        return ForcePlan { composition, assessment, required };
    }

    // DECLAIM (ADR 0027 v1.1 P2) is winnable-by-construction (an undefended derelict controller) + always-
    // field — assemble the fixed CLAIM force directly (NO EV/optimize search, which only sizes combat parts
    // and would never field a CLAIM body, and NO default combat floor — a declaimer carries CLAIM + MOVE
    // only). Mirrors the `Suppress` direct-assembly path. `None` only when no home affords even one declaimer.
    if matches!(ctx.objective, DoctrineObjective::Declaim) {
        let retreat = doctrine.retreat_threshold();
        let composition = assemble_force(&required, params.member_energy).map(|mut c| {
            c.retreat_threshold = retreat;
            c
        });
        return ForcePlan { composition, assessment, required };
    }

    // An ALWAYS-FIELD doctrine (operator intent / defense / deny) fields AT LEAST the minimal default floor
    // and scales UP with the observed threat (D11). The floor is applied by raising the requirement the
    // optimizer searches over (a max — never below floor); the optimizer then EV-searches the over-power /
    // tough ladders from there.
    //
    // EXCEPTION (ADR 0031 §2(d)): suppress the floor for a CONFIRMED-undefended target — RELIABLE intel
    // showing zero towers + zero defenders. The floor is a HEDGE for an UNSCOUTED room (`dps == 0` because
    // we have no intel → don't field a naked force into the unknown), but on a genuinely-clear room it
    // over-rides the oracle's correct `heal_parts = 0` into ≈2 wasted Healer slots. The floor is RETAINED
    // for an unscouted room (`!defense_confirmed_undefended()`) and for any defended target, so the hedge
    // holds exactly where it is correct.
    let confirmed_undefended = ctx.defense_confirmed_undefended();
    if !doctrine.honor_verdict() && !confirmed_undefended {
        let floor = default_floor_force();
        required.heal_parts = required.heal_parts.max(floor.heal_parts);
        required.anti_creep_parts = required.anti_creep_parts.max(floor.anti_creep_parts);
    }

    let retreat = doctrine.retreat_threshold();
    let composition = crate::composition::optimize_composition(
        ctx.objective,
        &ctx.defense,
        ctx.enemy_force,
        ctx.target_value,
        onsite_window,
        ctx.coordination,
        ctx.importance,
        doctrine.honor_verdict(),
        confirmed_undefended,
        &params,
    )
    // The always-field floor (above) ⇒ the optimizer searches a non-empty requirement; but if it can't field
    // even one member at this energy it returns None. For an always-field doctrine that means assemble the
    // floor directly so we never field nothing (D11).
    .or_else(|| {
        (!doctrine.honor_verdict()).then(|| assemble_force(&required, params.member_energy)).flatten()
    })
    .map(|mut c| {
        c.retreat_threshold = retreat;
        c
    });
    ForcePlan { composition, assessment, required }
}

/// A pluggable engagement doctrine (ADR 0026 §9.3 / ADR 0031 T3a) — now a PURE CLASSIFIER (the twin of
/// [`crate::strategy::CombatStrategy`]). It declares only WHAT objective it handles + the objective-shaping
/// knobs; the shared [`plan_engagement`] driver does ALL sizing + assembly. No `template()`, no `is_sized()`,
/// no per-doctrine `plan()` — those (and the catalogs) are gone (ADR 0031 D4/D7/D14/D15).
pub trait ForceDoctrine: Sync {
    /// A stable identifier (telemetry / tuning).
    fn name(&self) -> &'static str;
    /// Does this doctrine apply to `ctx`? (the activator / classifier)
    fn applies(&self, ctx: &EngagementContext) -> bool;
    /// The kill-weapon role this objective fields: [`SquadRole::Dismantler`] (a dismantle-able structure
    /// ring) or [`SquadRole::RangedDPS`] (an immune core / creep clear / keeper). Selects the winnability
    /// ceiling's fighter ([`force_ceiling`]).
    fn fighter_role(&self) -> SquadRole;
    /// HONOR the oracle's unwinnable verdict — `true` GATES (defer to `None` when unwinnable: gated offense);
    /// `false` ALWAYS-FIELDS (defense / operator intent / deny — fields the best effort + the default floor).
    fn honor_verdict(&self) -> bool;
    /// Per-objective retreat tuning; SK is bursty → higher. Default 0.3.
    fn retreat_threshold(&self) -> f32 {
        0.3
    }
}

// ── the starter doctrine set (ADR 0026 §9.5) — rung 1 re-expresses the current arms ──────────────────

/// Invader core (dismantle-IMMUNE) → oracle-sized RANGED force. `Individual` (one core). The bot's
/// current InvaderCore arm (R-attack §12.6 sizes the ranged kill parts + the out-heal). Safe-mode is
/// handled by the oracle (`assess` returns unwinnable), so no separate veto doctrine is needed.
pub struct NpcCore;
impl ForceDoctrine for NpcCore {
    fn name(&self) -> &'static str {
        "npc-core"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::KillImmuneStructure)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::RangedDPS // a dismantle-immune core is killed by RANGED
    }
    fn honor_verdict(&self) -> bool {
        true // gated offense — defer an unwinnable core
    }
}

/// A dismantle-ABLE structure ring (a base raze) → oracle-sized WORK force. The eval's structure
/// beds + the future G4-HEAVY bot path. `Individual` for an undefended raze; a defended player base is
/// the `Coordinated` `PlayerRaid` (rung 3).
pub struct SiegeBreach;
impl ForceDoctrine for SiegeBreach {
    fn name(&self) -> &'static str {
        "siege-breach"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::DismantleStructure)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::Dismantler // a dismantle-able ring is razed by WORK (the assembler adds RANGED for guards)
    }
    fn honor_verdict(&self) -> bool {
        true // gated offense — defer an unwinnable base
    }
}

/// Clear hostile creeps (an operator attack flag / secure a player room) → a ranged force CLEAR_FORCE-SIZED
/// to OUT-POWER the defenders (ADR 0026 §9.10 L4, the player-offense rung). `Coordinated` (a player squad
/// fights together). **Size-but-ALWAYS-FIELD** (never gate-skip operator intent): with no scouted enemy
/// (`dps == 0` — an unscouted flag room) the driver fields the default floor; with intel `clear_force`
/// sizes the force to out-power + out-heal and the assembler grows the member count to match. `hits = 0`:
/// like defense, the binding constraint is out-powering the incoming dps (a raze targets the creeps first),
/// not grinding HP. The member count emerges continuously (bounded by `MAX_SIZED_MEMBERS`; the multi-squad
/// N-blob escalation is L5). Falls back to the default floor if the sizing can't field (no regression).
pub struct PlayerRaid;
impl ForceDoctrine for PlayerRaid {
    fn name(&self) -> &'static str {
        "player-raid"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::ClearCreeps)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::RangedDPS
    }
    fn honor_verdict(&self) -> bool {
        false // operator intent — always field (sizes to scouted defenders; the default floor when unscouted)
    }
}

/// Resource-denial RAID on a hostile PLAYER's remote (ADR 0029 §7/D7) — the SIZED + GATED twin of
/// [`PlayerRaid`]. It runs the SAME `clear_force` creep-clear sizing (out-power + out-heal the defenders
/// AND their towers), but unlike `PlayerRaid` (always-field operator intent) it HONORS the oracle's
/// verdict: a hopeless room (enemy safe mode / can't out-heal the towers / their heal out-paces a feasible
/// kill) makes [`plan_engagement`] defer to `None`, so the bot's winnability + ROI gate DEFERS it instead
/// of feeding a doomed squad to a tower (the prior solo-harasser death). `honor_verdict() == true` so it
/// flows through the bot's force-budget + winnability + ROI gate (the gated-offense path); the emitter
/// sizes it via creep `clear_force` (not the structure `assess`). `Coordinated` (a player's creeps fight together).
pub struct GatedPlayerRaid;
impl ForceDoctrine for GatedPlayerRaid {
    fn name(&self) -> &'static str {
        "gated-player-raid"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::RaidCreeps)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::RangedDPS
    }
    fn honor_verdict(&self) -> bool {
        true // the GATED resource-denial raid — DEFER a hopeless / unaffordable room (the defer PlayerRaid lacks)
    }
}

/// Harass / deny a hostile remote → a DYNAMIC anti-creep deny force `clear_force`-sized to the room's
/// observed creeps + margin (D11), NOT a fixed solo. LOW priority + always-field (no oracle gate); its
/// distinction is purely TACTICAL (deny-don't-hold, retreat-happy — see `retreat`/tactics), and the
/// member count emerges from the assembler. Unscouted (`dps == 0`) → the driver's default floor.
pub struct HarassRemote;
impl ForceDoctrine for HarassRemote {
    fn name(&self) -> &'static str {
        "harass-remote"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::Harass)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::RangedDPS
    }
    fn honor_verdict(&self) -> bool {
        false // deny-don't-hold — always field, scaled to the room force + margin (D11; tactics keep it retreat-happy)
    }
}

/// De-claim a derelict controller (ADR 0027 v1.1 P2) → field a CLAIM declaimer that `attackController`s the
/// controller to neutral for the waiting mining outpost. ALWAYS-FIELD (`honor_verdict() == false`) — a
/// derelict controller is undefended by construction (the salvage mission aborts the moment the room re-arms),
/// so there is no oracle gate here; the EV/admit decision (is this room worth taking over?) stays in
/// `SalvageOperation::salvage_worthwhile`, and the producer only emits the `Declaim` objective once the
/// corridor is open (`ControllerAccess::ReachableNow`, opened by the P1 breach `Dismantle` squad). The driver
/// assembles the CLAIM force DIRECTLY (the `Declaim` arm in `plan_engagement` — the optimizer only sizes
/// combat parts and would never field a CLAIM body). `Individual` (one controller).
pub struct DeclaimAttack;
impl ForceDoctrine for DeclaimAttack {
    fn name(&self) -> &'static str {
        "declaim-attack"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::Declaim)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::Declaimer // CLAIM neutralizes the controller (not a combat weapon)
    }
    fn honor_verdict(&self) -> bool {
        false // always-field — the EV/admit decision is upstream in SalvageOperation, not the oracle
    }
}

/// Garrison defense (ADR 0026 §9.10 L3, generalized ADR 0029) — hold an owned room against a present threat.
/// Sizes a ranged+heal force CONTINUOUSLY through the force-sizing oracle (`clear_force` → `assemble_force`):
/// the member count emerges from the threat, NOT from fixed buckets. The buckets (the former
/// `DefenseEscalation::from_threat`) straddled a hard threshold on jittery live threat → the committed
/// roster flapped 1↔2 tick-to-tick → wipe. Always-fields (you can't skip defending an owned room) — falls
/// back to the driver's default floor if the oracle can't size.
pub struct GarrisonDefense;
impl ForceDoctrine for GarrisonDefense {
    fn name(&self) -> &'static str {
        "garrison-defense"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::ClearCreeps)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::RangedDPS
    }
    fn honor_verdict(&self) -> bool {
        // ADR 0029 — you can't skip defending an owned room: ALWAYS field. The assembler sizes a CONTINUOUS
        // blob from the threat (member count emerges from `clear_force`'s out-power + out-heal — no fixed
        // buckets to straddle, the W9N8 1↔2 flap is structurally impossible), and the role-set floor +
        // the driver's default floor replace the former hardcoded fallback force.
        false
    }
}

/// A defender's on-site window (≈ a creep lifetime; defense fields in-room, no travel). `clear_force`
/// uses it only for the kill-in-time term, which is inert for defense (`hits = 0`), so the exact value
/// is not load-bearing.
const CLEAR_ONSITE_TICKS: u32 = 1400;

/// The standard OFFENSE doctrine collection (collection order = priority; first activator wins). Rung-1
/// objectives are mutually exclusive (each candidate maps to one [`DoctrineObjective`]), so order is not
/// yet load-bearing; it becomes so when a player room matches more than one (rungs 2–3). Add / retire a
/// doctrine = one entry — the bot and eval are untouched.
pub fn default_doctrines() -> Vec<Box<dyn ForceDoctrine>> {
    vec![
        Box::new(NpcCore),
        Box::new(SiegeBreach),
        Box::new(PlayerRaid),
        Box::new(GatedPlayerRaid),
        Box::new(HarassRemote),
        Box::new(DeclaimAttack),
    ]
}

/// SK kill window: ticks to grind a keeper's HP at the proven full-strength suppression rate (~150 DPS,
/// R6 + R-attack). A keeper does NOT self-heal (engine-fixed body), so net kill == gross ranged DPS.
const SK_KEEPER_KILL_TICKS: u32 = 34;

/// Source-Keeper suppression (ADR 0026 §9.10 L7) — the SK farm's suppression force, UNIFIED onto the
/// registry from the SK mission's former inline sizing. Sizes the HEALER to out-heal the keeper's melee (× `HOLD_MARGIN`, so
/// a kiting slip recovers, not dies) AND the KITER's RANGED to KILL the keeper in the kill window (a dead
/// keeper clears the source for the respawn). `Individual` (one keeper per source) + KITED, so this is NOT
/// `clear_force` — the force kites and out-heals rather than trading blows, so there is no square-law
/// over-power term. The keeper's stats arrive as the observed `enemy_force`. The emitter sizes the heal +
/// kill parts directly from the keeper and the assembler fields the force (member count emerges; falls
/// back to the driver's default floor when no home affords the sized force).
pub struct SkSuppression;
impl ForceDoctrine for SkSuppression {
    fn name(&self) -> &'static str {
        "sk-suppression"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::Suppress)
    }
    fn fighter_role(&self) -> SquadRole {
        SquadRole::RangedDPS // the kiter grinds the keeper (a CREEP) with RANGED
    }
    fn honor_verdict(&self) -> bool {
        false // the SK farm always fields its suppression duo (Suppress is always winnable by construction)
    }
    fn retreat_threshold(&self) -> f32 {
        0.5 // SK damage is bursty — retreat earlier so a kiting slip recovers
    }
}

/// The SK-suppression doctrine collection — the SK farm coordinator's registry (its duo selection joins
/// the doctrine layer; the keeper stats arrive as the engagement's `enemy_force`).
pub fn sk_doctrines() -> Vec<Box<dyn ForceDoctrine>> {
    vec![Box::new(SkSuppression)]
}

/// The DEFENSE doctrine collection — a separate registry so defender selection (`GarrisonDefense`, L3) is
/// distinct from the offense `ClearCreeps` arm (`PlayerRaid`, L4): defense sizes to the threat and holds,
/// a raid sizes to out-power and presses. One doctrine today; future turtle/sally variants
/// add entries here.
pub fn defense_doctrines() -> Vec<Box<dyn ForceDoctrine>> {
    vec![Box::new(GarrisonDefense)]
}

/// First doctrine whose activator fires (collection order = priority) — the twin of `decide_strategy`.
pub fn decide_doctrine<'a>(
    ctx: &EngagementContext,
    doctrines: &'a [Box<dyn ForceDoctrine>],
) -> Option<&'a dyn ForceDoctrine> {
    doctrines.iter().map(|d| d.as_ref()).find(|d| d.applies(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::force_sizing::{TowerThreat, COORDINATED_DPS_MARGIN, HOLD_MARGIN};

    fn ctx(objective: DoctrineObjective, defense: DefenseProfile) -> EngagementContext {
        EngagementContext {
            objective,
            coordination: EnemyCoordination::Individual,
            defense,
            enemy_force: None,
            importance: 0.0,
            member_energy: 5600,
            // High value so "EV > commit" ⇔ "winnable" (a winnable target is never deferred for low value);
            // window = a full creep lifetime; Default optimizer knobs.
            target_value: 100_000.0,
            onsite_window: 1400,
            params: crate::composition::CompositionParams { member_energy: 5600, ..Default::default() },
            // Default to NOT-confirmed (an unseen room) — the SAFE side: the always-field floor is retained
            // unless a test explicitly opts into confirmed-undefended (see the §2(d) suppression tests).
            defense_intel_reliable: false,
        }
    }

    #[test]
    fn sk_suppression_sizes_the_keeper_kill() {
        let docs = sk_doctrines();
        let mut c = ctx(DoctrineObjective::Suppress, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 168.0, heal: 0.0, hits: 5000, count: 1, boosted: false });
        let doc = decide_doctrine(&c, &docs).expect("Suppress → sk-suppression");
        let plan = plan_engagement(doc, &c, None);
        // Behavior-preserving vs the former SK mission sizing: 5000 HP ÷ 34t ÷ 10 = 15 ranged kill parts
        // (R-attack), and HEAL > 0 to out-heal the 168 melee × HOLD_MARGIN (R6).
        assert_eq!(plan.required.anti_creep_parts, 15, "kills the keeper (a creep) in the window");
        assert!(plan.required.heal_parts > 0, "out-heals the keeper melee");
        let comp = plan.composition.expect("always fields the suppression force");
        assert!((comp.retreat_threshold - 0.5).abs() < 1e-6, "SK retreat tuning is layered (bursty → 0.5)");
    }

    /// ADR 0027 v1.1 P2: the `DeclaimAttack` doctrine routes a `Declaim` objective + sizes a CLAIM declaimer
    /// squad — ALWAYS-FIELD (the EV/admit gate is upstream in SalvageOperation), undefended-by-construction
    /// (fields even with no scouted threat), and the assembled body is CLAIM + MOVE only (no combat floor).
    #[test]
    fn declaim_attack_sizes_a_claim_declaimer_and_always_fields() {
        use crate::composition::SquadRole;
        let docs = default_doctrines();
        let c = ctx(DoctrineObjective::Declaim, DefenseProfile::default());
        let doc = decide_doctrine(&c, &docs).expect("Declaim → declaim-attack");
        assert_eq!(doc.name(), "declaim-attack");
        assert!(!doc.honor_verdict(), "declaim always-fields (the gate is upstream in SalvageOperation)");
        let plan = plan_engagement(doc, &c, None);
        assert_eq!(plan.required.claim_parts, super::DECLAIM_CLAIM_PARTS, "the doctrine sizes the CLAIM weapon");
        let comp = plan.composition.expect("a home affords the declaimer (always-field, undefended)");
        // Exactly one declaimer (one strike per 1000-tick cadence), built CLAIM + MOVE — no combat parts.
        assert_eq!(comp.slots.len(), 1, "one declaimer (a second would idle behind the upgrade-block cadence)");
        assert!(comp.slots.iter().all(|s| s.role == SquadRole::Declaimer), "the sole role is Declaimer");
        let body = comp.slots[0].body_type.build_body(c.member_energy, crate::bodies::MoveProfile::Plains).expect("builds");
        assert!(body.iter().any(|&p| p == screeps::Part::Claim), "the declaimer carries CLAIM");
        assert!(body.iter().any(|&p| p == screeps::Part::Move), "and MOVE");
        assert!(
            body.iter().all(|&p| matches!(p, screeps::Part::Claim | screeps::Part::Move)),
            "CLAIM + MOVE only — no combat floor on a declaimer"
        );
    }

    #[test]
    fn player_raid_sizes_when_scouted_and_always_fields() {
        let docs = default_doctrines();
        let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        // Scouted defenders → clear_force-sized RANGED (assembled, no template).
        c.enemy_force = Some(EnemyForce { dps: 150.0, heal: 0.0, hits: 0, count: 3, boosted: false });
        let comp = plan_engagement(decide_doctrine(&c, &docs).unwrap(), &c, None).composition.expect("operator intent always fields");
        assert!(
            comp.slots.iter().any(|s| matches!(s.body_type, crate::composition::BodyType::Sized(spec) if spec.ranged_attack > 0)),
            "raid sized to out-power the defenders"
        );
        // Unscouted (no intel) → STILL fields (always-field operator intent), every member force-Sized — no
        // catalog template anywhere (D14/D15); it sizes up as defense is identified (D11).
        c.enemy_force = Some(EnemyForce::default());
        let comp0 = plan_engagement(decide_doctrine(&c, &docs).unwrap(), &c, None).composition.expect("always fields a force");
        assert!(!comp0.slots.is_empty() && comp0.slots.iter().all(|s| matches!(s.body_type, crate::composition::BodyType::Sized(_))), "unscouted → an assembled (Sized) force, never a hardcoded template");
    }

    #[test]
    fn gated_player_raid_sizes_when_winnable_else_defers() {
        // ADR 0029 §7/D7: the SIZED + GATED resource-denial raid. Unlike the always-field `PlayerRaid`, it
        // HONORS the oracle's verdict so the bot's gate can DEFER a hopeless room.
        let docs = default_doctrines();
        let mut c = ctx(DoctrineObjective::RaidCreeps, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 120.0, heal: 0.0, hits: 0, count: 3, boosted: false });
        let doc = decide_doctrine(&c, &docs).expect("RaidCreeps → gated-player-raid");
        assert_eq!(doc.name(), "gated-player-raid");
        assert!(doc.honor_verdict(), "the gated raid DEFERS a hopeless room (vs always-field PlayerRaid)");
        // Out-powerable defenders → winnable → a clear_force-sized ranged force (NOT deferred).
        let plan = plan_engagement(doc, &c, None);
        assert!(plan.winnable(), "out-powerable defenders are winnable: {}", plan.assessment.reason);
        assert!(plan.composition.is_some(), "sizes a force when affordable");
        assert!(plan.required.anti_creep_parts > 0, "sized the anti-creep kill parts");
        // Enemy safe mode → the oracle defers (the gate the always-field `PlayerRaid` lacks).
        let mut safe = ctx(DoctrineObjective::RaidCreeps, DefenseProfile { safe_mode: true, ..Default::default() });
        safe.enemy_force = c.enemy_force;
        let plan = plan_engagement(doc, &safe, None);
        assert!(!plan.winnable(), "safe mode → defer");
        assert!(plan.composition.is_none(), "deferred → no force fielded (the bot skips)");
    }

    #[test]
    fn garrison_defense_clear_force_sizes_the_defender() {
        // L3b: a strong grouped threat → the defender is assembled to OUT-POWER it (a Sized ranged force).
        let docs = defense_doctrines();
        let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 200.0, heal: 0.0, hits: 0, count: 4, boosted: false });
        let comp = plan_engagement(decide_doctrine(&c, &docs).unwrap(), &c, None).composition.expect("defense always fields");
        assert!(
            comp.slots.iter().any(|s| matches!(s.body_type, crate::composition::BodyType::Sized(spec) if spec.ranged_attack > 0)),
            "defender assembled to over-power the threat"
        );
    }

    #[test]
    fn garrison_defense_sizes_continuously_no_straddle() {
        let docs = defense_doctrines();
        let size = |force: EnemyForce| {
            let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
            c.enemy_force = Some(force);
            let doc = decide_doctrine(&c, &docs).expect("ClearCreeps → garrison-defense");
            plan_engagement(doc, &c, None).composition.expect("defense always fields").slots.len()
        };
        // ADR 0029/0031: no buckets. The member COUNT emerges continuously from the assembler's role-set
        // floor (≥1 fighter + ≥1 healer = 2) grown by the threat — there is no discrete shape to straddle
        // (the W9N8 1↔2 flap is structurally impossible) and the size is MONOTONIC non-decreasing in the threat.
        let trivial = size(EnemyForce { dps: 10.0, heal: 0.0, hits: 100, count: 1, boosted: false });
        let moderate = size(EnemyForce { dps: 80.0, heal: 0.0, hits: 2000, count: 2, boosted: false });
        let strong = size(EnemyForce { dps: 150.0, heal: 30.0, hits: 8000, count: 5, boosted: false });
        assert!((2..=8).contains(&trivial), "defense floors at the role-set minimum (a fighter + a healer = 2): {trivial}");
        assert!(moderate >= trivial, "monotonic non-decreasing: {moderate} >= {trivial}");
        assert!(strong >= moderate, "monotonic non-decreasing: {strong} >= {moderate}");
    }

    /// A winnable core defense: one weak tower, the core's hits, reachable.
    fn core_defense() -> DefenseProfile {
        DefenseProfile {
            towers: vec![TowerThreat { range_to_assault: 15, energy: 200 }],
            breach_hits: 0,
            objective_hits: 100_000,
            enemy_dps: 0.0,
            repair_per_tick: 0.0,
            safe_mode: false,
        }
    }

    #[test]
    fn each_objective_routes_to_its_doctrine() {
        let docs = default_doctrines();
        let cases = [
            (DoctrineObjective::KillImmuneStructure, "npc-core"),
            (DoctrineObjective::DismantleStructure, "siege-breach"),
            (DoctrineObjective::ClearCreeps, "player-raid"),
            (DoctrineObjective::RaidCreeps, "gated-player-raid"),
            (DoctrineObjective::Harass, "harass-remote"),
            (DoctrineObjective::Declaim, "declaim-attack"),
        ];
        for (obj, name) in cases {
            let c = ctx(obj, DefenseProfile::default());
            assert_eq!(decide_doctrine(&c, &docs).map(|d| d.name()), Some(name), "{obj:?}");
        }
    }

    #[test]
    fn npc_core_assembles_a_ranged_force_when_winnable() {
        let docs = default_doctrines();
        let c = ctx(DoctrineObjective::KillImmuneStructure, core_defense());
        let doc = decide_doctrine(&c, &docs).expect("core routes");
        let plan = plan_engagement(doc, &c, None);
        assert!(plan.winnable(), "weak-tower core is winnable: {}", plan.assessment.reason);
        let comp = plan.composition.expect("a home affords it");
        assert!(plan.required.immune_struct_parts > 0, "sized ranged kill parts");
        // The assembled force fields RANGED (the core is dismantle-immune), not WORK.
        assert!(comp.slots.iter().any(|s| s.role == crate::composition::SquadRole::RangedDPS));
    }

    #[test]
    fn safe_mode_core_defers_via_the_oracle() {
        let docs = default_doctrines();
        let mut d = core_defense();
        d.safe_mode = true;
        let c = ctx(DoctrineObjective::KillImmuneStructure, d);
        let doc = decide_doctrine(&c, &docs).expect("core routes");
        let plan = plan_engagement(doc, &c, None);
        assert!(!plan.winnable(), "safe mode → not winnable");
        assert!(plan.composition.is_none(), "gated doctrine defers to None (D10)");
    }

    #[test]
    fn always_field_doctrines_field_even_unscouted() {
        let docs = default_doctrines();
        // honor_verdict == false (operator intent / deny) → field a force even with NO scouted threat.
        for obj in [DoctrineObjective::ClearCreeps, DoctrineObjective::Harass, DoctrineObjective::Declaim] {
            let c = ctx(obj, DefenseProfile::default());
            let doc = decide_doctrine(&c, &docs).expect("routes");
            assert!(!doc.honor_verdict(), "{obj:?} is always-field");
            let plan = plan_engagement(doc, &c, None);
            assert!(plan.composition.is_some(), "{obj:?}: an always-field doctrine fields a force");
        }
    }

    /// Count the Healer slots an assembled composition fields.
    fn healer_slots(comp: &SquadComposition) -> usize {
        comp.slots.iter().filter(|s| s.role == SquadRole::Healer).count()
    }

    /// ADR 0031 §2(d) — the heal RIGHT-SIZING fix. The always-field heal/anti-creep floor is a HEDGE for an
    /// UNSCOUTED room (don't field a naked force into the unknown), but it must NOT over-heal a CONFIRMED-
    /// undefended target (RELIABLE intel showing zero towers + zero defenders), where the Lanchester oracle
    /// already correctly sizes `heal_parts = 0`. The distinction is the SINGLE predicate
    /// [`EngagementContext::defense_confirmed_undefended`] (reliable intel ∧ no towers ∧ no defender dps) —
    /// suppressing on `dps == 0` ALONE would be a regression (it would strip the hedge from unscouted rooms).
    #[test]
    fn confirmed_undefended_suppresses_the_always_field_floor() {
        let docs = default_doctrines();

        // (a) CONFIRMED-undefended (reliable intel, no towers, no defenders) → floor SUPPRESSED → the oracle's
        // heal_parts = 0 survives → NO Healer slot (the live undefended-core over-heal is gone).
        let mut confirmed = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        confirmed.defense_intel_reliable = true; // scouted/mapped — empty defense is genuinely clear
        confirmed.enemy_force = Some(EnemyForce::default()); // reliably zero defenders
        assert!(confirmed.defense_confirmed_undefended(), "reliable + no towers + no defenders ⇒ confirmed-undefended");
        let plan = plan_engagement(decide_doctrine(&confirmed, &docs).unwrap(), &confirmed, None);
        let comp = plan.composition.expect("always-field still fields a force (the kill weapon), just no heal floor");
        assert_eq!(plan.required.heal_parts, 0, "confirmed-undefended ⇒ the heal floor is suppressed (oracle's 0 survives)");
        assert_eq!(healer_slots(&comp), 0, "confirmed-undefended ⇒ NO wasted Healer slot");

        // (b) UNSCOUTED (no/unreliable intel, dps still reads 0 because we have NO vision) → floor RETAINED →
        // the hedge holds (a survivable scout force, never naked). This is the regression-guard: the SAME
        // all-zero defense, but `defense_intel_reliable == false`, MUST keep the floor.
        let mut unscouted = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        unscouted.defense_intel_reliable = false; // no vision — empty is merely unseen, not clear
        unscouted.enemy_force = Some(EnemyForce::default());
        assert!(!unscouted.defense_confirmed_undefended(), "no reliable intel ⇒ NOT confirmed-undefended (even with dps == 0)");
        let plan = plan_engagement(decide_doctrine(&unscouted, &docs).unwrap(), &unscouted, None);
        assert!(plan.required.heal_parts >= DEFAULT_FLOOR_PARTS, "unscouted ⇒ the heal floor is RETAINED (the hedge): {}", plan.required.heal_parts);
        assert!(healer_slots(&plan.composition.expect("unscouted still fields the floor force")) >= 1, "unscouted ⇒ a survivable (healed) scout force, never naked");

        // (c) DEFENDED (reliable intel, but a present creep force) → NOT confirmed-undefended (the
        // `enemy_force` dps channel) → the oracle sizes heal to the threat + the floor is honored UNCHANGED.
        let mut defended = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        defended.defense_intel_reliable = true; // we DO see the room — and it has defenders
        defended.enemy_force = Some(EnemyForce { dps: 150.0, heal: 0.0, hits: 0, count: 3, boosted: false });
        assert!(!defended.defense_confirmed_undefended(), "a present defender force ⇒ NOT confirmed-undefended (no regression)");
        let plan = plan_engagement(decide_doctrine(&defended, &docs).unwrap(), &defended, None);
        assert!(plan.required.heal_parts >= DEFAULT_FLOOR_PARTS, "defended ⇒ heal sized to the threat (≥ floor)");
        assert!(healer_slots(&plan.composition.expect("defended fields a healed force")) >= 1, "defended ⇒ a healed force");
    }

    /// (d) GarrisonDefense on a real/unknown threat is UNCHANGED — a defense caller carries the threat in
    /// `enemy_force` (NOT `defense`) and leaves `defense_intel_reliable` at the SAFE default (false), so it is
    /// never confirmed-undefended and the floor is always retained (the §2(d) guarantee).
    #[test]
    fn garrison_defense_floor_is_unchanged_by_the_fix() {
        let docs = defense_doctrines();
        // A present threat in enemy_force, an empty `defense` (the defense path's shape), default intel-flag.
        let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 80.0, heal: 0.0, hits: 0, count: 2, boosted: false });
        assert!(!c.defense_confirmed_undefended(), "defense vs a present threat is never confirmed-undefended");
        let plan = plan_engagement(decide_doctrine(&c, &docs).unwrap(), &c, None);
        assert!(healer_slots(&plan.composition.expect("defense always fields")) >= 1, "GarrisonDefense keeps a healed defender (unchanged)");

        // Even an UNSCOUTED defend-flag room (enemy_force None, default intel-flag) keeps the floor.
        let mut unscouted = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        unscouted.enemy_force = None;
        assert!(!unscouted.defense_confirmed_undefended(), "no enemy_force + default (unreliable) intel ⇒ floor retained");
        let plan = plan_engagement(decide_doctrine(&unscouted, &docs).unwrap(), &unscouted, None);
        assert!(plan.required.heal_parts >= DEFAULT_FLOOR_PARTS, "unscouted defense keeps the floor");
    }

    /// ADR 0031 P2 determinism fence: the unified [`emit_requirement`] is a pure fold over Vec-ordered
    /// inputs, so run-twice-equal must hold for EVERY objective (the emitter is the shared sizing path
    /// every objective now flows through). (ADR 0031 §5.)
    #[test]
    fn emit_requirement_is_deterministic_over_objectives() {
        let defense = DefenseProfile {
            towers: vec![TowerThreat { range_to_assault: 10, energy: 1000 }],
            breach_hits: 20_000,
            objective_hits: 100_000,
            enemy_dps: 120.0,
            repair_per_tick: 50.0,
            safe_mode: false,
        };
        let enemy = Some(EnemyForce { dps: 120.0, heal: 20.0, hits: 4000, count: 3, boosted: false });
        let budget = crate::composition::optimizer_ceiling_budget(DoctrineObjective::KillImmuneStructure, 5600, 1400);
        for obj in [
            DoctrineObjective::KillImmuneStructure,
            DoctrineObjective::DismantleStructure,
            DoctrineObjective::ClearCreeps,
            DoctrineObjective::RaidCreeps,
            DoctrineObjective::Suppress,
            DoctrineObjective::Harass,
            DoctrineObjective::Declaim,
        ] {
            let run = || emit_requirement(obj, &defense, enemy, Some(&budget), EnemyCoordination::Coordinated, 0.5, HOLD_MARGIN, COORDINATED_DPS_MARGIN);
            assert_eq!(run(), run(), "{obj:?}: the emitter is deterministic");
        }
    }

    /// ADR 0031 P2: the emitter must reproduce each doctrine's prior per-objective sizing semantics — the
    /// structure paths size the structure weapon + an anti-creep OVERLAY when defenders are present
    /// (`KillImmuneStructure` keeps the RANGED immune-alt; `DismantleStructure` zeroes it), the creep-clear
    /// paths produce anti-creep only, and a creep-free structure bed is left unperturbed (the calibration
    /// invariant). This pins the consolidation contract at the unit level (the eval golden-output over
    /// `realistic_bases()` pins it at the bed level).
    #[test]
    fn emit_requirement_reproduces_per_objective_semantics() {
        // A guard but NO towers, so the anti-creep overlay's `clear_force` is out-heal-feasible against the
        // siege budget (towers + a guard exceed the siege ceiling's heal — that's a correct defer, tested elsewhere).
        let guarded = DefenseProfile { towers: vec![], breach_hits: 10_000, objective_hits: 100_000, enemy_dps: 90.0, repair_per_tick: 0.0, safe_mode: false };
        let undefended = DefenseProfile { breach_hits: 10_000, objective_hits: 100_000, ..Default::default() };
        let guard = Some(EnemyForce { dps: 90.0, heal: 0.0, hits: 3000, count: 2, boosted: false });
        let budget = crate::composition::optimizer_ceiling_budget(DoctrineObjective::DismantleStructure, 5600, 1400);

        // DismantleStructure vs a guard: WORK to raze (dismantle_parts), anti-creep to clear the guard, and
        // the RANGED immune-alt is zeroed (a dismantle-able ring uses WORK).
        let (a, dr) = emit_requirement(DoctrineObjective::DismantleStructure, &guarded, guard, Some(&budget), EnemyCoordination::Coordinated, 0.0, HOLD_MARGIN, COORDINATED_DPS_MARGIN);
        assert!(a.winnable && dr.dismantle_parts > 0 && dr.anti_creep_parts > 0 && dr.immune_struct_parts == 0, "siege vs guard: WORK + anti-creep, no immune-alt: {dr:?}");

        // KillImmuneStructure vs a guard: the RANGED immune-alt is KEPT + anti-creep is added (both feed the
        // ranged role's sum) — the core needs ranged AND the guard needs killing.
        let (_, kr) = emit_requirement(DoctrineObjective::KillImmuneStructure, &guarded, guard, Some(&budget), EnemyCoordination::Coordinated, 0.0, HOLD_MARGIN, COORDINATED_DPS_MARGIN);
        assert!(kr.immune_struct_parts > 0 && kr.anti_creep_parts > 0, "immune core vs guard: ranged immune-alt + anti-creep: {kr:?}");

        // A creep-free structure bed is UNPERTURBED (no anti-creep overlay) — the OracleCalibration invariant.
        let (_, clean) = emit_requirement(DoctrineObjective::DismantleStructure, &undefended, None, Some(&budget), EnemyCoordination::Individual, 0.0, HOLD_MARGIN, COORDINATED_DPS_MARGIN);
        assert_eq!(clean.anti_creep_parts, 0, "no defenders → no anti-creep (calibration beds unperturbed)");

        // Creep-clear produces ANTI-CREEP only (no structure weapon).
        let (_, cc) = emit_requirement(DoctrineObjective::ClearCreeps, &DefenseProfile::default(), guard, Some(&budget), EnemyCoordination::Coordinated, 0.0, HOLD_MARGIN, COORDINATED_DPS_MARGIN);
        assert!(cc.anti_creep_parts > 0 && cc.dismantle_parts == 0 && cc.immune_struct_parts == 0, "clear → anti-creep only: {cc:?}");
    }
}
