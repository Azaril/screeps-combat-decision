//! ADR 0026 §9 — the OBJECTIVE + FORCE-COMPOSITION selection layer, a pluggable **doctrine registry**
//! that is the structural twin of the strategy registry ([`crate::strategy`]) one layer up: each
//! doctrine is a named ACTIVATOR (`applies` — the classifier) + a `plan` that returns the objective's
//! sized force. [`decide_doctrine`] returns the first doctrine whose activator fires (collection order =
//! priority). Adding / removing a doctrine is one entry — the bot's `war.rs` and the eval are untouched.
//!
//! Pure + host-shared so the BOT (war.rs offense) and the SIM (the eval's force-fielding) select and
//! size compositions through THE SAME code — no divergent inline selection in either (the parity the ADR
//! requires). Each caller projects its world into an [`EngagementContext`] and computes the
//! [`ForceBudget`] for the doctrine's template (bot: `best_force_budget` over home rooms; eval: from the
//! scenario); the doctrine runs the force-sizing oracle ([`crate::force_sizing`]) and hands back a sized
//! [`SquadComposition`]. The budget computation is itself shared via [`SquadComposition::force_budget`].
//!
//! Doctrines (ADR 0026 §9.5/§9.10): OFFENSE (`default_doctrines`) = `NpcCore` (sized ranged quad vs a
//! dismantle-immune core), `SiegeBreach` (sized siege quad vs a structure ring), `PlayerRaid` (L4 —
//! clear_force-sized ranged quad vs a player room's creeps, always-field), `GatedPlayerRaid` (ADR 0029
//! §7/D7 — the SIZED + GATED resource-denial raid: same clear_force sizing, but DEFERS a hopeless room
//! through the bot's winnability + ROI gate), `HarassRemote` (fixed solo).
//! DEFENSE (`defense_doctrines`) = `GarrisonDefense` (L3 — clear_force-sized defender). SK
//! (`sk_doctrines`) = `SkSuppression` (L7 — sized kiting duo vs a keeper). The `coordination`/
//! `enemy_force` context fields feed the creep-clear sizing (`clear_force`).

use crate::bodies::defender_heal_parts_for_dps;
use crate::composition::SquadComposition;
use crate::force_sizing::{
    assess, clear_force, importance_margin, AssaultMode, DefenseProfile, ForceAssessment, ForceBudget, RequiredForce, COORDINATED_DPS_MARGIN, HOLD_MARGIN,
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
/// The [`ForceBudget`] is NOT here: it is template-specific, so the caller computes it for the chosen
/// doctrine's `template()` and passes it to [`ForceDoctrine::plan`].
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
    /// Strongest in-range spawn energy — the sizing ceiling passed to [`SquadComposition::sized_for`].
    pub member_energy: u32,
}

/// The doctrine's output: the sized force + the oracle verdict (ADR 0026 §9.3).
#[derive(Clone, Debug)]
pub struct ForcePlan {
    /// The SIZED composition to field. `None` = defer: unwinnable, or no in-range home affords the
    /// required force (the caller skips / defers to a heavier path).
    pub composition: Option<SquadComposition>,
    /// The oracle verdict (winnable / mode / est_ticks / reason) — `winnable: true` + a "no oracle gate"
    /// reason for a FIXED (unsized) doctrine.
    pub assessment: ForceAssessment,
    /// The required parts the sized composition was built to (for the caller's win-confidence log);
    /// all-zero for a fixed doctrine.
    pub required: RequiredForce,
}

impl ForcePlan {
    /// Convenience: did the oracle (or the fixed arm) clear this engagement?
    pub fn winnable(&self) -> bool {
        self.assessment.winnable
    }

    fn skip(assessment: ForceAssessment) -> Self {
        ForcePlan { composition: None, assessment, required: RequiredForce::default() }
    }

    /// An UNSIZED plan — field the template as-is (the current hardcoded arms: harass solo, secure quad).
    /// No oracle gate (preserves the current unconditional behavior of those arms).
    fn fixed(comp: SquadComposition) -> Self {
        ForcePlan {
            composition: Some(comp),
            assessment: ForceAssessment {
                winnable: true,
                mode: AssaultMode::Breach,
                required_heal_per_tick: 0.0,
                required_dismantle_dps: 0.0,
                est_ticks: 0,
                reason: "fixed composition (no oracle gate)",
            },
            required: RequiredForce::default(),
        }
    }
}

/// Run the force-sizing oracle for a structure objective + size `template` to it. The SHARED sizing path
/// that the bot's InvaderCore arm and the eval's structure beds both used inline — now ONE place (the
/// parity seam). `budget` is the caller-computed [`ForceBudget`] for `template` (bot: over home rooms;
/// eval: from the scenario). `None` composition ⇒ no in-range home affords the required force ⇒ defer.
fn sized_plan(ctx: &EngagementContext, budget: &ForceBudget, template: SquadComposition) -> ForcePlan {
    let a = assess(&ctx.defense, budget);
    if !a.winnable {
        return ForcePlan::skip(a);
    }
    // R5: over-invest by the objective's importance (a no-op at importance 0). The eval passes 0 to match
    // its current base-force sizing; the bot passes the objective's priority-derived importance.
    let required = RequiredForce::from_assessment(&a).scaled(importance_margin(ctx.importance));
    let composition = template.sized_for(required, ctx.member_energy);
    ForcePlan { composition, assessment: a, required }
}

/// A pluggable engagement doctrine (ADR 0026 §9.3) — the twin of [`crate::strategy::CombatStrategy`].
/// A doctrine is `applies` (the classifier) + a `template` it fields, sized or fixed. The default `plan`
/// runs the shared oracle for a sized doctrine and fields the template as-is for a fixed one, so a
/// doctrine normally only implements `name`/`applies`/`template`/`is_sized`.
pub trait ForceDoctrine: Sync {
    /// A stable identifier (telemetry / tuning).
    fn name(&self) -> &'static str;
    /// Does this doctrine apply to `ctx`? (the activator / classifier)
    fn applies(&self, ctx: &EngagementContext) -> bool;
    /// The base composition this doctrine fields. A sized doctrine sizes this to the oracle's required
    /// force; a fixed doctrine fields it as-is. The caller computes the [`ForceBudget`] for this template
    /// when `is_sized()`.
    fn template(&self) -> SquadComposition;
    /// Whether to run the force-sizing oracle (size `template` to the defense) or field it fixed.
    fn is_sized(&self) -> bool;
    /// Plan the engagement. `budget` is the caller-computed force budget for `template()`, present iff
    /// `is_sized()`; a sized doctrine assesses + sizes with it, a fixed doctrine ignores it.
    fn plan(&self, ctx: &EngagementContext, budget: Option<ForceBudget>) -> ForcePlan {
        if self.is_sized() {
            let budget = budget.expect("a sized doctrine must be given a ForceBudget");
            sized_plan(ctx, &budget, self.template())
        } else {
            ForcePlan::fixed(self.template())
        }
    }
}

// ── the starter doctrine set (ADR 0026 §9.5) — rung 1 re-expresses the current arms ──────────────────

/// Invader core (dismantle-IMMUNE) → oracle-sized RANGED quad. `Individual` (one core). The bot's
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
    fn template(&self) -> SquadComposition {
        SquadComposition::quad_ranged()
    }
    fn is_sized(&self) -> bool {
        true
    }
}

/// A dismantle-ABLE structure ring (a base raze) → oracle-sized WORK siege quad. The eval's structure
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
    fn template(&self) -> SquadComposition {
        SquadComposition::siege_quad()
    }
    fn is_sized(&self) -> bool {
        true
    }
}

/// Clear hostile creeps (an operator attack flag / secure a player room) → a ranged quad CLEAR_FORCE-SIZED
/// to OUT-POWER the defenders (ADR 0026 §9.10 L4, the player-offense rung). `Coordinated` (a player squad
/// fights together). **Size-but-ALWAYS-FIELD** (never gate-skip operator intent): with no scouted enemy
/// (`dps == 0` — an unscouted flag room) it fields the default quad — byte-identical to the prior
/// `SecureRoom` behavior, so the rung is a no-op until the flag room's intel is wired in; with intel it
/// sizes the quad to out-power + out-heal. `hits = 0`: like defense, the binding constraint is out-powering
/// the incoming dps (a raze targets the creeps first), not grinding HP. Quad-capped (the N-blob escalation
/// is L5). Falls back to the default quad if the sizing can't field (no regression).
pub struct PlayerRaid;
impl ForceDoctrine for PlayerRaid {
    fn name(&self) -> &'static str {
        "player-raid"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::ClearCreeps)
    }
    fn template(&self) -> SquadComposition {
        SquadComposition::quad_ranged()
    }
    fn is_sized(&self) -> bool {
        false // custom clear_force sizing (below), always-field; not the generic budget/assess path
    }
    fn plan(&self, ctx: &EngagementContext, _budget: Option<ForceBudget>) -> ForcePlan {
        let f = ctx.enemy_force.unwrap_or_default();
        // No scouted defenders → the default quad (the prior SecureRoom behavior; operator intent fields
        // regardless). Keeps the rung a no-op until the flag room's intel is wired in.
        if f.dps <= 0.0 {
            return ForcePlan::fixed(SquadComposition::quad_ranged());
        }
        let quad = SquadComposition::quad_ranged();
        let budget = quad.force_budget(ctx.member_energy, CLEAR_ONSITE_TICKS);
        let (_, required) = clear_force(ctx.defense.towers.clone(), f.dps, 0, f.heal, &budget, COORDINATED_DPS_MARGIN, ctx.defense.safe_mode);
        let comp = quad.sized_for(required, ctx.member_energy).unwrap_or_else(SquadComposition::quad_ranged);
        ForcePlan::fixed(comp)
    }
}

/// Resource-denial RAID on a hostile PLAYER's remote (ADR 0029 §7/D7) — the SIZED + GATED twin of
/// [`PlayerRaid`]. It runs the SAME `clear_force` creep-clear sizing (out-power + out-heal the defenders
/// AND their towers), but unlike `PlayerRaid` (always-field operator intent) it HONORS the oracle's
/// verdict: a hopeless room (enemy safe mode / can't out-heal the towers / their heal out-paces a feasible
/// kill) returns [`ForcePlan::skip`], so the bot's winnability + ROI gate DEFERS it instead of feeding a
/// doomed squad to a tower (the prior solo-harasser death). `is_sized() == true` so it flows through the
/// bot's force-budget + winnability + ROI gate (the structural arms' path); the custom `plan` swaps the
/// structure `assess` for creep `clear_force`. `Coordinated` (a player's creeps fight together).
pub struct GatedPlayerRaid;
impl ForceDoctrine for GatedPlayerRaid {
    fn name(&self) -> &'static str {
        "gated-player-raid"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::RaidCreeps)
    }
    fn template(&self) -> SquadComposition {
        SquadComposition::quad_ranged()
    }
    fn is_sized(&self) -> bool {
        true // routes through the bot's force-budget + winnability + ROI gate (defer on unwinnable)
    }
    fn plan(&self, ctx: &EngagementContext, budget: Option<ForceBudget>) -> ForcePlan {
        let budget = budget.expect("a sized doctrine must be given a ForceBudget");
        let f = ctx.enemy_force.unwrap_or_default();
        // Creep-clear sizing (NOT the structure `assess` the generic sized path runs): out-heal the towers
        // AND the defenders' dps with the hold margin, and out-power their dps by the square-law margin.
        // HONOR the verdict — unwinnable ⇒ skip ⇒ the bot's gate DEFERS (the defer the always-field
        // `PlayerRaid` lacks). `enemy_force.hits` is 0 from the bot today (the binding constraint is out-
        // powering the defenders, not grinding HP), so this sizes to out-power; passing `f.hits` keeps it
        // forward-compatible with the eval's `clear_outcome_at` if real hits are wired in.
        let (assessment, required) = clear_force(
            ctx.defense.towers.clone(),
            f.dps,
            f.hits,
            f.heal,
            &budget,
            COORDINATED_DPS_MARGIN,
            ctx.defense.safe_mode,
        );
        if !assessment.winnable {
            return ForcePlan::skip(assessment);
        }
        let composition = SquadComposition::quad_ranged().sized_for(required, ctx.member_energy);
        ForcePlan { composition, assessment, required }
    }
}

/// Harass / deny a hostile remote → a throwaway solo. LOW priority, no oracle gate (current behavior).
pub struct HarassRemote;
impl ForceDoctrine for HarassRemote {
    fn name(&self) -> &'static str {
        "harass-remote"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::Harass)
    }
    fn template(&self) -> SquadComposition {
        SquadComposition::solo_harasser()
    }
    fn is_sized(&self) -> bool {
        false
    }
}

/// Garrison defense (ADR 0026 §9.10 L3, generalized ADR 0029) — hold an owned room against a present threat.
/// Sizes a single ranged+heal base (`quad_ranged`) CONTINUOUSLY through the force-sizing oracle
/// (`clear_force` → `sized_for`): the member count emerges from the threat, NOT from solo/duo/quad buckets.
/// The buckets (the former `DefenseEscalation::from_threat`) straddled a hard threshold on jittery live
/// threat → the committed roster flapped 1↔2 tick-to-tick → wipe (see `plan`). Always-fields (you can't skip
/// defending an owned room) — falls back to the bare base if the oracle can't size. `is_sized() == false`
/// only because it runs `clear_force` (creep-clear) in its own `plan`, not the structure-assess default.
pub struct GarrisonDefense;
impl ForceDoctrine for GarrisonDefense {
    fn name(&self) -> &'static str {
        "garrison-defense"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::ClearCreeps)
    }
    fn template(&self) -> SquadComposition {
        SquadComposition::quad_ranged()
    }
    fn is_sized(&self) -> bool {
        false
    }
    fn plan(&self, ctx: &EngagementContext, _budget: Option<ForceBudget>) -> ForcePlan {
        let f = ctx.enemy_force.unwrap_or_default();
        // ADR 0029 — GENERALIZE defense onto the oracle: size a single ranged+heal BASE into a CONTINUOUS
        // blob; the member COUNT emerges from `clear_force`'s required out-power + out-heal, NOT from
        // solo/duo/quad buckets. The buckets straddled a HARD threshold (dps>60 / count>=2 / …) on jittery
        // live threat → the committed roster flapped 1↔2 tick-to-tick → on a `requested=1` tick the lone
        // creep departed into the defended room and was wiped → re-field → churn (the live W9N8 stuck-at-1/1
        // + wipe). A continuous size cannot straddle. `clear_force` out-heals the incoming AND out-powers it
        // (Coordinated square-law), `hits=0` (defense has no kill-deadline). Needs a REAL `member_energy`
        // (the defense scan now passes the defended room's spawn capacity, not 0, so `sized_for` actually
        // sizes instead of silently falling back to the bare template). BUDGET from `quad_ranged` (large
        // enough that clear_force sizes a STRONG threat — a smaller base's budget trips the can't-out-heal
        // guard → an under-strength default), but FLOOR from the smaller `duo_attack_heal` so a TRIVIAL
        // threat doesn't over-spawn to 4. Decoupling the floor from the budget is the ADR 0029 forming-
        // completion fix: the 4-member floor × N contested rooms saturated the spawn lanes so no roster ever
        // completed. sized_for grows the duo floor for real threats; the manager deploys it immediately (FIX A).
        let budget = SquadComposition::quad_ranged().force_budget(ctx.member_energy, CLEAR_ONSITE_TICKS);
        let (_, required) = clear_force(vec![], f.dps, 0, f.heal, &budget, COORDINATED_DPS_MARGIN, false);
        let floor = SquadComposition::duo_attack_heal();
        let comp = floor.sized_for(required, ctx.member_energy).unwrap_or(floor);
        ForcePlan::fixed(comp)
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
    ]
}

/// SK kill window: ticks to grind a keeper's HP at the proven full-template suppression rate (~150 DPS,
/// R6 + R-attack). A keeper does NOT self-heal (engine-fixed body), so net kill == gross ranged DPS.
const SK_KEEPER_KILL_TICKS: u32 = 34;

/// Source-Keeper suppression (ADR 0026 §9.10 L7) — the SK farm's duo, UNIFIED onto the registry from the
/// SK mission's former inline sizing. Sizes the HEALER to out-heal the keeper's melee (× `HOLD_MARGIN`, so
/// a kiting slip recovers, not dies) AND the KITER's RANGED to KILL the keeper in the kill window (a dead
/// keeper clears the source for the respawn). `Individual` (one keeper per source) + KITED, so this is NOT
/// `clear_force` — the duo kites and out-heals rather than trading blows, so there is no square-law
/// over-power term. The keeper's stats arrive as the observed `enemy_force`. Sizes `duo_sk_farmer`, falling
/// back to the template when no home affords the sized duo (behavior-identical to the prior SK sizing).
pub struct SkSuppression;
impl ForceDoctrine for SkSuppression {
    fn name(&self) -> &'static str {
        "sk-suppression"
    }
    fn applies(&self, ctx: &EngagementContext) -> bool {
        matches!(ctx.objective, DoctrineObjective::Suppress)
    }
    fn template(&self) -> SquadComposition {
        SquadComposition::duo_sk_farmer()
    }
    fn is_sized(&self) -> bool {
        false // a custom kiting-suppression sizing (below), not the generic budget/assess path
    }
    fn plan(&self, ctx: &EngagementContext, _budget: Option<ForceBudget>) -> ForcePlan {
        let keeper = ctx.enemy_force.unwrap_or_default();
        let required = RequiredForce {
            heal_parts: defender_heal_parts_for_dps(keeper.dps * HOLD_MARGIN, false),
            ranged_parts: keeper.hits.div_ceil(SK_KEEPER_KILL_TICKS * RANGED_ATTACK_POWER),
            ..Default::default()
        };
        let duo = SquadComposition::duo_sk_farmer();
        let composition = Some(duo.sized_for(required, ctx.member_energy).unwrap_or_else(SquadComposition::duo_sk_farmer));
        ForcePlan {
            composition,
            assessment: ForceAssessment {
                winnable: true,
                mode: AssaultMode::Breach,
                required_heal_per_tick: keeper.dps * HOLD_MARGIN,
                required_dismantle_dps: 0.0,
                est_ticks: SK_KEEPER_KILL_TICKS,
                reason: "sk: out-heal + kill the keeper",
            },
            required,
        }
    }
}

/// The SK-suppression doctrine collection — the SK farm coordinator's registry (its duo selection joins
/// the doctrine layer; the keeper stats arrive as the engagement's `enemy_force`).
pub fn sk_doctrines() -> Vec<Box<dyn ForceDoctrine>> {
    vec![Box::new(SkSuppression)]
}

/// The DEFENSE doctrine collection — a separate registry so defender selection (`GarrisonDefense`, L3) is
/// distinct from the offense `ClearCreeps` arm (`PlayerRaid`, L4): defense picks the shape by threat and
/// holds, a raid sizes a quad to out-power and presses. One doctrine today; future turtle/sally variants
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
    use crate::force_sizing::TowerThreat;

    fn ctx(objective: DoctrineObjective, defense: DefenseProfile) -> EngagementContext {
        EngagementContext {
            objective,
            coordination: EnemyCoordination::Individual,
            defense,
            enemy_force: None,
            importance: 0.0,
            member_energy: 5600,
        }
    }

    #[test]
    fn sk_suppression_sizes_the_keeper_kill() {
        let docs = sk_doctrines();
        let mut c = ctx(DoctrineObjective::Suppress, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 168.0, heal: 0.0, hits: 5000, count: 1, boosted: false });
        let doc = decide_doctrine(&c, &docs).expect("Suppress → sk-suppression");
        let plan = doc.plan(&c, None);
        // Behavior-preserving vs the former SK mission sizing: 5000 HP ÷ 34t ÷ 10 = 15 ranged kill parts
        // (R-attack), and HEAL > 0 to out-heal the 168 melee × HOLD_MARGIN (R6).
        assert_eq!(plan.required.ranged_parts, 15, "kills the keeper in the window");
        assert!(plan.required.heal_parts > 0, "out-heals the keeper melee");
        assert!(plan.composition.is_some(), "always fields the duo");
    }

    #[test]
    fn player_raid_sizes_when_scouted_else_default_quad() {
        let docs = default_doctrines();
        let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        // Scouted player defenders → clear_force-sized to out-power (a Sized ranged body).
        c.enemy_force = Some(EnemyForce { dps: 150.0, heal: 0.0, hits: 0, count: 3, boosted: false });
        let comp = decide_doctrine(&c, &docs).unwrap().plan(&c, None).composition.expect("always fields");
        assert!(
            comp.slots.iter().any(|s| matches!(s.body_type, crate::composition::BodyType::Sized(spec) if spec.ranged_attack > 0)),
            "raid clear_force-sized to out-power the defenders"
        );
        // Unscouted flag room (no intel) → the default quad, no-op (the prior SecureRoom behavior).
        c.enemy_force = Some(EnemyForce::default());
        let comp0 = decide_doctrine(&c, &docs).unwrap().plan(&c, None).composition.expect("always fields");
        assert!(
            comp0.slots.iter().all(|s| !matches!(s.body_type, crate::composition::BodyType::Sized(_))),
            "unscouted → default quad (operator intent fields regardless)"
        );
    }

    #[test]
    fn gated_player_raid_sizes_when_winnable_else_defers() {
        // ADR 0029 §7/D7: the SIZED + GATED resource-denial raid. Unlike the always-field `PlayerRaid`, it
        // HONORS `clear_force`'s verdict so the bot's gate can DEFER a hopeless room.
        let docs = default_doctrines();
        let mut c = ctx(DoctrineObjective::RaidCreeps, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 120.0, heal: 0.0, hits: 0, count: 3, boosted: false });
        let doc = decide_doctrine(&c, &docs).expect("RaidCreeps → gated-player-raid");
        assert_eq!(doc.name(), "gated-player-raid");
        assert!(doc.is_sized(), "flows through the bot's force-budget + winnability + ROI gate");
        let budget = doc.template().force_budget(c.member_energy, 1400);
        // Out-powerable defenders → winnable → a clear_force-sized ranged force (NOT deferred).
        let plan = doc.plan(&c, Some(budget.clone()));
        assert!(plan.winnable(), "out-powerable defenders are winnable: {}", plan.assessment.reason);
        assert!(plan.composition.is_some(), "sizes a force when affordable");
        assert!(plan.required.ranged_parts > 0, "sized the ranged kill parts");
        // Enemy safe mode → the oracle defers (the gate the always-field `PlayerRaid` lacks).
        let mut safe = ctx(DoctrineObjective::RaidCreeps, DefenseProfile { safe_mode: true, ..Default::default() });
        safe.enemy_force = c.enemy_force;
        let plan = doc.plan(&safe, Some(budget));
        assert!(!plan.winnable(), "safe mode → defer");
        assert!(plan.composition.is_none(), "deferred → no force fielded (the bot skips)");
    }

    #[test]
    fn garrison_defense_clear_force_sizes_the_defender() {
        // L3b: a strong grouped threat → the quad's parts are clear_force-sized to OUT-POWER it (a Sized
        // body with ranged), not the bare spawn-path template.
        let docs = defense_doctrines();
        let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
        c.enemy_force = Some(EnemyForce { dps: 200.0, heal: 0.0, hits: 0, count: 4, boosted: false });
        let comp = decide_doctrine(&c, &docs).unwrap().plan(&c, None).composition.expect("defense always fields");
        assert!(
            comp.slots.iter().any(|s| matches!(s.body_type, crate::composition::BodyType::Sized(spec) if spec.ranged_attack > 0)),
            "defender clear_force-sized to over-power the threat"
        );
    }

    #[test]
    fn garrison_defense_sizes_continuously_no_straddle() {
        let docs = defense_doctrines();
        let size = |force: EnemyForce| {
            let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
            c.enemy_force = Some(force);
            let doc = decide_doctrine(&c, &docs).expect("ClearCreeps → garrison-defense");
            doc.plan(&c, None).composition.expect("defense always fields").slots.len()
        };
        // ADR 0029: no buckets, and the floor is DECOUPLED from the budget (forming-completion fix). A
        // trivial threat floors at the small `duo_attack_heal` (2) — NOT the over-spending quad (4) that ×N
        // contested rooms saturated the spawn lanes — yet is sized via the quad BUDGET so a strong threat
        // still grows. No discrete shape to straddle (the W9N8 1↔2 flap is structurally impossible), and the
        // size is MONOTONIC non-decreasing in the threat (what the hard-threshold buckets violated).
        let trivial = size(EnemyForce { dps: 10.0, heal: 0.0, hits: 100, count: 1, boosted: false });
        let moderate = size(EnemyForce { dps: 80.0, heal: 0.0, hits: 2000, count: 2, boosted: false });
        let strong = size(EnemyForce { dps: 150.0, heal: 30.0, hits: 8000, count: 5, boosted: false });
        assert!((2..=8).contains(&trivial), "defense floors at the small duo (2), not the over-spending quad: {trivial}");
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

    fn budget_for(d: &dyn ForceDoctrine, energy: u32) -> Option<ForceBudget> {
        d.is_sized().then(|| d.template().force_budget(energy, 1400))
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
        ];
        for (obj, name) in cases {
            let c = ctx(obj, DefenseProfile::default());
            assert_eq!(decide_doctrine(&c, &docs).map(|d| d.name()), Some(name), "{obj:?}");
        }
    }

    #[test]
    fn npc_core_sizes_a_ranged_quad_when_winnable() {
        let docs = default_doctrines();
        let c = ctx(DoctrineObjective::KillImmuneStructure, core_defense());
        let doc = decide_doctrine(&c, &docs).expect("core routes");
        let plan = doc.plan(&c, budget_for(doc, c.member_energy));
        assert!(plan.winnable(), "weak-tower core is winnable: {}", plan.assessment.reason);
        let comp = plan.composition.expect("a home affords it");
        assert!(plan.required.ranged_parts > 0, "sized ranged kill parts");
        // The sized quad fields RANGED (the core is dismantle-immune), not WORK.
        assert!(comp.slots.iter().any(|s| s.role == crate::composition::SquadRole::RangedDPS));
    }

    #[test]
    fn safe_mode_core_defers_via_the_oracle() {
        let docs = default_doctrines();
        let mut d = core_defense();
        d.safe_mode = true;
        let c = ctx(DoctrineObjective::KillImmuneStructure, d);
        let doc = decide_doctrine(&c, &docs).expect("core routes");
        let plan = doc.plan(&c, budget_for(doc, c.member_energy));
        assert!(!plan.winnable(), "safe mode → not winnable");
        assert!(plan.composition.is_none());
    }

    #[test]
    fn fixed_arms_field_their_template_unconditionally() {
        let docs = default_doctrines();
        for (obj, sized) in [(DoctrineObjective::ClearCreeps, false), (DoctrineObjective::Harass, false)] {
            let c = ctx(obj, DefenseProfile::default());
            let doc = decide_doctrine(&c, &docs).expect("routes");
            assert_eq!(doc.is_sized(), sized);
            let plan = doc.plan(&c, None);
            assert!(plan.winnable() && plan.composition.is_some(), "fixed arm fields its template");
        }
    }
}
