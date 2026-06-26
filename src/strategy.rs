//! ADR 0026 — the objective/information-dependent combat **strategy-selection layer**, built as a
//! PLUGGABLE TRAIT registry. Each strategy is an ACTIVATOR (does it apply to this context?) + the weight
//! PROFILE it fields; [`decide_strategy`] takes a COLLECTION of strategies and returns the profile of the
//! first whose activator fires (collection order = priority). Adding or removing a strategy is one entry
//! in the collection — the kernel and the FSM are untouched.
//!
//! This is the strategic layer ABOVE the per-tick kernel: the kernel *prices* outcomes (it already knows
//! structure vs creep via `V_struct`); this layer *picks the position-shaping weights*, so objective
//! semantics never re-enter the kernel's hot loop (ADR 0025 §2 stays intact).
//!
//! Motivated by the ADR 0025 §12 realistic re-tune: no single [`KernelParams`] wins both open-creep
//! combat and structure breaching. The profiles below are the thorough-run winners per lens (48-config ×
//! 56-bed × 52-base sweep): open combat wants low-approach / high-incumbency / tight cohesion (it kites
//! and holds firing tiles); a structure breach wants the same low approach but moderate incumbency /
//! default cohesion so the squad presses through the rampart ring rather than chipping at standoff.

use crate::force_sizing::AssaultMode;
use crate::kernel::KernelParams;
use crate::kite::SquadTacticParams;

/// The strategic objective class the selector keys on — a bot-enum-agnostic projection of the bot's
/// `ObjectiveKind` (the decision crate stays bot/JS-free, so it receives the *class*, not the bot enum).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombatObjectiveClass {
    /// Open-creep combat: clear / deny / defend / harass against creeps — no rampart ring to crack.
    OpenCombat,
    /// Break a defended STRUCTURE objective behind a rampart/wall ring (dismantle / a base raze).
    StructureBreach,
}

/// The information signals an activator reads (all pre-computed bot-side; this crate only reads them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StrategyInfo {
    /// Enemy safe mode active in the target room → zero damage possible (the force-sizing hard veto).
    pub enemy_safe_mode: bool,
    /// The force-sizing oracle's chosen assault mode, when the producer ran it (`None` ⇒ a towered base is
    /// treated as a straight breach).
    pub assault_mode: Option<AssaultMode>,
}

/// What a strategy activator sees: the squad's objective class + the information signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrategyContext {
    pub class: CombatObjectiveClass,
    pub info: StrategyInfo,
}

/// A pluggable combat strategy: a named ACTIVATOR (`applies`) + the weight PROFILE the kernel fights with
/// when it fires. Implement this and add it to a [`decide_strategy`] collection to introduce a strategy;
/// remove the entry to retire it. Pure + deterministic; `Sync` so a collection can be shared across the
/// tournament's parallel matches.
pub trait CombatStrategy: Sync {
    /// A stable identifier (tuning / telemetry).
    fn name(&self) -> &'static str;
    /// Does this strategy apply to `ctx`? (the activator)
    fn applies(&self, ctx: &StrategyContext) -> bool;
    /// The weight profile the kernel fights with under this strategy.
    fn profile(&self) -> SquadTacticParams;
}

// ── the weight profiles (the thorough re-tune winners) ───────────────────────────────────────────────

impl SquadTacticParams {
    /// Open-creep combat profile (`a1-i6-tight-s2`): low approach (kite, don't over-commit), strong
    /// incumbency (hold firing tiles), tight cohesion, **spacing 2** (de-stack to shed focus-fire / RMA).
    /// The original §12 re-tune's grid fixed spacing=1 and crowned `a1-i6-tight` "exploitability 0" — but
    /// that was a BLIND SPOT: Screeps AoE is pure Chebyshev with no LOS, so a tight blob eats stacked
    /// ranged-mass-attack + tower fire. The spacing-aware re-tune (2026-06-26, on the bit-deterministic
    /// sim) shows that once spacing is in the field, the old spacing-1 profile is NEGATIVE-mean and
    /// exploitable (176), while spacing 2 sweeps the top (+103 mean, exploit cut 176→51) and beats the old
    /// profile by ~+305 over the realistic comp basket. Spacing 2 (not 4) is the generic sweet spot;
    /// spacing 4 only wins a pure-ranged mirror (a candidate situational mode — see ADR 0026a).
    pub fn open_combat() -> Self {
        Self {
            kernel: KernelParams { approach_coef: 1, incumbency_coef: 6, discohesion_coef: 20, cohesion_k: 2, spacing_coef: 2 },
            ..Self::default()
        }
    }

    /// Structure-breach profile (`a1-i4-def`). Vs the open profile it keeps the SAME low approach (the
    /// thorough re-tune's headline: at scale, low approach survives — a hot approach over-commits and
    /// bleeds creeps without breaching faster, since a winnable force breaches anyway) but holds LESS
    /// (incumbency 4 not 6) with looser cohesion — so the squad moves IN to range-1 and dismantles the
    /// rampart ring rather than holding a ranged firing tile (the kernel's `V_struct` already pulls toward
    /// the structure; this just stops the squad latching a standoff tile). The profile rests on the
    /// dismantle-needs-range-1 PRINCIPLE + the open-combat win (see the §per-objective gate). NOTE: base-
    /// attack scoring USED to be noise-dominated (~1% cross-process from two seed-ordered hash iterations
    /// in rover's resolver); that is now FIXED — the sim is bit-deterministic (the `sim_is_deterministic`
    /// fence), so a clean base-attack re-tune is now possible should the principle ever need a measured lead.
    pub fn breach() -> Self {
        Self {
            kernel: KernelParams { approach_coef: 1, incumbency_coef: 4, discohesion_coef: 10, cohesion_k: 3, spacing_coef: 1 },
            ..Self::default()
        }
    }

    /// Tower-drain breach profile — when the oracle picks `AssaultMode::Drain` (a tank soaks the towers
    /// dry, then the squad breaches): like the breach profile but hold position LONGER through the soak
    /// (incumbency 6). Seed; tuned separately once a tower-energy-bounded drain scenario lands.
    pub fn breach_drain() -> Self {
        Self {
            kernel: KernelParams { approach_coef: 1, incumbency_coef: 6, discohesion_coef: 10, cohesion_k: 3, spacing_coef: 1 },
            ..Self::default()
        }
    }
}

// ── the standard strategies ──────────────────────────────────────────────────────────────────────────

/// Open-creep combat → kite-and-hold.
pub struct OpenCombat;
impl CombatStrategy for OpenCombat {
    fn name(&self) -> &'static str {
        "open-combat"
    }
    fn applies(&self, ctx: &StrategyContext) -> bool {
        ctx.class == CombatObjectiveClass::OpenCombat
    }
    fn profile(&self) -> SquadTacticParams {
        SquadTacticParams::open_combat()
    }
}

/// VETO: a rampart-shielded base under safe mode takes ZERO damage — never spend approach risk; hold the
/// kite profile until safe mode lapses. (Highest priority — it overrides any breach.)
pub struct SafeModeHold;
impl CombatStrategy for SafeModeHold {
    fn name(&self) -> &'static str {
        "safe-mode-hold"
    }
    fn applies(&self, ctx: &StrategyContext) -> bool {
        ctx.class == CombatObjectiveClass::StructureBreach && ctx.info.enemy_safe_mode
    }
    fn profile(&self) -> SquadTacticParams {
        SquadTacticParams::open_combat()
    }
}

/// A structure breach the oracle classified as a tower-DRAIN → the patient drain profile.
pub struct DrainBreach;
impl CombatStrategy for DrainBreach {
    fn name(&self) -> &'static str {
        "drain-breach"
    }
    fn applies(&self, ctx: &StrategyContext) -> bool {
        ctx.class == CombatObjectiveClass::StructureBreach && matches!(ctx.info.assault_mode, Some(AssaultMode::Drain))
    }
    fn profile(&self) -> SquadTacticParams {
        SquadTacticParams::breach_drain()
    }
}

/// A structure breach (the general case — a straight breach of the rampart ring).
pub struct Breach;
impl CombatStrategy for Breach {
    fn name(&self) -> &'static str {
        "breach"
    }
    fn applies(&self, ctx: &StrategyContext) -> bool {
        ctx.class == CombatObjectiveClass::StructureBreach
    }
    fn profile(&self) -> SquadTacticParams {
        SquadTacticParams::breach()
    }
}

/// The standard ordered strategy registry (priority = order; first activator that fires wins): the
/// safe-mode veto → tower-drain → straight breach → open-combat fallback. Build ONCE and reuse across
/// ticks; add/remove entries to extend or retire strategies.
pub fn default_strategies() -> Vec<Box<dyn CombatStrategy>> {
    vec![Box::new(SafeModeHold), Box::new(DrainBreach), Box::new(Breach), Box::new(OpenCombat)]
}

/// Decide the weight profile: the FIRST strategy in `strategies` whose activator fires (collection order =
/// priority). Falls back to the open-combat profile if nothing matches (defensive — the standard registry
/// always matches). The collection is passed in so callers can extend/restrict/re-order the registry.
pub fn decide_strategy(ctx: &StrategyContext, strategies: &[Box<dyn CombatStrategy>]) -> SquadTacticParams {
    strategies
        .iter()
        .find(|s| s.applies(ctx))
        .map_or_else(SquadTacticParams::open_combat, |s| s.profile())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(class: CombatObjectiveClass, info: StrategyInfo) -> StrategyContext {
        StrategyContext { class, info }
    }

    // SquadTacticParams isn't PartialEq (kite weights are f64), so assert on the selected `.kernel`.
    #[test]
    fn open_combat_objective_picks_the_open_profile() {
        let reg = default_strategies();
        let k = decide_strategy(&ctx(CombatObjectiveClass::OpenCombat, StrategyInfo::default()), &reg).kernel;
        assert_eq!(k, SquadTacticParams::open_combat().kernel);
    }

    #[test]
    fn structure_breach_picks_the_breach_profile() {
        let reg = default_strategies();
        let k = decide_strategy(&ctx(CombatObjectiveClass::StructureBreach, StrategyInfo::default()), &reg).kernel;
        assert_eq!(k, SquadTacticParams::breach().kernel);
        assert_ne!(k, SquadTacticParams::open_combat().kernel);
    }

    #[test]
    fn safe_mode_vetoes_the_breach_no_approach_into_an_invulnerable_base() {
        let reg = default_strategies();
        let info = StrategyInfo { enemy_safe_mode: true, ..Default::default() };
        let k = decide_strategy(&ctx(CombatObjectiveClass::StructureBreach, info), &reg).kernel;
        assert_eq!(k, SquadTacticParams::open_combat().kernel);
    }

    #[test]
    fn drain_mode_picks_the_patient_drain_profile() {
        let reg = default_strategies();
        let info = StrategyInfo { assault_mode: Some(AssaultMode::Drain), ..Default::default() };
        let k = decide_strategy(&ctx(CombatObjectiveClass::StructureBreach, info), &reg).kernel;
        assert_eq!(k, SquadTacticParams::breach_drain().kernel);
    }

    /// The registry is PLUGGABLE: a custom strategy + a custom collection select a custom profile, proving
    /// strategies are added/removed by editing the collection (the operator's extensibility requirement).
    #[test]
    fn registry_is_pluggable_with_custom_strategies() {
        struct AlwaysSpread;
        impl CombatStrategy for AlwaysSpread {
            fn name(&self) -> &'static str {
                "always-spread"
            }
            fn applies(&self, _ctx: &StrategyContext) -> bool {
                true
            }
            fn profile(&self) -> SquadTacticParams {
                SquadTacticParams {
                    kernel: KernelParams { spacing_coef: 9, ..KernelParams::default() },
                    ..SquadTacticParams::default()
                }
            }
        }
        let custom: Vec<Box<dyn CombatStrategy>> = vec![Box::new(AlwaysSpread)];
        let k = decide_strategy(&ctx(CombatObjectiveClass::OpenCombat, StrategyInfo::default()), &custom).kernel;
        assert_eq!(k.spacing_coef, 9, "the injected strategy wins; the registry is the seam to extend");
    }
}
