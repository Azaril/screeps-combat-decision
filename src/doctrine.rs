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
//! **Rung 1 (this module) re-expresses the CURRENT arms as doctrines — behaviorally a no-op.** The
//! `Coordinated` square-law sizing + the `PlayerDefend`/`PlayerRaid` doctrines are rungs 2–3 (ADR 0026
//! §9.7); `GarrisonDefense` (L3) unifies the bot's defender selection onto the registry. The
//! `coordination`/`enemy_force` context fields feed the creep-clear sizing.

use crate::composition::SquadComposition;
use crate::force_sizing::{
    assess, importance_margin, AssaultMode, DefenseProfile, ForceAssessment, ForceBudget, RequiredForce,
};

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
    /// Harass / deny a hostile remote (don't hold).
    Harass,
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

/// Clear hostile creeps (an operator attack flag / secure) → a ranged quad. Rung 1 keeps it UNSIZED (the
/// current hardcoded `AttackFlag` behavior); the sized `Coordinated` `PlayerRaid` is rung 3 (it needs the
/// §12.7(B) creep-target oracle the AttackFlag/Harass deferral, §12.6, called out).
pub struct SecureRoom;
impl ForceDoctrine for SecureRoom {
    fn name(&self) -> &'static str {
        "secure-room"
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

/// Garrison defense (ADR 0026 §9.10 L3) — hold an owned/remote room against a present threat. UNIFIES the
/// bot's former `DefenseEscalation::from_threat` 3-bucket selection onto the registry: selects the defender
/// SHAPE by the threat and fields it spawn-path-sized (no oracle sizing — `clear_force`-based threat-
/// proportional defender sizing is the §9.8/L6 enhancement, which is why this is `is_sized() == false`).
/// Always-fields (you can't skip defending an owned room). The shape thresholds are the §9.8 `defend_size_
/// curve` (L6-tunable); kept as the former `from_threat` constants so the unification is behavior-preserving.
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
        let comp = if (f.boosted && f.dps > 200.0) || (f.heal > 100.0 && f.dps > 150.0) || f.count >= 4 {
            SquadComposition::quad_ranged()
        } else if f.dps > 60.0 || f.heal > 20.0 || f.count >= 2 || f.boosted {
            SquadComposition::duo_attack_heal()
        } else {
            SquadComposition::solo_ranged()
        };
        ForcePlan::fixed(comp)
    }
}

/// The standard OFFENSE doctrine collection (collection order = priority; first activator wins). Rung-1
/// objectives are mutually exclusive (each candidate maps to one [`DoctrineObjective`]), so order is not
/// yet load-bearing; it becomes so when a player room matches more than one (rungs 2–3). Add / retire a
/// doctrine = one entry — the bot and eval are untouched.
pub fn default_doctrines() -> Vec<Box<dyn ForceDoctrine>> {
    vec![
        Box::new(NpcCore),
        Box::new(SiegeBreach),
        Box::new(SecureRoom),
        Box::new(HarassRemote),
    ]
}

/// The DEFENSE doctrine collection — a separate registry so defender selection joins the doctrine layer
/// (L3) without coupling to the offense `ClearCreeps` arm (`SecureRoom`, still rung-1 fixed pending the
/// L4 `PlayerRaid` reachability/operator-intent call). One doctrine today; future turtle/sally variants
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
    fn garrison_defense_selects_shape_by_threat() {
        let docs = defense_doctrines();
        let shape = |force: EnemyForce| {
            let mut c = ctx(DoctrineObjective::ClearCreeps, DefenseProfile::default());
            c.enemy_force = Some(force);
            let doc = decide_doctrine(&c, &docs).expect("ClearCreeps → garrison-defense");
            doc.plan(&c, None).composition.expect("defense always fields").slots.len()
        };
        // Solo (1 slot) for a trivial threat; Duo (2) for moderate; Quad (4) for count ≥ 4 — the former
        // DefenseEscalation::from_threat buckets, now on the registry.
        assert_eq!(shape(EnemyForce { dps: 10.0, heal: 0.0, hits: 100, count: 1, boosted: false }), 1, "solo");
        assert_eq!(shape(EnemyForce { dps: 80.0, heal: 0.0, hits: 2000, count: 2, boosted: false }), 2, "duo");
        assert_eq!(shape(EnemyForce { dps: 50.0, heal: 0.0, hits: 8000, count: 5, boosted: false }), 4, "quad");
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
            (DoctrineObjective::ClearCreeps, "secure-room"),
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
