//! Pure energy-equivalent objective valuation (`value_e`) — the cross-goal EV currency ADR 0020 §11
//! deferred and ADR 0032 makes concrete. Every combat objective (defense / farm / denial) is scored in ONE
//! comparable unit (energy-equivalent), so the auction (ADR 0032 v1.1 per-squad EV, v1.2 global Hungarian)
//! can weigh "defend the base" against "farm a core" against "deny a player's remote" on a single axis —
//! today it cannot (defense uses a `DEFENSE_TARGET_VALUE = 1_000_000` sentinel; offense uses
//! `score · OFFENSE_TARGET_VALUE_SCALE`; ADR 0032 lines 39-53).
//!
//! THE dps=0 FIX (ADR 0032 line 46, the operator-named defect): a DEFENSE objective's value SCALES with the
//! threat's DANGER — `asset_value · risk(threat_danger)`. A harmless intruder (a scout in an owned room, dps
//! 0) yields a LOW `value_e`, so the EV-positive gate (ADR 0032 §EV-positive gate) does NOT pull a CRITICAL
//! defender for it. A genuinely dangerous (high-dps) threat keeps a high `value_e` and still fields a
//! defender. The old path read only the asset (`DEFENSE_TARGET_VALUE` flat) → a dps=0 scout in an owned room
//! triggered a CRITICAL over-response.
//!
//! Bit-deterministic: pure scalar arithmetic over the [`ObjectiveIntel`] facts, no `HashMap`, no `game::*`.
//! The bot projects its `ObjectiveKind` + the room intel into [`ObjectiveValueKind`] + [`ObjectiveIntel`]
//! (parity with how `DoctrineObjective` is projected); the eval/harness builds synthetic ones.

/// The bot-agnostic objective CLASS the valuation keys on — a projection of the bot's `ObjectiveKind` +
/// `FarmKind`. The decision crate stays bot/JS-free, so it receives the class, not the bot enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectiveValueKind {
    /// Defend/Secure an owned (or threat-centric neighbour) room — value scales with the THREAT's danger.
    Defend,
    /// Farm a level-0 invader core (NPC-reserving our remote) — value = the denied-reservation income.
    FarmCore,
    /// Farm a Source-Keeper room — value = the SK net energy over the horizon minus suppression upkeep.
    FarmSourceKeeper,
    /// Farm a power bank — value = the existing (already energy-equivalent) ROI estimate.
    FarmPowerBank,
    /// Dismantle a blocking structure / Harass a hostile remote — value = the resource denial (discounted).
    Denial,
}

/// The per-objective intel the valuation reads — the bot folds it from `RoomThreatData` / the room intel /
/// the farm estimate (the only non-pure step); the harness supplies synthetic facts. All fields default to
/// 0, so a caller fills only the ones its kind uses.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ObjectiveIntel {
    /// DEFEND: the owned room's asset/replacement value (RCL/structure weight, energy-equivalent). The
    /// value FLOOR a base under attack is worth defending at — scaled by `risk(threat_danger)`.
    pub asset_value: f32,
    /// DEFEND: the threat's estimated DPS (the `war_decision::estimate_danger` fold). Drives `risk(...)`:
    /// dps 0 (a harmless scout) → near-zero risk → LOW value_e (the dps=0 over-response fix); a high-dps
    /// assault → risk → 1 → the full asset value.
    pub threat_danger: f32,
    /// FARM: energy/tick the farm yields (denied-reservation income for a core; SK net energy/tick;
    /// power-bank power/tick). Multiplied by `horizon` for the total energy-equivalent upside.
    pub income_per_tick: f32,
    /// FARM: the horizon (ticks) the income accrues over (the farm's productive window, or the downtime a
    /// defended asset would otherwise lose). 0 ⇒ no horizon → no farm value.
    pub horizon: f32,
    /// FARM{SourceKeeper}: the suppression UPKEEP energy/tick (the cost of holding the keepers down),
    /// subtracted from the SK income before the horizon multiply.
    pub upkeep_per_tick: f32,
    /// FARM{PowerBank}: the existing ROI estimate (already energy-equivalent — passed straight through).
    pub roi: f32,
    /// DENIAL: the raw resource-denial value (the enemy income/asset we deny), before the strategic discount.
    pub denial_value: f32,
}

/// The strategic discount on resource-DENIAL value (Dismantle/Harass): denying an enemy a resource is worth
/// LESS to us than the same resource in hand (it's an opportunity cost on them, not income for us). A
/// fraction in (0, 1]; ADR 0032 line 50 ("resource denial × strategic discount").
pub const DENIAL_DISCOUNT: f32 = 0.5;

/// The danger scale (DPS) at which a DEFEND threat's `risk(...)` reaches ~half the asset value — a threat at
/// this DPS is "a real assault." Below it the value tapers toward 0 (a harmless scout), above it toward the
/// full asset value. Tuned so a single ATTACK creep (30 dps) already reads as a substantial fraction.
pub const DEFENSE_DANGER_HALF: f32 = 30.0;

/// DEFENSE risk multiplier ∈ [0, 1): how much of the asset value a threat of `danger` DPS puts at risk. A
/// saturating curve `danger / (danger + HALF)`: 0 at dps 0 (a harmless scout — the over-response fix), 0.5
/// at `DEFENSE_DANGER_HALF`, → 1 for an overwhelming assault. Monotone in danger, bounded, deterministic.
pub fn defense_risk(threat_danger: f32) -> f32 {
    let d = threat_danger.max(0.0);
    if d <= 0.0 {
        return 0.0;
    }
    d / (d + DEFENSE_DANGER_HALF)
}

/// THE pure energy-equivalent valuation (ADR 0032). One comparable currency for every objective kind:
/// - **Defend** — `asset_value · defense_risk(threat_danger)`. A dps=0 harmless threat → ~0 (the over-
///   response fix); a dangerous one → up to the full asset value.
/// - **FarmCore** — the denied-reservation income recovered = `income_per_tick · horizon`.
/// - **FarmSourceKeeper** — `(income_per_tick − upkeep_per_tick) · horizon`, floored at 0 (a net-negative SK
///   farm is worth nothing).
/// - **FarmPowerBank** — the existing `roi` (already energy-equivalent), passed straight through.
/// - **Denial** (Dismantle/Harass) — `denial_value · DENIAL_DISCOUNT`.
///
/// Always finite + non-negative. Bit-deterministic (scalar arithmetic, no `HashMap`).
pub fn value_e(kind: ObjectiveValueKind, intel: &ObjectiveIntel) -> f32 {
    let v = match kind {
        ObjectiveValueKind::Defend => intel.asset_value * defense_risk(intel.threat_danger),
        ObjectiveValueKind::FarmCore => intel.income_per_tick.max(0.0) * intel.horizon.max(0.0),
        ObjectiveValueKind::FarmSourceKeeper => {
            ((intel.income_per_tick - intel.upkeep_per_tick) * intel.horizon.max(0.0)).max(0.0)
        }
        ObjectiveValueKind::FarmPowerBank => intel.roi.max(0.0),
        ObjectiveValueKind::Denial => intel.denial_value.max(0.0) * DENIAL_DISCOUNT,
    };
    v.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE dps=0 FIX (ADR 0032 line 46): a harmless threat (dps 0 — a scout in an owned room) yields a LOW
    /// (zero) defense value, so the EV gate does NOT pull a CRITICAL defender for it; a genuinely dangerous
    /// (high-dps) threat yields a HIGH value (up to the full asset) so a real assault still fields one.
    #[test]
    fn defend_value_scales_with_threat_danger_fixing_dps0_over_response() {
        let asset = 1_000_000.0;
        let harmless = value_e(ObjectiveValueKind::Defend, &ObjectiveIntel { asset_value: asset, threat_danger: 0.0, ..Default::default() });
        assert_eq!(harmless, 0.0, "a dps=0 harmless threat is worth ~nothing to defend (the over-response fix)");

        let dangerous = value_e(ObjectiveValueKind::Defend, &ObjectiveIntel { asset_value: asset, threat_danger: 300.0, ..Default::default() });
        assert!(dangerous > asset * 0.8, "a high-dps assault puts most of the asset at risk: {dangerous}");

        // Monotone: more danger → more value, never less.
        let mild = value_e(ObjectiveValueKind::Defend, &ObjectiveIntel { asset_value: asset, threat_danger: 30.0, ..Default::default() });
        assert!(harmless < mild && mild < dangerous, "value_e is monotone in danger ({harmless} < {mild} < {dangerous})");
    }

    /// FarmCore = the denied-reservation income recovered (income/tick × horizon).
    #[test]
    fn farm_core_is_denied_reservation_income() {
        let v = value_e(ObjectiveValueKind::FarmCore, &ObjectiveIntel { income_per_tick: 10.0, horizon: 1500.0, ..Default::default() });
        assert_eq!(v, 15_000.0);
    }

    /// FarmSourceKeeper = net energy (income − upkeep) × horizon, floored at 0 (a net-negative SK is worth 0).
    #[test]
    fn farm_sk_is_net_energy_floored() {
        let net = value_e(ObjectiveValueKind::FarmSourceKeeper, &ObjectiveIntel { income_per_tick: 30.0, upkeep_per_tick: 10.0, horizon: 1000.0, ..Default::default() });
        assert_eq!(net, 20_000.0, "(30 − 10) × 1000");
        let negative = value_e(ObjectiveValueKind::FarmSourceKeeper, &ObjectiveIntel { income_per_tick: 5.0, upkeep_per_tick: 30.0, horizon: 1000.0, ..Default::default() });
        assert_eq!(negative, 0.0, "a net-negative SK farm is worth nothing");
    }

    /// FarmPowerBank passes the existing (already energy-equivalent) ROI straight through.
    #[test]
    fn farm_power_bank_is_existing_roi() {
        let v = value_e(ObjectiveValueKind::FarmPowerBank, &ObjectiveIntel { roi: 42_000.0, ..Default::default() });
        assert_eq!(v, 42_000.0);
    }

    /// Denial (Dismantle/Harass) = the raw denial value × the strategic discount.
    #[test]
    fn denial_is_discounted() {
        let v = value_e(ObjectiveValueKind::Denial, &ObjectiveIntel { denial_value: 1000.0, ..Default::default() });
        assert_eq!(v, 1000.0 * DENIAL_DISCOUNT);
    }

    /// (d) Reach-bug #3 / ADR 0032 §economic-value-unlocked — DEFENSE STAYS DOMINANT: a high-value Defend
    /// (a real RCL8-magnitude asset under a genuine assault) out-values a healthy remote economic target
    /// (a reservable lvl0-core remote, priced via the FarmCore economic arm at its net-ROI). The economic-
    /// value-unlocked fix lifts a winnable core from ~0 to its real net-ROI, but must NOT let a remote
    /// out-bid defending the base.
    #[test]
    fn high_value_defend_out_ranks_a_remote_economic_target() {
        // A genuine assault on a substantial base (RCL8-ish asset, a real attacking force).
        let defend = value_e(
            ObjectiveValueKind::Defend,
            &ObjectiveIntel { asset_value: 1_000_000.0, threat_danger: 300.0, ..Default::default() },
        );
        // A healthy reservable remote core, priced economically: ~7 net e/t × a 1500-tick horizon ≈ 10.5k.
        let remote_economy = value_e(
            ObjectiveValueKind::FarmCore,
            &ObjectiveIntel { income_per_tick: 7.0, horizon: 1500.0, ..Default::default() },
        );
        assert!(remote_economy > 5_000.0, "the economic value is healthy (the fix), got {remote_economy}");
        assert!(
            defend > remote_economy,
            "defense stays dominant: defend ({defend}) > remote economy ({remote_economy})"
        );
    }

    /// value_e is always finite + non-negative + deterministic.
    #[test]
    fn value_e_is_nonnegative_and_deterministic() {
        let intel = ObjectiveIntel { asset_value: 500.0, threat_danger: 90.0, income_per_tick: 7.0, horizon: 200.0, upkeep_per_tick: 1.0, roi: 9.0, denial_value: 3.0 };
        for kind in [
            ObjectiveValueKind::Defend,
            ObjectiveValueKind::FarmCore,
            ObjectiveValueKind::FarmSourceKeeper,
            ObjectiveValueKind::FarmPowerBank,
            ObjectiveValueKind::Denial,
        ] {
            let a = value_e(kind, &intel);
            assert!(a >= 0.0 && a.is_finite(), "{kind:?} → {a}");
            assert_eq!(a, value_e(kind, &intel), "{kind:?} deterministic");
        }
    }
}
