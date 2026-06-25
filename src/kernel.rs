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

use crate::kite::ThreatField;
use crate::{CombatCreepDto, CombatIntent, CombatStructureDto, FocusTarget, Ownership};
use screeps::local::LocalCostMatrix;
use screeps::{Position, RawObjectId, RoomCoordinate, StructureType};
use screeps_combat_engine::constants::{HEAL_POWER, RANGED_HEAL_POWER};
use std::collections::HashMap;

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

// ── The per-(tile × action-set) EV kernel + squad commit/drain loop ───────────────────────────────────

/// A squad member as the kernel sees it (built by the adapter from the live/sim member). Capabilities +
/// per-tick outputs drive `enumerate_legal_sets` + pricing; `id` lets a heal intent target this ally.
#[derive(Clone, Copy, Debug)]
pub struct EvMember {
    pub idx: usize,
    pub id: Option<RawObjectId>,
    pub pos: Position,
    pub hits: u32,
    pub hits_max: u32,
    pub caps: ActorCaps,
    /// Melee damage/tick at range 1 (working ATTACK × `ATTACK_POWER`).
    pub melee_power: u32,
    /// Ranged damage/tick at range ≤ 3 (working RANGED_ATTACK × `RANGED_ATTACK_POWER`).
    pub ranged_power: u32,
    /// Working HEAL part count (heal output = parts × {12 @≤1, 4 @2..=3}).
    pub heal_parts: u32,
    /// Structure damage/tick at range 1 via `dismantle` (working WORK × `DISMANTLE_POWER`).
    pub dismantle_power: u32,
    /// Controller-attack/tick at range 1 (working CLAIM × `CONTROLLER_ATTACK_PER_PART`).
    pub claim_power: u32,
}

/// The kernel's per-member output: where to move (`goal`) and what to do (`intents`, the chosen
/// engine-legal action-set, already resolved to targets). Movement rides `member_goals`; intents ride
/// `member_intents` (ADR 0025 — no separate `decide_combat` pass for managed creeps).
#[derive(Clone, Debug, Default)]
pub struct EvResult {
    pub goal: Option<Position>,
    pub intents: Vec<CombatIntent>,
}

// Seed value constants — every one is a tournament-tunable seam (ADR 0025 directive 3: do NOT hand-tune;
// get a clean working system, then sweep). Structure objective values are in the same "fighting-strength
// removed" units as a creep's `threat_value`, so the `g_them` currency prices razing a tower vs killing a
// creep on one scale.
fn struct_kind_value(t: StructureType) -> i64 {
    match t {
        StructureType::InvaderCore => 700,
        StructureType::Tower => 600, // its tower_dps is literally in enemy_strength — razing it raises W
        StructureType::Spawn => 500,
        _ => 0, // ramparts/walls/etc. are valueless EXCEPT the chosen breach focus (bonus below)
    }
}
/// The chosen objective / breach structure (`focus`) gets this ×bonus so the squad commits to breaking it
/// (reuses the existing `select_focus_target`/`breach_redirect` to pick WHICH structure; ADR 0025 §2.4).
const FOCUS_STRUCT_VALUE: i64 = 600;
const FOCUS_STRUCT_BONUS: i64 = 4;
/// Healing an ally in MORTAL danger (anticipated incoming ≥ its hits) prevents losing its whole fighting
/// strength — worth a multiple of a marginal top-up.
const MORTAL_HEAL_MULT: i64 = 4;
const SPACING_R: u32 = 1;

/// The kernel's **position-shaping tuning seam** (ADR 0025 directive 3 — the tournament tunes these;
/// `Default` is the working seed set this build shipped with). Each coefficient is a multiple of the
/// per-tick currency `unit`, so they stay commensurate with offense/heal EV (a kill is `unit × hundreds`;
/// these are tie-breaker scale, `unit × units`). They are the stability + engagement levers:
/// - `approach_coef` — the downhill pull toward the objective (per tile of flood-distance). MUST exceed
///   `incumbency_coef` or a creep stalls instead of advancing.
/// - `incumbency_coef` — the dead-band that holds a FIRING tile (damps engaged period-2 jitter).
/// - `discohesion_coef` / `cohesion_k` — the squad-cohesion pull past radius K from the centroid.
/// - `spacing_coef` — the spread penalty for crowding a claimed tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelParams {
    pub approach_coef: i64,
    pub incumbency_coef: i64,
    pub discohesion_coef: i64,
    pub cohesion_k: u32,
    pub spacing_coef: i64,
}

impl Default for KernelParams {
    fn default() -> Self {
        Self { approach_coef: 2, incumbency_coef: 3, discohesion_coef: 10, cohesion_k: 3, spacing_coef: 1 }
    }
}
/// Hard survival backstop: a tile whose net incoming would kill the member within the horizon is priced
/// astronomically so it is never chosen regardless of EV (the binary veto under the graduated RISK term).
const LETHAL: i64 = 1_000_000_000_000_000;
const SURVIVAL_HORIZON: i32 = 3;

/// A damage target (enemy creep OR structure) in the shared residual ledger.
struct DamageTarget {
    pos: Position,
    id: Option<RawObjectId>,
    /// Hits left to remove THIS tick to complete it (creep: hits + reaching heal; structure: hits).
    residual: i64,
    /// `mhp` per point of damage landed = `g_them × value / initial_residual` (linear progress toward the
    /// target's full objective value; ADR 0025 — the finish/concentration bonus is a tournament refinement).
    value_per_hit: i64,
    /// Reachable by melee/dismantle (range 1) — structures + creeps; ranged (≤3) — all.
    is_structure: bool,
    /// Melee attack-back (the target's own ATTACK output) — the `SELF_RISK_MELEE` term (creeps only).
    attack_back: u32,
}

/// Heal need for an ally in the shared ledger.
struct HealTarget {
    pos: Position,
    id: Option<RawObjectId>,
    /// HP worth healing this tick (deficit + anticipated incoming), drained as members heal it.
    need: i64,
    mortal: bool,
}

fn rma_factor(range: u32) -> i64 {
    match range {
        0..=1 => 10,
        2 => 4,
        3 => 1,
        _ => 0,
    }
}

/// **The unified EV-of-(position × action) squad decision** (ADR 0025). For each member it (1) emits the
/// highest-EV engine-legal action-set it can fire from its CURRENT tile (engine resolves actions before
/// movement), draining the shared residual ledgers so combined fire/heal never double-counts, and (2)
/// moves to the local tile whose best-action EV (net of incoming risk, cohesion, and a downhill approach
/// gradient when out of range) is highest. Members are processed in value-sorted order (highest-leverage
/// first) — not a role order. Formation emerges; no archetype, no desired-range, no claim-priority.
#[allow(clippy::too_many_arguments)]
pub fn plan_squad_ev(
    members: &[EvMember],
    hostiles: &[CombatCreepDto],
    structures: &[CombatStructureDto],
    focus: Option<FocusTarget>,
    centroid: Position,
    our_strength: u64,
    enemy_strength: u64,
    mu: i64,
    threat_field: &ThreatField,
    squad_heal: u32,
    matrix: &LocalCostMatrix,
    dist_to_target: &HashMap<(u8, u8), u32>,
    params: &KernelParams,
) -> Vec<EvResult> {
    let room = centroid.room_name();
    let Sensitivities { g_us, g_them } = sensitivities(our_strength, enemy_strength, mu);
    let unit = ((g_us + g_them) / 2).max(1);

    // ── STAGE 0: shared residual ledgers ──
    // Killable enemy creeps (kill budget + EV value), via the existing EV target order.
    let our_dps: u32 = members.iter().map(|m| m.melee_power + m.ranged_power).sum();
    let mut dmg: Vec<DamageTarget> = crate::ev_target_order(hostiles, structures, our_dps)
        .into_iter()
        .map(|(c, budget)| {
            let value = g_them.saturating_mul(crate::threat_value(c).max(1) as i64);
            DamageTarget {
                pos: c.pos,
                id: c.id,
                residual: budget.max(1) as i64,
                value_per_hit: (value / budget.max(1) as i64).max(1),
                is_structure: false,
                attack_back: c.working_parts(screeps::Part::Attack) as u32 * screeps_combat_engine::constants::ATTACK_POWER,
            }
        })
        .collect();
    // Valuable enemy structures (kind value; the chosen focus/breach structure gets the big bonus).
    for s in structures.iter().filter(|s| s.ownership == Ownership::Hostile && s.hits > 0) {
        let is_focus = focus.is_some_and(|f| f.id.is_none() && f.pos == s.pos);
        let base = struct_kind_value(s.structure_type);
        let v = if is_focus { (base + FOCUS_STRUCT_VALUE) * FOCUS_STRUCT_BONUS } else { base };
        if v == 0 {
            continue;
        }
        let value = g_them.saturating_mul(v);
        dmg.push(DamageTarget {
            pos: s.pos,
            id: None,
            residual: s.hits.max(1) as i64,
            value_per_hit: (value / s.hits.max(1) as i64).max(1),
            is_structure: true,
            attack_back: 0,
        });
    }
    // Ally heal needs (deficit + anticipated incoming; mortal first).
    let mut heals: Vec<HealTarget> = members
        .iter()
        .filter(|m| m.hits > 0)
        .map(|m| {
            let inc = threat_field.raw_at(m.pos).max(0) as u32;
            HealTarget {
                pos: m.pos,
                id: m.id,
                need: (m.hits_max.saturating_sub(m.hits) + inc) as i64,
                mortal: inc >= m.hits,
            }
        })
        .collect();

    // ── Commit order: STABLE (member index) ──
    // A value-sorted "highest-leverage first" order churns tick-to-tick (per-tick residual/position
    // shifts flip the comparator), so contended tiles get reassigned between members every tick → a
    // period-2 position oscillation. A stable index order resolves contention deterministically (the
    // same member always claims first), which is what kills the swap-oscillation. (Re-introducing a
    // value/contestedness priority is a tournament refinement once it carries its own hysteresis.)
    let order: Vec<usize> = (0..members.len()).collect();

    let mut out = vec![EvResult::default(); members.len()];
    let mut claimed: Vec<Position> = Vec::with_capacity(members.len());
    for &mi in &order {
        let m = &members[mi];
        // (1) ACTION from the current tile (engine resolves actions pre-move); drains the ledgers.
        let (_, intents) = best_action(m, m.pos, &mut dmg, &mut heals, g_us, true);
        // (2) POSITION: the local tile (current ±1, walkable) whose value is highest.
        let goal = best_tile(m, room, &dmg, &heals, squad_heal, g_us, unit, centroid, threat_field, matrix, dist_to_target, &claimed, params);
        if let Some(g) = goal {
            claimed.push(g);
        }
        out[mi] = EvResult { goal, intents };
    }
    out
}

/// The `mhp` value of the best engine-legal action-set `m` could fire from `tile` against the CURRENT
/// ledgers (no drain) — used for the commit-order pre-pass and inside `best_tile`.
fn best_action_value(m: &EvMember, tile: Position, dmg: &[DamageTarget], heals: &[HealTarget], g_us: i64) -> i64 {
    let mut dmg_clone: Vec<i64> = dmg.iter().map(|t| t.residual).collect();
    let mut heal_clone: Vec<i64> = heals.iter().map(|t| t.need).collect();
    score_best_set(m, tile, dmg, heals, &mut dmg_clone, &mut heal_clone, g_us, false).0
}

/// Pick + (optionally) COMMIT the best engine-legal action-set from `tile`. Returns its `mhp` value and,
/// when `emit`, the resolved [`CombatIntent`]s; when committing it drains the shared `dmg`/`heals` ledgers.
#[allow(clippy::too_many_arguments)]
fn best_action(
    m: &EvMember,
    tile: Position,
    dmg: &mut [DamageTarget],
    heals: &mut [HealTarget],
    g_us: i64,
    emit: bool,
) -> (i64, Vec<CombatIntent>) {
    let mut dmg_res: Vec<i64> = dmg.iter().map(|t| t.residual).collect();
    let mut heal_need: Vec<i64> = heals.iter().map(|t| t.need).collect();
    let (val, set) = score_best_set(m, tile, dmg, heals, &mut dmg_res, &mut heal_need, g_us, emit);
    if emit {
        // Apply the chosen set's drains to the shared ledgers.
        for (i, t) in dmg.iter_mut().enumerate() {
            t.residual = dmg_res[i];
        }
        for (i, t) in heals.iter_mut().enumerate() {
            t.need = heal_need[i];
        }
    }
    (val, set)
}

/// Core pricer: over every engine-legal action-set, greedily resolve each `Act` to its best target
/// against a LOCAL copy of the ledgers (`dmg_res`/`heal_need`, so a set's own actions don't double-count
/// the same target), score the set in `mhp`, and return the max-value set (+ its intents when `emit`).
/// On the chosen set the local ledgers reflect its drains (the caller copies them back when committing).
#[allow(clippy::too_many_arguments)]
fn score_best_set(
    m: &EvMember,
    tile: Position,
    dmg: &[DamageTarget],
    heals: &[HealTarget],
    dmg_res: &mut [i64],
    heal_need: &mut [i64],
    g_us: i64,
    emit: bool,
) -> (i64, Vec<CombatIntent>) {
    let sets = enumerate_legal_sets(m.caps);
    let mut best: (i64, Vec<CombatIntent>, Vec<i64>, Vec<i64>) = (i64::MIN, Vec::new(), Vec::new(), Vec::new());
    for set in &sets {
        let mut res = dmg_res.to_vec();
        let mut need = heal_need.to_vec();
        let mut val = 0i64;
        let mut intents = Vec::new();
        for &act in set {
            val += apply_act(m, tile, act, dmg, heals, &mut res, &mut need, g_us, emit, &mut intents);
        }
        if val > best.0 {
            best = (val, intents, res, need);
        }
    }
    dmg_res.copy_from_slice(&best.2);
    heal_need.copy_from_slice(&best.3);
    (best.0, best.1)
}

/// Resolve one `Act` to its best target against the local ledgers, return its `mhp` contribution, drain
/// the local ledger, and (when `emit`) push the [`CombatIntent`].
#[allow(clippy::too_many_arguments)]
fn apply_act(
    m: &EvMember,
    tile: Position,
    act: Act,
    dmg: &[DamageTarget],
    heals: &[HealTarget],
    res: &mut [i64],
    need: &mut [i64],
    g_us: i64,
    emit: bool,
    intents: &mut Vec<CombatIntent>,
) -> i64 {
    match act {
        Act::Melee | Act::Dismantle => {
            let power = if act == Act::Dismantle { m.dismantle_power } else { m.melee_power };
            let want_struct = act == Act::Dismantle;
            let mut best: Option<(usize, i64)> = None;
            for (i, t) in dmg.iter().enumerate() {
                if res[i] <= 0 || tile.get_range_to(t.pos) > 1 || (want_struct && !t.is_structure) {
                    continue;
                }
                let landed = (power as i64).min(res[i]);
                let mut v = landed.saturating_mul(t.value_per_hit);
                if act == Act::Melee && !t.is_structure {
                    v -= g_us.saturating_mul(t.attack_back as i64); // SELF_RISK_MELEE
                }
                if best.is_none_or(|(_, bv)| v > bv) {
                    best = Some((i, v));
                }
            }
            apply_dmg(act, best, dmg, res, power, emit, intents)
        }
        Act::Ranged => {
            let mut best: Option<(usize, i64)> = None;
            for (i, t) in dmg.iter().enumerate() {
                if res[i] <= 0 || tile.get_range_to(t.pos) > 3 {
                    continue;
                }
                let v = (m.ranged_power as i64).min(res[i]).saturating_mul(t.value_per_hit);
                if best.is_none_or(|(_, bv)| v > bv) {
                    best = Some((i, v));
                }
            }
            apply_dmg(Act::Ranged, best, dmg, res, m.ranged_power, emit, intents)
        }
        Act::Rma => {
            // Area: hit every in-range target, value summed, each drained by the falloff damage.
            let mut total = 0i64;
            for (i, t) in dmg.iter().enumerate() {
                let r = tile.get_range_to(t.pos);
                if res[i] <= 0 || r > 3 {
                    continue;
                }
                let landed = (m.ranged_power as i64 * rma_factor(r) / 10).min(res[i]);
                total += landed.saturating_mul(t.value_per_hit);
                res[i] -= landed;
            }
            if total > 0 && emit {
                intents.push(CombatIntent::RangedMassAttack);
            }
            total
        }
        Act::Declaim => {
            // Controller-attack: the focus controller in range 1 (modeled as a high-value structure on the
            // ledger). For v1 the controller is priced via FOCUS bonus on its structure entry.
            let mut best: Option<(usize, i64)> = None;
            for (i, t) in dmg.iter().enumerate() {
                if res[i] <= 0 || !t.is_structure || tile.get_range_to(t.pos) > 1 {
                    continue;
                }
                let v = (m.claim_power as i64).min(res[i]).saturating_mul(t.value_per_hit);
                if best.is_none_or(|(_, bv)| v > bv) {
                    best = Some((i, v));
                }
            }
            if let Some((i, v)) = best {
                res[i] -= (m.claim_power as i64).min(res[i]);
                if emit {
                    intents.push(CombatIntent::Attack { target: dmg[i].pos, id: None });
                }
                return v;
            }
            0
        }
        Act::Heal | Act::RangedHeal => {
            let max_r = if act == Act::Heal { 1 } else { 3 };
            let per = if act == Act::Heal { HEAL_POWER } else { RANGED_HEAL_POWER };
            let output = (m.heal_parts * per) as i64;
            let mut best: Option<(usize, i64)> = None;
            for (i, t) in heals.iter().enumerate() {
                let r = tile.get_range_to(t.pos);
                if need[i] <= 0 || r > max_r || (act == Act::Heal && r > 1) || (act == Act::RangedHeal && !(2..=3).contains(&r)) {
                    continue;
                }
                let mult = if t.mortal { MORTAL_HEAL_MULT } else { 1 };
                let v = output.min(need[i]).saturating_mul(g_us).saturating_mul(mult);
                if best.is_none_or(|(_, bv)| v > bv) {
                    best = Some((i, v));
                }
            }
            if let Some((i, v)) = best {
                need[i] -= output.min(need[i]);
                if emit {
                    let it = if act == Act::Heal {
                        CombatIntent::Heal { target: heals[i].pos, id: heals[i].id }
                    } else {
                        CombatIntent::RangedHeal { target: heals[i].pos, id: heals[i].id }
                    };
                    intents.push(it);
                }
                return v;
            }
            0
        }
    }
}

/// Shared tail for the single-target damage acts (Melee/Dismantle/Ranged): drain the chosen target's
/// local residual and (when `emit`) push the right intent.
fn apply_dmg(act: Act, best: Option<(usize, i64)>, dmg: &[DamageTarget], res: &mut [i64], power: u32, emit: bool, intents: &mut Vec<CombatIntent>) -> i64 {
    let Some((i, v)) = best else { return 0 };
    res[i] -= (power as i64).min(res[i]);
    if emit {
        let t = &dmg[i];
        let it = match act {
            Act::Ranged => CombatIntent::RangedAttack { target: t.pos, id: t.id },
            Act::Dismantle => CombatIntent::Dismantle { target: t.pos, id: t.id },
            _ => CombatIntent::Attack { target: t.pos, id: t.id },
        };
        intents.push(it);
    }
    v
}

/// Whether `m` could land an OFFENSE action (damage a creep/structure) from `tile` — melee/dismantle/
/// declaim at range 1, ranged at range ≤ 3. Distinguishes a real firing position (where the incumbency
/// dead-band should hold the creep) from an approach/heal-in-place tile (where the approach pull wins).
fn offense_reachable(m: &EvMember, tile: Position, dmg: &[DamageTarget]) -> bool {
    let melee_range = m.caps.melee || m.caps.dismantle || m.caps.claim;
    dmg.iter().any(|t| {
        let r = tile.get_range_to(t.pos);
        (melee_range && r <= 1) || (m.caps.ranged && r <= 3)
    })
}

/// Choose the member's move tile: the local Moore neighbourhood (current ±1, walkable) plus the current
/// tile, scored by best-action EV there − incoming risk − the approach pull toward the objective −
/// discohesion − spacing, with an incumbency dead-band on a firing tile, and the hard survival veto.
#[allow(clippy::too_many_arguments)]
fn best_tile(
    m: &EvMember,
    room: screeps::RoomName,
    dmg: &[DamageTarget],
    heals: &[HealTarget],
    squad_heal: u32,
    g_us: i64,
    unit: i64,
    centroid: Position,
    threat_field: &ThreatField,
    matrix: &LocalCostMatrix,
    dist_to_target: &HashMap<(u8, u8), u32>,
    claimed: &[Position],
    params: &KernelParams,
) -> Option<Position> {
    let (cx, cy) = (m.pos.x().u8() as i32, m.pos.y().u8() as i32);
    let mut best: Option<(i64, u8, u8, Position)> = None;
    for dx in -1..=1 {
        for dy in -1..=1 {
            let (nx, ny) = (cx + dx, cy + dy);
            if !(0..50).contains(&nx) || !(0..50).contains(&ny) {
                continue;
            }
            let (Ok(rx), Ok(ry)) = (RoomCoordinate::new(nx as u8), RoomCoordinate::new(ny as u8)) else {
                continue;
            };
            let tile = Position::new(rx, ry, room);
            if matrix.get(tile.xy()) == u8::MAX {
                continue; // impassable
            }
            let action = best_action_value(m, tile, dmg, heals, g_us);
            let net = (threat_field.raw_at(tile) - squad_heal as i32).max(0);
            // Value of being here = the action EV (offense + heal) − incoming RISK − the approach pull
            // toward the objective. The approach gradient is applied ALWAYS (not only when out of range):
            // otherwise a creep that can merely HEAL in place (heal is "doable" almost everywhere there is
            // an at-risk ally) would never advance to the objective. Once real OFFENSE lands, its huge EV
            // (`g_them × damage`) dwarfs the small approach term, so the creep holds + fights.
            let d = dist_to_target.get(&(tile.x().u8(), tile.y().u8())).copied().unwrap_or(u32::MAX);
            let mut cost = action
                .saturating_sub(g_us.saturating_mul(net as i64))
                .saturating_sub(unit.saturating_mul(params.approach_coef).saturating_mul(d.min(10_000) as i64));
            // Cohesion: penalize distance past K from the squad centroid.
            let coh = centroid.get_range_to(tile);
            if coh > params.cohesion_k {
                cost -= unit.saturating_mul(params.discohesion_coef).saturating_mul((coh - params.cohesion_k) as i64);
            }
            // Spacing: penalize crowding an already-claimed tile.
            if claimed.iter().any(|c| tile.get_range_to(*c) <= SPACING_R && *c != tile) {
                cost -= unit.saturating_mul(params.spacing_coef);
            }
            // Incumbency: hold the current tile to damp engaged period-2 jitter — applied only at a
            // FIRING position (offense reachable). While approaching/healing it is off, so the approach
            // pull (always applied above) is never blocked; once in firing range it strongly damps the
            // tile-flip that two engaged squads would otherwise oscillate into.
            if tile == m.pos && offense_reachable(m, tile, dmg) {
                cost += unit.saturating_mul(params.incumbency_coef);
            }
            // Survival veto: never step onto a tile whose net incoming kills the member within the horizon.
            if m.hits > 0 && net > 0 && net * SURVIVAL_HORIZON > m.hits as i32 {
                cost -= LETHAL;
            }
            // Maximize cost (it's a value); deterministic tie-break (then lower x, lower y).
            let key = (cost, tile.x().u8(), tile.y().u8());
            if best.is_none_or(|(bc, bx, by, _)| (key.0, std::cmp::Reverse(key.1), std::cmp::Reverse(key.2)) > (bc, std::cmp::Reverse(bx), std::cmp::Reverse(by))) {
                best = Some((cost, tile.x().u8(), tile.y().u8(), tile));
            }
        }
    }
    best.map(|(_, _, _, p)| p)
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

    // ── plan_squad_ev: the joint position+action decision (ADR 0025 §4 worked examples) ──
    use crate::{CombatBodyPart, CombatStructureDto, Ownership};
    use screeps::{Part, RoomCoordinate, RoomName, StructureType};

    fn kpos(x: u8, y: u8) -> Position {
        let room: RoomName = "W1N1".parse().unwrap();
        Position::new(RoomCoordinate::new(x).unwrap(), RoomCoordinate::new(y).unwrap(), room)
    }
    fn kbody(parts: &[(Part, u32)]) -> Vec<CombatBodyPart> {
        parts.iter().flat_map(|&(p, n)| std::iter::repeat_n(CombatBodyPart { part: p, hits: 100 }, n as usize)).collect()
    }
    fn kraw(id: u8) -> RawObjectId {
        format!("{id:024x}").parse().unwrap()
    }
    fn khostile(id: u8, x: u8, y: u8, hits: u32, parts: &[(Part, u32)]) -> CombatCreepDto {
        let b = kbody(parts);
        let hm = b.len() as u32 * 100;
        CombatCreepDto { id: Some(kraw(id)), pos: kpos(x, y), hits, hits_max: hm, body: b }
    }
    fn kstruct(x: u8, y: u8, t: StructureType) -> CombatStructureDto {
        CombatStructureDto { pos: kpos(x, y), structure_type: t, hits: 5000, hits_max: 5000, ownership: Ownership::Hostile, energy: 0 }
    }
    fn melee_evm(idx: usize, id: u8, x: u8, y: u8) -> EvMember {
        EvMember {
            idx,
            id: Some(kraw(id)),
            pos: kpos(x, y),
            hits: 1000,
            hits_max: 1000,
            caps: ActorCaps { melee: true, ..Default::default() },
            melee_power: 90,
            ranged_power: 0,
            heal_parts: 0,
            dismantle_power: 0,
            claim_power: 0,
        }
    }
    fn no_threat() -> ThreatField {
        ThreatField::build(&[], &[])
    }
    fn chebyshev_flood(focus: Position) -> HashMap<(u8, u8), u32> {
        let mut m = HashMap::new();
        for x in 0..50u8 {
            for y in 0..50u8 {
                m.insert((x, y), kpos(x, y).get_range_to(focus));
            }
        }
        m
    }

    #[test]
    fn a_melee_creep_attacks_the_adjacent_enemy() {
        let me = melee_evm(0, 1, 25, 25);
        let enemy = khostile(9, 26, 25, 100, &[(Part::Attack, 1)]);
        let focus = Some(FocusTarget { pos: enemy.pos, id: enemy.id });
        let out = plan_squad_ev(&[me], &[enemy], &[], focus, kpos(25, 25), 10_000, 5_000, 300, &no_threat(), 0, &LocalCostMatrix::new(), &chebyshev_flood(kpos(26, 25)), &KernelParams::default());
        assert!(out[0].intents.iter().any(|i| matches!(i, CombatIntent::Attack { target, .. } if *target == kpos(26, 25))), "melee attacks the adjacent enemy: {:?}", out[0].intents);
    }

    #[test]
    fn a_melee_creep_approaches_a_far_enemy() {
        let me = melee_evm(0, 1, 20, 25);
        let enemy = khostile(9, 28, 25, 100, &[(Part::Attack, 1)]);
        let focus = Some(FocusTarget { pos: enemy.pos, id: enemy.id });
        let out = plan_squad_ev(&[me], &[enemy], &[], focus, kpos(20, 25), 10_000, 5_000, 300, &no_threat(), 0, &LocalCostMatrix::new(), &chebyshev_flood(kpos(28, 25)), &KernelParams::default());
        let goal = out[0].goal.expect("a move goal");
        assert!(goal.get_range_to(kpos(20, 25)) <= 1, "a local step: {goal:?}");
        assert!(goal.get_range_to(kpos(28, 25)) < kpos(20, 25).get_range_to(kpos(28, 25)), "steps toward the enemy: {goal:?}");
        assert!(out[0].intents.is_empty(), "nothing in range yet → no action");
    }

    #[test]
    fn a_melee_creep_breaches_the_focus_structure() {
        // No enemy creeps; a hostile spawn is the focus. The melee creep adjacent to it attacks it.
        let me = melee_evm(0, 1, 25, 25);
        let spawn = kstruct(26, 25, StructureType::Spawn);
        let focus = Some(FocusTarget { pos: spawn.pos, id: None });
        let out = plan_squad_ev(&[me], &[], &[spawn], focus, kpos(25, 25), 10_000, 5_000, 300, &no_threat(), 0, &LocalCostMatrix::new(), &chebyshev_flood(kpos(26, 25)), &KernelParams::default());
        assert!(out[0].intents.iter().any(|i| matches!(i, CombatIntent::Attack { target, id: None } if *target == kpos(26, 25))), "melee breaches the structure: {:?}", out[0].intents);
    }

    #[test]
    fn a_healer_heals_a_mortal_ally() {
        // A pure healer + a low-HP ally under melee fire (mortal). No attack targets for the healer.
        let healer = EvMember {
            idx: 0,
            id: Some(kraw(1)),
            pos: kpos(25, 25),
            hits: 1000,
            hits_max: 1000,
            caps: ActorCaps { heal: true, ..Default::default() },
            melee_power: 0,
            ranged_power: 0,
            heal_parts: 5,
            dismantle_power: 0,
            claim_power: 0,
        };
        let ally = EvMember {
            idx: 1,
            id: Some(kraw(2)),
            pos: kpos(26, 25),
            hits: 50,
            hits_max: 500,
            caps: ActorCaps::default(),
            melee_power: 0,
            ranged_power: 0,
            heal_parts: 0,
            dismantle_power: 0,
            claim_power: 0,
        };
        // A hostile melee creep adjacent to the ally (range 1) but range 2 from the healer → only the
        // ally is threatened → mortal (150 ≥ 50). The healer is safe.
        let threat = ThreatField::build(
            &[crate::kite::KiteThreat { pos: kpos(27, 25), kind: crate::kite::ThreatKind::MeleeOnly, reach: 1, step_ticks: Some(1), attack_power: 150, ranged_power: 0 }],
            &[],
        );
        let out = plan_squad_ev(&[healer, ally], &[], &[], None, kpos(25, 25), 10_000, 5_000, -300, &threat, 0, &LocalCostMatrix::new(), &HashMap::new(), &KernelParams::default());
        assert!(out[0].intents.iter().any(|i| matches!(i, CombatIntent::Heal { target, .. } if *target == kpos(26, 25))), "healer heals the mortal ally: {:?}", out[0].intents);
    }

    #[test]
    fn combined_fire_does_not_overkill_one_target() {
        // Two melee creeps, one 100-HP enemy adjacent to both. The kill budget (100) is covered by the
        // first; the second's residual on that enemy is 0 → it does not also pile its whole output there.
        let a = melee_evm(0, 1, 25, 25);
        let b = melee_evm(1, 2, 25, 26);
        let enemy = khostile(9, 26, 25, 100, &[(Part::Attack, 1)]); // both adjacent (a: range1, b: range1)
        let weak = khostile(8, 26, 26, 100, &[(Part::Attack, 1)]); // a second target for the spill
        let focus = Some(FocusTarget { pos: enemy.pos, id: enemy.id });
        let out = plan_squad_ev(&[a, b], &[enemy.clone(), weak.clone()], &[], focus, kpos(25, 25), 20_000, 4_000, 400, &no_threat(), 0, &LocalCostMatrix::new(), &chebyshev_flood(kpos(26, 25)), &KernelParams::default());
        let attacked: Vec<Position> = out.iter().flat_map(|r| r.intents.iter().filter_map(|i| match i { CombatIntent::Attack { target, .. } => Some(*target), _ => None })).collect();
        assert_eq!(attacked.len(), 2, "both creeps act");
        assert!(attacked.contains(&kpos(26, 25)) && attacked.contains(&kpos(26, 26)), "fire spills to the second target instead of overkilling the first: {attacked:?}");
    }
}
