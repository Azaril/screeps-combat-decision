//! ADR 0025 — the unified **EV-of-(position × action)** combat kernel.
//!
//! The whole per-creep combat decision collapses to one idea: a creep's value at a tile **is** the
//! expected value of the best ENGINE-LEGAL set of intents it could actually fire from that tile this
//! tick, priced in a single win-probability currency (`mhp`) and netted against the incoming-damage
//! risk at the tile. Position and action are one argmax over one currency — no role archetype, no
//! per-role desired-range, no claim-priority ordering (ADR 0025).
//!
//! This module holds the two correctness-critical foundations:
//! 1. **The currency** ([`win_permille`] + [`sensitivities`]): the exchange rate between *removing
//!    enemy fighting strength* (offense/denial) and *preserving ours* (heal/survival), derived from the
//!    Lanchester margin `μ` the engage gate already computes — so there are no new hand-tuned weights,
//!    only the curve shape (tournament-tunable).
//! 2. **The engine-legal action menu** ([`enumerate_legal_sets`]): the fixed, tiny set of intent combos
//!    a creep's capabilities permit that the real engine will NOT drop (ADR 0025 §3, verified against
//!    `screeps-engine/src/processor/intents/creeps/intents.js`).
//!
//! The per-tile pricing, residual ledgers, and the squad commit/drain loop build on these (subsequent
//! ADR-0025 build steps).

/// What an actor (creep) CAN DO this tick, from its working PARTS — the input to [`enumerate_legal_sets`].
/// This replaces the role archetype: behavior derives from the union of capabilities, not a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ActorCaps {
    /// Has a working ATTACK part (melee `attack`, creep or structure, range 1).
    pub melee: bool,
    /// Has a working RANGED_ATTACK part (`rangedAttack` / `rangedMassAttack`, range ≤ 3).
    pub ranged: bool,
    /// Has a working HEAL part (`heal` range 1 / `rangedHeal` range ≤ 3).
    pub heal: bool,
    /// Has a working WORK part (`dismantle` a structure, range 1, 2× structure damage).
    pub dismantle: bool,
    /// Has a working CLAIM part (`attackController` to downgrade/neutralize, range 1).
    pub claim: bool,
}

/// One emittable combat intent, abstracted for enumeration + EV pricing (target resolution happens in
/// pricing, not here). The engine treats melee `attack` on a creep vs a structure as the SAME `attack`
/// intent with the same conflicts, so [`Act::Melee`] covers both; likewise [`Act::Ranged`] covers
/// `rangedAttack` on a creep or a structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Act {
    /// `attack` — melee, range 1 (creep or structure).
    Melee,
    /// `rangedAttack` — single-target, range ≤ 3 (creep or structure).
    Ranged,
    /// `rangedMassAttack` — area, range ≤ 3.
    Rma,
    /// `dismantle` — structure, range 1 (WORK).
    Dismantle,
    /// `attackController` — neutralize a controller, range 1 (CLAIM).
    Declaim,
    /// `heal` — ally (incl. self), range 1.
    Heal,
    /// `rangedHeal` — ally, range 2..=3.
    RangedHeal,
}

impl Act {
    /// The higher-priority intents that DROP this one when also queued (ADR 0025 §3, the combat subset of
    /// `intents.js` `priorities`). An action survives a tick iff NONE of these is also queued.
    fn dropped_by(self) -> &'static [Act] {
        match self {
            // `attack` is dropped by dismantle, attackController, rangedHeal, heal.
            Act::Melee => &[Act::Dismantle, Act::Declaim, Act::RangedHeal, Act::Heal],
            // `rangedAttack` is dropped by rangedMassAttack, rangedHeal. (NOT by plain heal.)
            Act::Ranged => &[Act::Rma, Act::RangedHeal],
            // `rangedMassAttack` is dropped by rangedHeal. (NOT by plain heal.)
            Act::Rma => &[Act::RangedHeal],
            // `dismantle` is dropped by attackController, rangedHeal, heal.
            Act::Dismantle => &[Act::Declaim, Act::RangedHeal, Act::Heal],
            // `attackController` is dropped by rangedHeal, heal.
            Act::Declaim => &[Act::RangedHeal, Act::Heal],
            // `rangedHeal` is dropped by heal.
            Act::RangedHeal => &[Act::Heal],
            // `heal` is the top of the conflict chains — never dropped within the combat subset.
            Act::Heal => &[],
        }
    }
}

/// True iff every action in `set` survives the engine's priority table (no member is dropped by another).
/// Used as a self-check on [`enumerate_legal_sets`] and as a debug assertion at emit time.
pub fn is_engine_legal(set: &[Act]) -> bool {
    set.iter().all(|a| a.dropped_by().iter().all(|c| !set.contains(c)))
}

/// Every ENGINE-LEGAL combat action-set a creep with `caps` could emit this tick (ADR 0025 §3). A tiny
/// fixed menu, NOT a powerset: a "ground" action (one of `attackController` > `dismantle` > melee `attack`
/// — mutually exclusive by priority, enumerated as alternatives) × a ranged action (`rangedAttack` xor
/// `rangedMassAttack`) × a heal flavour, with the illegal pairings pruned. Always includes the empty set
/// (idle / pure-move). Order is deterministic. Pricing later picks the argmax set per tile.
///
/// The legality structure (no-heal vs heal vs rangedHeal):
/// - **No heal:** any ground (`Melee`/`Dismantle`/`Declaim` or none) composes with any ranged
///   (`Ranged`/`Rma` or none) — none of those pairs conflict.
/// - **`heal`:** drops every ground action (melee/dismantle/declaim) but composes with ranged offense
///   (plain `heal` is not in `rangedAttack`/`rangedMassAttack`'s conflict lists).
/// - **`rangedHeal`:** drops the ground action AND all ranged offense → it is emitted solo.
pub fn enumerate_legal_sets(caps: ActorCaps) -> Vec<Vec<Act>> {
    // Ground alternatives (mutually exclusive by the engine priority chain; we offer each as a choice).
    let mut grounds: Vec<Option<Act>> = vec![None];
    if caps.claim {
        grounds.push(Some(Act::Declaim));
    }
    if caps.dismantle {
        grounds.push(Some(Act::Dismantle));
    }
    if caps.melee {
        grounds.push(Some(Act::Melee));
    }
    // Ranged alternatives (single-target xor mass; or none).
    let mut rangeds: Vec<Option<Act>> = vec![None];
    if caps.ranged {
        rangeds.push(Some(Act::Ranged));
        rangeds.push(Some(Act::Rma));
    }

    let mut out: Vec<Vec<Act>> = Vec::new();
    // No-heal combos: ground × ranged (the empty/empty case is the idle set, kept once).
    for &g in &grounds {
        for &r in &rangeds {
            let set: Vec<Act> = g.into_iter().chain(r).collect();
            out.push(set);
        }
    }
    // Heal combos: plain `heal` composes with ranged offense (no ground); `rangedHeal` is solo.
    if caps.heal {
        for &r in &rangeds {
            let set: Vec<Act> = std::iter::once(Act::Heal).chain(r).collect();
            out.push(set);
        }
        out.push(vec![Act::RangedHeal]);
    }

    debug_assert!(out.iter().all(|s| is_engine_legal(s)), "enumerate_legal_sets emitted an engine-illegal set");
    out
}

// ── The win-probability currency (`mhp`) ─────────────────────────────────────────────────────────────

/// 41-entry monotone logistic LUT: win-probability (permille, 0..=1000) over the Lanchester margin
/// `μ ∈ [-1000, 1000]` permille (step 50), `W(μ) = 1000 / (1 + e^(−μ/250))`. Integer, no `powf`,
/// deterministic. The curve STEEPNESS is the tournament-tunable shape (this seed: `k = 4`, so a dead-even
/// `μ = 0` is 500‰ and `μ = ±1000` is ~±982‰). `assess_engage` already produces `μ`.
const WIN_LUT: [u16; 41] = [
    18, 22, 27, 32, 39, 47, 57, 69, 83, 100, 119, 142, 168, 198, 231, 269, 310, 354, 401, 450, 500, 550,
    599, 646, 690, 731, 769, 802, 832, 858, 881, 900, 917, 931, 943, 953, 961, 968, 973, 978, 982,
];

/// Win-probability in permille (0..=1000) for a Lanchester margin `mu` (permille; clamped to the LUT
/// domain). Linearly interpolated between the 50-wide LUT steps for a smooth local slope.
pub fn win_permille(mu: i64) -> i64 {
    let mu = mu.clamp(-1000, 1000);
    let t = mu + 1000; // 0..=2000
    let i = (t / 50) as usize; // 0..=40
    if i >= WIN_LUT.len() - 1 {
        return WIN_LUT[WIN_LUT.len() - 1] as i64;
    }
    let (lo, hi) = (WIN_LUT[i] as i64, WIN_LUT[i + 1] as i64);
    let frac = t % 50; // 0..49
    lo + (hi - lo) * frac / 50
}

/// Blowout floor for the local margin-slope: even at `|μ|` extremes (the sigmoid flattening to ~0 slope),
/// the relative ordering of "kill A vs heal vs hold safe" must survive, so the squad keeps fighting
/// coherently when winning/losing hard. Seed; tournament-tunable.
const SLOPE_FLOOR: i64 = 1;

/// Local margin-slope `W'(μ)`: the central difference `W(μ+50) − W(μ−50)` (≥ 0 since `W` is monotone),
/// floored at [`SLOPE_FLOOR`]. In permille-win over a 100-wide `μ` window — the multiplier that makes a
/// unit of fighting-strength change worth more near a knife-edge fight than in a blowout.
fn margin_slope(mu: i64) -> i64 {
    (win_permille(mu + 50) - win_permille(mu - 50)).max(SLOPE_FLOOR)
}

/// The per-tick exchange rate between **removing enemy fighting strength** (offense, denial) and
/// **preserving our own** (heal, survival) — the entire calibration surface between dealing damage,
/// preventing a death, and taking risk, derived from the EXISTING Lanchester model (ADR 0025 §2.1), not
/// new constants. `g_us : g_them == enemy_strength : our_strength` (Lanchester): when we are AHEAD,
/// removing enemy strength out-values preserving ours (press the advantage); when BEHIND, the reverse
/// (survival first). Both are scaled by the shared local margin-slope (knife-edge fights weight every
/// term up). Normalized to keep downstream EV products in `i64` while preserving the ratio exactly enough
/// for a stable argmax.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sensitivities {
    /// `mhp` per unit of OUR fighting-strength preserved/added (scales HEAL + survival terms).
    pub g_us: i64,
    /// `mhp` per unit of ENEMY fighting-strength removed (scales OFFENSE + denial terms).
    pub g_them: i64,
}

/// Largest `g` magnitude after normalization — bounds EV products (`g × damage × …`) inside `i64`.
const G_CAP: i128 = 1 << 30;

pub fn sensitivities(our_strength: u64, enemy_strength: u64, mu: i64) -> Sensitivities {
    let slope = margin_slope(mu) as i128;
    // Clear the (shared) denominators of dμ/d(strength): g_us ∝ slope·enemy, g_them ∝ slope·our. The
    // dropped common factor (1000 / enemy²) scales every EV term equally → argmax unchanged.
    let mut g_us = slope * enemy_strength.max(1) as i128;
    let mut g_them = slope * our_strength.max(1) as i128;
    let m = g_us.max(g_them);
    if m > G_CAP {
        g_us = (g_us * G_CAP / m).max(1);
        g_them = (g_them * G_CAP / m).max(1);
    }
    Sensitivities { g_us: g_us as i64, g_them: g_them as i64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(melee: bool, ranged: bool, heal: bool, dismantle: bool, claim: bool) -> ActorCaps {
        ActorCaps { melee, ranged, heal, dismantle, claim }
    }

    #[test]
    fn enumerated_sets_are_all_engine_legal() {
        // Every capability combination's whole menu must be engine-legal by construction.
        for m in [false, true] {
            for r in [false, true] {
                for h in [false, true] {
                    for d in [false, true] {
                        for c in [false, true] {
                            let sets = enumerate_legal_sets(caps(m, r, h, d, c));
                            for s in &sets {
                                assert!(is_engine_legal(s), "illegal set {s:?} for caps m{m} r{r} h{h} d{d} c{c}");
                            }
                            assert!(sets.iter().any(|s| s.is_empty()), "idle/move-only is always an option");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn is_engine_legal_rejects_the_known_illegal_combos() {
        // The exact asymmetry verified against intents.js (ADR 0025 §3).
        assert!(is_engine_legal(&[Act::Melee, Act::Ranged]), "{{melee, ranged}} composes");
        assert!(is_engine_legal(&[Act::Ranged, Act::Heal]), "plain heal does not drop ranged");
        assert!(is_engine_legal(&[Act::Rma, Act::Heal]), "plain heal does not drop RMA");
        assert!(is_engine_legal(&[Act::Dismantle, Act::Ranged]), "dismantle composes with ranged");
        assert!(is_engine_legal(&[Act::Declaim, Act::Ranged]), "declaim composes with ranged");
        assert!(!is_engine_legal(&[Act::Melee, Act::Heal]), "heal drops melee");
        assert!(!is_engine_legal(&[Act::Ranged, Act::RangedHeal]), "rangedHeal drops ranged");
        assert!(!is_engine_legal(&[Act::Rma, Act::RangedHeal]), "rangedHeal drops RMA");
        assert!(!is_engine_legal(&[Act::Ranged, Act::Rma]), "RMA drops single-target ranged");
        assert!(!is_engine_legal(&[Act::Dismantle, Act::Declaim]), "declaim drops dismantle");
        assert!(!is_engine_legal(&[Act::RangedHeal, Act::Heal]), "heal drops rangedHeal");
    }

    #[test]
    fn a_melee_ranged_heal_creep_can_fire_both_weapons_or_fire_and_heal() {
        let sets = enumerate_legal_sets(caps(true, true, true, false, false));
        // The front-line "both weapons" slot is available...
        assert!(sets.iter().any(|s| s.contains(&Act::Melee) && s.contains(&Act::Ranged)), "{{melee, ranged}} offered");
        // ...AND the fire-and-heal slot (ranged keeps firing while plain-healing) — the EV picks between them.
        assert!(sets.iter().any(|s| s.contains(&Act::Ranged) && s.contains(&Act::Heal)), "{{ranged, heal}} offered");
        // ...but never an engine-illegal melee+heal or ranged+rangedHeal.
        assert!(!sets.iter().any(|s| s.contains(&Act::Melee) && s.contains(&Act::Heal)));
        assert!(!sets.iter().any(|s| s.contains(&Act::Ranged) && s.contains(&Act::RangedHeal)));
    }

    #[test]
    fn win_curve_is_monotone_symmetric_and_centered() {
        assert_eq!(win_permille(0), 500, "dead-even margin is a coin flip");
        assert!(win_permille(-2000) < win_permille(0) && win_permille(0) < win_permille(2000), "clamped extremes ordered");
        let mut prev = -1i64;
        for mu in (-1000..=1000).step_by(25) {
            let w = win_permille(mu);
            assert!(w >= prev, "monotone non-decreasing at μ={mu} ({w} < {prev})");
            assert!((0..=1000).contains(&w), "in [0,1000] at μ={mu}");
            prev = w;
        }
        // Symmetry about 500 (±tolerance from integer rounding/interpolation).
        for mu in (50..=1000).step_by(50) {
            let s = win_permille(mu) + win_permille(-mu);
            assert!((995..=1005).contains(&s), "W(μ)+W(−μ)≈1000 at μ={mu} (got {s})");
        }
    }

    #[test]
    fn sensitivity_ratio_follows_lanchester() {
        // Behind (enemy stronger, μ<0): preserving OUR strength out-values removing theirs → g_us > g_them.
        let behind = sensitivities(1000, 3000, -400);
        assert!(behind.g_us > behind.g_them, "when losing, survival/heal weighted above offense");
        // Ahead (we are stronger, μ>0): removing enemy strength (offense) out-values preserving ours.
        let ahead = sensitivities(3000, 1000, 400);
        assert!(ahead.g_them > ahead.g_us, "when winning, offense weighted above survival");
        // The ratio tracks enemy:our (the cleared-denominator identity), within normalization rounding.
        // behind: enemy/our = 3 → g_us ≈ 3 × g_them.
        assert!(behind.g_us >= 2 * behind.g_them, "ratio ~ enemy:our (3:1) when behind");
        assert!(ahead.g_them >= 2 * ahead.g_us, "ratio ~ our:enemy (3:1) when ahead");
    }

    #[test]
    fn sensitivities_are_floored_in_a_blowout_and_stay_in_i64() {
        // Extreme margin → sigmoid is flat → slope floored, so g stays positive (squad still fights).
        let blowout = sensitivities(50_000_000, 1_000, 1000);
        assert!(blowout.g_us > 0 && blowout.g_them > 0, "blowout floor keeps EV ordering alive");
        // Huge strengths must not overflow i64 (normalization caps the magnitude).
        let huge = sensitivities(u64::MAX / 2, u64::MAX / 2, 0);
        assert!(huge.g_us > 0 && huge.g_them > 0 && huge.g_us as i128 <= G_CAP && huge.g_them as i128 <= G_CAP);
    }
}
