//! Pure threat-centric DEFENSE-emission kernel (ADR 0027 v1, "Defending the wrong room" — Option B).
//!
//! Today `war.rs` emits only `Defend{owned_room}` — a garrison anchored to an OWNED room. But an enemy
//! roams the NEIGHBOUR rooms, so the defender stands uselessly in its empty owned room (the root of the
//! edge-oscillation + churn the lifecycle fix only *bounds*; ADR 0027 backlog #1). This kernel makes
//! defense **threat-centric**: given the owned rooms + the observed threats, it decides which `Secure`
//! objective to emit at the **threat's CURRENT room** (an "intercept" is mechanically just
//! *"go to room X and clear its hostiles"* = `Secure`; ADR 0027 lines 248-261). When the threat is in an
//! owned room the objective sits there (today's defend behaviour); when it roams a neighbour the objective
//! **moves with it** (re-emitted at the threat's room each scan; the stale one TTL-lapses) and the squad
//! reassigns to follow (`ObjectiveGone` on the old → `Reassign` to the new — [`crate::lifecycle`]).
//!
//! Two policy guards ride existing fields, not a parallel variant:
//! - **Asset-priority boost** (ADR 0027 line 253): a threat IN or ADJACENT to a valuable owned room
//!   outranks one chasing a distant roamer — base defense first.
//! - **Over-extension leash** (ADR 0027 line 254): a threat farther than `leash` rooms from the nearest
//!   owned room is NOT chased (we don't drag a squad across the map after a roamer).
//!
//! Pure + deterministic: a `Vec` in, a `BTreeMap`-ordered `Vec` out (no `HashMap` in the result path). The
//! room key is generic so the offline harness can drive synthetic integer rooms while the bot passes
//! `screeps::RoomName`; the caller supplies the Chebyshev room distance (the only spatial fact needed).

/// Priority bands the kernel emits at — mirror `objective_queue`'s `OBJECTIVE_PRIORITY_*` so the bot
/// adapter maps them 1:1 without re-deriving the policy here.
pub const DEFENSE_PRIORITY_CRITICAL: f32 = 100.0;
pub const DEFENSE_PRIORITY_HIGH: f32 = 75.0;
pub const DEFENSE_PRIORITY_MEDIUM: f32 = 50.0;

/// Default over-extension leash (rooms): a threat farther than this from the nearest owned room is not
/// chased. One step out (a neighbour) is always chased; this bounds how far past that we pursue a roamer.
pub const DEFAULT_LEASH: u32 = 2;

/// One observed threat the defense scan found (already reduced to plain facts by the adapter / harness).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Threat<R> {
    /// The room the threat is in RIGHT NOW (the objective is emitted here — the squad intercepts it here).
    pub room: R,
    /// A coarse danger/value score (estimated DPS, hostile count, etc., folded by the caller). Higher ⇒
    /// the threat ranks higher among multiple threats AT THE SAME effective priority band. Never feeds the
    /// band itself (that comes from the asset-priority boost), only the within-band tie-break.
    pub danger: f32,
    /// The neighbour's hostile-TOWER threat (Σ energized-tower DPS at a representative range), carried
    /// DISTINCT from `danger` (ADR 0037 T1). A *defender* must NOT be sized to beat towers, so this is a
    /// pure SIGNAL of "how towered" the room is — kept separate from the creep `danger` and (in T1) never
    /// gating/priority/sizing: it flows through un-consumed for the T2/T3 stages that suppress the bare
    /// defense reflex + route a towered neighbour to the offense/winnability path. `0.0` ⇒ no live towers.
    pub tower_danger: f32,
}

/// One owned room the defender protects, with its strategic value (RCL / asset weight). The kernel uses
/// `value` only to pick which owned room a threat is "adjacent to" when several are in range (the most
/// valuable wins the boost), so a high-value base outranks a marginal outpost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OwnedRoom<R> {
    pub room: R,
    pub value: f32,
}

/// One `Secure` objective the kernel decided to emit. The bot adapter turns this into an
/// `ObjectiveKind::Secure{room}` request at `priority`; the harness pushes it into its toy queue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SecureEmission<R> {
    /// Emit at the threat's CURRENT room (the intercept point).
    pub room: R,
    /// The selection priority (already boosted if the threat is in/adjacent a valuable owned room).
    pub priority: f32,
    /// True iff the asset-priority boost applied (the threat is IN or ADJACENT an owned room) — surfaced
    /// for the HUD/trace + asserted in tests; the boost is already folded into `priority`.
    pub asset_boosted: bool,
}

/// Tunables for the emission policy (so the bed/bot can vary the leash without forking the kernel).
#[derive(Clone, Copy, Debug)]
pub struct DefensePolicy {
    /// Over-extension leash (rooms from the nearest owned room). A threat beyond this is not chased.
    pub leash: u32,
}

impl Default for DefensePolicy {
    fn default() -> Self {
        DefensePolicy { leash: DEFAULT_LEASH }
    }
}

/// THE pure decision. Given owned rooms + observed threats + a Chebyshev room-distance fn, emit the
/// `Secure` objectives (one per threat WITHIN the leash, at its current room, boosted when in/adjacent a
/// valuable owned room). Deterministic: the output is sorted by (priority desc, danger desc, then the
/// caller-stable threat order) so two equal-priority threats resolve identically every run. No `HashMap`.
///
/// `dist(a, b)` is the Chebyshev room distance (0 = same room, 1 = neighbour). The kernel never touches
/// `game::*` — the adapter computes distances from `RoomName`s, the harness from integer room coords.
pub fn emit_defense<R, D>(owned: &[OwnedRoom<R>], threats: &[Threat<R>], policy: DefensePolicy, dist: D) -> Vec<SecureEmission<R>>
where
    R: Copy + PartialEq,
    D: Fn(R, R) -> u32,
{
    // Build (emission, danger) pairs so the danger tie-break is captured at construction (one stable sort,
    // no second pass / no re-lookup). `danger` is only the within-band tie-break — never sets the band.
    let mut out: Vec<(SecureEmission<R>, f32)> = Vec::with_capacity(threats.len());

    for threat in threats {
        // Distance to the NEAREST owned room (and which owned room is nearest, value-tie-broken). With no
        // owned rooms there is nothing to defend → emit nothing (the leash is measured from an owned room).
        let nearest = owned
            .iter()
            .map(|o| (dist(o.room, threat.room), o.value, o.room))
            // Prefer the closer owned room; on a distance tie prefer the MORE VALUABLE one (the base we
            // most want to defend gets the boost). `f32` value compared finite-safe (NaN coalesces to Equal).
            .min_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            });
        let Some((nearest_dist, _value, _room)) = nearest else {
            continue;
        };

        // OVER-EXTENSION LEASH: don't chase a roamer farther than `leash` rooms from any owned room.
        if nearest_dist > policy.leash {
            continue;
        }

        // ASSET-PRIORITY BOOST: a threat IN (dist 0) or ADJACENT (dist 1) to an owned room outranks one
        // chasing a distant roamer — base defense first. In-owned-room is the most urgent (our base is
        // under attack → CRITICAL); adjacent is HIGH (intercept at the border); a leashed-but-not-adjacent
        // roamer is MEDIUM (worth chasing, but below base defense).
        let (priority, asset_boosted) = match nearest_dist {
            0 => (DEFENSE_PRIORITY_CRITICAL, true),
            1 => (DEFENSE_PRIORITY_HIGH, true),
            _ => (DEFENSE_PRIORITY_MEDIUM, false),
        };

        out.push((SecureEmission { room: threat.room, priority, asset_boosted }, threat.danger));
    }

    // Deterministic ordering: highest priority first, then higher danger, then the caller's stable input
    // order (a stable sort preserves the original `Vec` order on full ties). The input `threats` is the
    // caller's stable `Vec` (never a `HashMap` iteration), so this is bit-reproducible — no `HashMap` in
    // the result path.
    out.sort_by(|a, b| {
        b.0.priority
            .partial_cmp(&a.0.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    out.into_iter().map(|(e, _)| e).collect()
}

/// One room the live defense scan OBSERVED (a visible, non-owned neighbour candidate), reduced to the two
/// plain facts the neighbour-threat decision needs. The bot adapter builds these from `game::rooms()` /
/// the room intel (the only non-pure step); the harness builds synthetic ones.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObservedRoom<R> {
    /// The room the hostiles were seen in.
    pub room: R,
    /// Whether ANY hostile in the room warrants a defender (the caller folds `hostile_warrants_defender`
    /// over the bodies). A room with only unarmed scouts/haulers is `false`.
    pub armed: bool,
    /// The danger estimate (summed DPS, etc.) the caller folded over the armed hostiles — passed straight
    /// through to `Threat::danger` for the within-band tie-break.
    pub danger: f32,
    /// The room's hostile-TOWER threat (Σ energized-tower DPS at a representative range), DISTINCT from the
    /// creep `danger` (ADR 0037 T1). The caller reads it from the SCOUTED threat data
    /// (`RoomThreatData.hostile_tower_positions` + `tower_energy`, the same signal offense uses); the kernel
    /// carries it straight through to `Threat::tower_danger` UNGATED (T1 is purely additive — it never
    /// filters/prioritises/sizes on it yet). `0.0` ⇒ no live (energized) towers scouted.
    pub tower_danger: f32,
}

/// PURE neighbour-threat builder (ADR 0027 v1 LIVE SEAM). Given the owned rooms, the observed visible
/// neighbour rooms, the leash, and the Chebyshev room-distance fn, produce the `Vec<Threat>` to feed
/// alongside the owned-room threats into [`emit_defense`].
///
/// A neighbour room becomes a single `Threat{room, danger}` IFF it is ARMED (the caller already folded
/// `hostile_warrants_defender` → `ObservedRoom::armed`) AND it is WITHIN the leash of the nearest owned
/// room (we don't even gather beyond the leash — the kernel would drop it, but bounding here keeps the fed
/// list tight). A swarm of N hostiles in one neighbour is ONE `ObservedRoom` → ONE `Threat` (the danger is
/// already summed) → ONE `Secure` objective downstream — never N. Owned rooms are excluded (dist 0): owned
/// threats are gathered on the existing owned-room path, so this builder covers only the dist 1..=leash
/// band and never double-counts a room the owned scan already fed.
///
/// Deterministic: a `Vec` in → a `Vec` out in the caller's stable input order; no `HashMap`, no `game::*`.
pub fn neighbour_threats<R, D>(owned: &[OwnedRoom<R>], observed: &[ObservedRoom<R>], policy: DefensePolicy, dist: D) -> Vec<Threat<R>>
where
    R: Copy + PartialEq,
    D: Fn(R, R) -> u32,
{
    let mut out: Vec<Threat<R>> = Vec::new();

    for obs in observed {
        // Bounded #1: only an ARMED room (a hostile that warrants a defender) becomes a threat.
        if !obs.armed {
            continue;
        }

        // Distance to the NEAREST owned room. With no owned rooms there is nothing to defend.
        let Some(nearest_dist) = owned.iter().map(|o| dist(o.room, obs.room)).min() else {
            continue;
        };

        // Owned rooms (dist 0) are handled by the existing owned-room scan — never double-count one here.
        if nearest_dist == 0 {
            continue;
        }

        // Bounded #2: within the leash. We don't even gather beyond it (the kernel would drop it anyway).
        if nearest_dist > policy.leash {
            continue;
        }

        // Carry the tower signal through DISTINCT from the creep danger (ADR 0037 T1). Ungated — nothing
        // filters/prioritises on `tower_danger` yet; it just rides alongside for the T2/T3 consumers.
        out.push(Threat { room: obs.room, danger: obs.danger, tower_danger: obs.tower_danger });
    }

    out
}

// ── observe_neighbours (ADR 0027 P0 — the LAST live-only glue, now a PURE kernel) ────────────────────────
//
// Before P0 the neighbour OBSERVATION decision lived inline in `war.rs::run_defense_scan`: the armed-check
// (`hostile_warrants_defender`), the per-part danger estimate (Attack=30 / RangedAttack=10), the
// visible/non-owned/within-leash filter, and the swarm→one-`ObservedRoom`-per-room fold were all done over
// `game::*` reads — so the whole observation LAYER was un-sim-able (ADR 0027 "Update 2026-06-28 — sim-able
// layers", line 343-358 / the P0 migration item line 324-328). This lifts that DECISION into a pure fn:
// given the raw per-room hostile bodies + the visibility/ownership/distance facts, decide which rooms become
// `ObservedRoom`s. `war.rs` keeps ONLY the raw `game::rooms()` → (room, hostile parts) read, then calls this
// kernel, so the full observation decision is pure + deterministic + offline-provable (`run_v1_flow`).

/// Whether a hostile creep warrants dispatching a defender (lifted from `war.rs::hostile_warrants_defender`,
/// ADR 0027 P0). `RoomDynamicVisibilityData::hostile_creeps()` only flags Attack/RangedAttack/Work, so an
/// enemy CLAIM creep neutralising a controller (carrying neither) slips through it — in a towerless RCL1-2
/// room nothing else engages it, so it silently declaims us. This keys on body parts instead: armed creeps
/// (Attack/RangedAttack), dismantlers (Work), controller-attackers (Claim), and healers sustaining them
/// (Heal) are all worth a defender; pure scouts/haulers (only Move/Carry/Tough) are not. Pure.
pub fn hostile_warrants_defender(parts: &[screeps::Part]) -> bool {
    use screeps::Part;
    parts
        .iter()
        .any(|p| matches!(p, Part::Attack | Part::RangedAttack | Part::Work | Part::Claim | Part::Heal))
}

/// The coarse danger estimate (summed DPS) over a hostile's live body parts (lifted from the war.rs inline
/// fold, ADR 0027 P0): Attack=30, RangedAttack=10, everything else 0. The within-band tie-break currency
/// (`Threat::danger` / `ObservedRoom::danger`), NOT the priority band (that comes from the asset boost). The
/// caller passes only LIVE parts (`hits() > 0`). Pure + deterministic (a fold over a slice, no `HashMap`).
pub fn estimate_danger(parts: &[screeps::Part]) -> f32 {
    use screeps::Part;
    parts
        .iter()
        .map(|p| match p {
            Part::Attack => 30.0,
            Part::RangedAttack => 10.0,
            _ => 0.0,
        })
        .sum()
}

/// Per-WORK dismantle-danger, applied only where we own structures to tear down (see [`dismantle_danger`]).
/// Weighted below a pure attacker (30) because a dismantler cannot counter-attack; a defense-urgency proxy,
/// not literal `DISMANTLE_POWER`. Tune here.
pub const DISMANTLE_DANGER_PER_WORK: f32 = 15.0;

/// Dismantle danger from a hostile's WORK parts, **gated on whether we own dismantle-able structures in that
/// room**. A hostile WORK creep threatens us ONLY where we have built structures (base / ramparts / walls)
/// for it to tear down — `DISMANTLE_POWER` 50/tick, a breach even at ZERO creep-DPS; there, size a killer.
/// Where we hold nothing, there is nothing to dismantle, so it is harmless. This is the same principle that
/// keeps [`estimate_danger`] at 0 for WORK on the NEIGHBOUR path (a dismantler idling in a hostile room where
/// we own nothing — the ADR 0037 towered-neighbour case — is not a threat to US); the structure gate makes
/// the condition explicit rather than a fixed weight.
pub fn dismantle_danger(work_parts: usize, has_targetable_structures: bool) -> f32 {
    if has_targetable_structures {
        work_parts as f32 * DISMANTLE_DANGER_PER_WORK
    } else {
        0.0
    }
}

/// One RAW per-room observation the live scan gathered (the only non-pure step left in war.rs): a room, the
/// hostile bodies seen in it (each a slice of LIVE parts), and the visibility/ownership/distance facts. The
/// bot builds these from `game::rooms()` / room intel (excluding Source Keepers before this point); the
/// harness builds synthetic ones. `R` is generic so the harness drives integer rooms.
#[derive(Clone, Debug)]
pub struct RawObservation<'a, R> {
    /// The room the hostiles were seen in.
    pub room: R,
    /// One entry per hostile creep in the room — its LIVE body parts (`hits() > 0`). The kernel folds the
    /// armed-check + the danger estimate over these (a swarm of N hostiles ⇒ N entries ⇒ ONE `ObservedRoom`).
    pub hostile_bodies: &'a [Vec<screeps::Part>],
    /// The room is VISIBLE this scan (the bot read it from `game::rooms()`); an invisible room is simply
    /// absent from the input.
    pub visible: bool,
    /// We own this room (its threats are covered by the owned-room scan — never double-counted here).
    pub is_owned: bool,
    /// Chebyshev room-distance to the NEAREST owned room (`None` ⇒ no owned rooms → nothing to defend).
    pub nearest_owned_dist: Option<u32>,
    /// The room's hostile-TOWER threat (Σ energized-tower DPS at a representative range), read by the bot
    /// from the SCOUTED `RoomThreatData` (`hostile_tower_positions` + `tower_energy`, the same signal
    /// offense uses); the harness sets it synthetically. DISTINCT from the creep `hostile_bodies` danger
    /// (ADR 0037 T1). `observe_neighbours` carries it through to `ObservedRoom::tower_danger` UNGATED — T1
    /// never folds it into the armed-check, the danger, or any filter; it is a pure additive signal for the
    /// T2/T3 consumers. `0.0` ⇒ no live towers.
    pub tower_danger: f32,
}

/// THE pure OBSERVATION decision (ADR 0027 P0). Given the raw per-room hostile observations + the leash,
/// decide which rooms become a single `ObservedRoom` to feed [`neighbour_threats`]. A raw observation
/// becomes ONE `ObservedRoom{room, armed, danger}` IFF it is VISIBLE, NON-OWNED, WITHIN the leash of the
/// nearest owned room, and has ≥1 hostile that warrants a defender (the armed fold). A swarm of N hostiles in
/// one room is ONE `ObservedRoom` (danger summed across the bodies), never N. Owned rooms (dist 0 / `is_owned`)
/// are excluded — covered by the owned-room scan. Deterministic: a `Vec` in → a `Vec` out in the caller's
/// stable input order; no `HashMap`, no `game::*`.
pub fn observe_neighbours<R>(observations: &[RawObservation<R>], policy: DefensePolicy) -> Vec<ObservedRoom<R>>
where
    R: Copy,
{
    let mut out: Vec<ObservedRoom<R>> = Vec::new();

    for obs in observations {
        // VISIBLE + NON-OWNED only (the live filter). Owned rooms are covered by the owned-room scan.
        if !obs.visible || obs.is_owned {
            continue;
        }
        // Within the leash of the nearest owned room (and there IS an owned room). dist 0 = owned → excluded.
        let Some(nearest) = obs.nearest_owned_dist else {
            continue;
        };
        if nearest == 0 || nearest > policy.leash {
            continue;
        }
        // Fold the armed-check + the danger estimate over the swarm (N bodies → one room verdict).
        let mut armed = false;
        let mut danger: f32 = 0.0;
        for body in obs.hostile_bodies {
            if hostile_warrants_defender(body) {
                armed = true;
            }
            danger += estimate_danger(body);
        }
        // A room with only unarmed scouts/haulers warrants no defender.
        if !armed {
            continue;
        }
        // ── ADR 0037 T2 (SUPPRESS the bare defense reflex) ────────────────────────────────────────────────
        // Do NOT emit a Secure for a neighbour whose ONLY threat is a `danger==0` creep sitting under hostile
        // towers. A non-attacking creep (a Work dismantler / Claim controller-attacker / Heal healer — armed
        // per `hostile_warrants_defender` but scoring 0 DPS in `estimate_danger`) that is inside a
        // towered/hostile-owned room is NOT attacking US: a bare floor-sized defender can never beat the
        // towers (T3 routes a towered room to the winnability-gated offense path instead), and the creep only
        // becomes a real threat if it ENTERS one of our rooms — at which point it fires its OWN owned-room
        // Secure (which this neighbour path never touches). So DROP exactly this room.
        //
        // PRESERVE (do NOT suppress):
        //   (i)  `danger > 0.0` — a REAL attacker (Attack/RangedAttack) adjacent IS a threat, towered or not.
        //   (ii) `danger == 0.0 && tower_danger == 0.0` — a Work/Claim/Heal creep in a NON-towered room (a
        //        dismantler razing our remote, a claimer attacking our reservation) is a genuine non-tower
        //        threat and still warrants a defender.
        // ONLY the `danger==0 && tower_danger>0` case is dropped. Threshold gate: an EXACT-ZERO compare on the
        // creep danger + a strictly-positive tower signal — a clean `==0`/`>0` boundary (no float→discrete
        // bucketing beyond the threshold), so it stays deterministic. `tower_danger` is otherwise unconsumed
        // (T1 carries it ungated); this is its first (T2) consumer.
        if danger == 0.0 && obs.tower_danger > 0.0 {
            continue;
        }
        // Carry the tower signal through DISTINCT from the creep danger (ADR 0037 T1). It rode ungated in T1;
        // T2 (above) is now its first consumer (the suppress gate). It still never enters the armed-check or
        // the danger fold — so a *defender* that IS emitted (a real attacker) is never sized to the towers,
        // and the T3 stage can still route a towered room to the offense/winnability path.
        out.push(ObservedRoom { room: obs.room, armed, danger, tower_danger: obs.tower_danger });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic integer rooms (the harness's model): Chebyshev distance on (x, y) grid coords.
    type Room = (i32, i32);
    fn cheby(a: Room, b: Room) -> u32 {
        (a.0 - b.0).unsigned_abs().max((a.1 - b.1).unsigned_abs())
    }

    fn owned(r: Room, value: f32) -> OwnedRoom<Room> {
        OwnedRoom { room: r, value }
    }
    fn threat(r: Room, danger: f32) -> Threat<Room> {
        Threat { room: r, danger, tower_danger: 0.0 }
    }

    /// The objective is emitted AT THE THREAT'S CURRENT ROOM (the intercept point), not the owned room —
    /// the core "Defending the wrong room" fix. A threat in a neighbour emits Secure{neighbour}.
    #[test]
    fn emits_at_the_threat_room_not_the_owned_room() {
        let out = emit_defense(&[owned((0, 0), 1.0)], &[threat((1, 0), 5.0)], DefensePolicy::default(), cheby);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].room, (1, 0), "the Secure objective sits at the threat's room, not the owned room");
    }

    /// ASSET-PRIORITY BOOST: a threat IN an owned room is CRITICAL; ADJACENT is HIGH; a leashed roamer two
    /// rooms out is MEDIUM (no boost). Base defense outranks chasing a distant roamer.
    #[test]
    fn asset_priority_boost_ranks_in_then_adjacent_then_far() {
        let owned_rooms = [owned((0, 0), 1.0)];
        let in_room = emit_defense(&owned_rooms, &[threat((0, 0), 1.0)], DefensePolicy::default(), cheby);
        assert_eq!(in_room[0].priority, DEFENSE_PRIORITY_CRITICAL);
        assert!(in_room[0].asset_boosted);

        let adjacent = emit_defense(&owned_rooms, &[threat((1, 1), 1.0)], DefensePolicy::default(), cheby);
        assert_eq!(adjacent[0].priority, DEFENSE_PRIORITY_HIGH);
        assert!(adjacent[0].asset_boosted);

        // Two rooms out (within the default leash=2) → MEDIUM, no boost.
        let far = emit_defense(&owned_rooms, &[threat((2, 0), 1.0)], DefensePolicy::default(), cheby);
        assert_eq!(far[0].priority, DEFENSE_PRIORITY_MEDIUM);
        assert!(!far[0].asset_boosted);
    }

    /// OVER-EXTENSION LEASH: a threat farther than the leash from any owned room is NOT chased — no
    /// emission (we don't drag a squad across the map after a roamer).
    #[test]
    fn leash_bounds_the_chase_distance() {
        let owned_rooms = [owned((0, 0), 1.0)];
        // leash = 2: a threat 3 rooms out is dropped.
        let out = emit_defense(&owned_rooms, &[threat((3, 0), 9.0)], DefensePolicy::default(), cheby);
        assert!(out.is_empty(), "a threat beyond the leash is not chased");
        // A tighter leash = 1 drops a 2-rooms-out threat too.
        let tight = emit_defense(&owned_rooms, &[threat((2, 0), 9.0)], DefensePolicy { leash: 1 }, cheby);
        assert!(tight.is_empty(), "a leash=1 drops a 2-rooms-out threat");
        // The neighbour (dist 1) is still chased under leash=1.
        let neighbour = emit_defense(&owned_rooms, &[threat((1, 0), 9.0)], DefensePolicy { leash: 1 }, cheby);
        assert_eq!(neighbour.len(), 1);
    }

    /// No owned rooms → nothing to defend → no emission (the leash is measured from an owned room).
    #[test]
    fn no_owned_rooms_emits_nothing() {
        let out = emit_defense(&[], &[threat((1, 0), 5.0)], DefensePolicy::default(), cheby);
        assert!(out.is_empty());
    }

    /// Multiple threats: the in-base CRITICAL outranks the adjacent HIGH outranks the leashed MEDIUM, and a
    /// beyond-leash threat is dropped. Deterministic ordering (priority desc, then danger).
    #[test]
    fn multiple_threats_rank_by_priority_then_danger_deterministically() {
        let owned_rooms = [owned((0, 0), 1.0)];
        let threats = [
            threat((2, 0), 3.0), // leashed MEDIUM
            threat((0, 0), 1.0), // in-base CRITICAL
            threat((1, 0), 2.0), // adjacent HIGH
            threat((5, 0), 9.0), // beyond leash — dropped
        ];
        let out = emit_defense(&owned_rooms, &threats, DefensePolicy::default(), cheby);
        assert_eq!(out.len(), 3, "the beyond-leash threat is dropped");
        assert_eq!(out[0].room, (0, 0), "in-base CRITICAL first");
        assert_eq!(out[1].room, (1, 0), "adjacent HIGH second");
        assert_eq!(out[2].room, (2, 0), "leashed MEDIUM last");
        // Deterministic: same input → same output.
        assert_eq!(out, emit_defense(&owned_rooms, &threats, DefensePolicy::default(), cheby));
    }

    /// Two equal-priority (both adjacent) threats break the tie by danger (higher danger first),
    /// deterministically.
    #[test]
    fn equal_priority_breaks_by_danger() {
        let owned_rooms = [owned((0, 0), 1.0)];
        let threats = [threat((1, 0), 2.0), threat((-1, 0), 7.0)];
        let out = emit_defense(&owned_rooms, &threats, DefensePolicy::default(), cheby);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].room, (-1, 0), "higher danger ranks first on a priority tie");
        assert_eq!(out[1].room, (1, 0));
    }

    // ── neighbour_threats (ADR 0027 v1 LIVE SEAM) ──────────────────────────────────────────────────────

    fn obs(r: Room, armed: bool, danger: f32) -> ObservedRoom<Room> {
        ObservedRoom { room: r, armed, danger, tower_danger: 0.0 }
    }

    /// An ARMED hostile in a VISIBLE neighbour WITHIN the leash becomes one `Threat` at that neighbour, with
    /// the folded danger passed straight through.
    #[test]
    fn neighbour_armed_visible_within_leash_becomes_a_threat() {
        let owned_rooms = [owned((0, 0), 1.0)];
        // dist 1 (adjacent) and dist 2 (within default leash) — both armed → both fed.
        let observed = [obs((1, 0), true, 5.0), obs((2, 0), true, 3.0)];
        let out = neighbour_threats(&owned_rooms, &observed, DefensePolicy::default(), cheby);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], threat((1, 0), 5.0), "adjacent armed neighbour fed with its danger");
        assert_eq!(out[1], threat((2, 0), 3.0), "within-leash armed neighbour fed with its danger");
    }

    /// UNARMED (a scout/hauler, `armed=false`), INVISIBLE (never appears in `observed` at all), and
    /// BEYOND-LEASH neighbours each produce NO threat — the three bounds.
    #[test]
    fn neighbour_unarmed_invisible_or_beyond_leash_yields_none() {
        let owned_rooms = [owned((0, 0), 1.0)];
        let observed = [
            obs((1, 0), false, 5.0), // armed=false (unarmed scout/hauler) → dropped
            obs((3, 0), true, 9.0),  // beyond default leash=2 → dropped
            // an INVISIBLE neighbour is simply absent from `observed` (the scan never saw it) → nothing.
        ];
        let out = neighbour_threats(&owned_rooms, &observed, DefensePolicy::default(), cheby);
        assert!(out.is_empty(), "unarmed/beyond-leash dropped; invisible never present");
    }

    /// A SWARM of N hostiles in ONE neighbour room is ONE `ObservedRoom` (the caller summed the danger) →
    /// ONE `Threat` → ONE `Secure` objective downstream — never N. (The bot folds the swarm into a single
    /// `ObservedRoom` per room; this asserts the helper preserves that one-room-one-threat contract.)
    #[test]
    fn neighbour_swarm_is_one_threat_room() {
        let owned_rooms = [owned((0, 0), 1.0)];
        // One room, danger already summed across the swarm (e.g. 5 attackers ⇒ 150 dps).
        let observed = [obs((1, 0), true, 150.0)];
        let out = neighbour_threats(&owned_rooms, &observed, DefensePolicy::default(), cheby);
        assert_eq!(out.len(), 1, "a swarm in one room is a single threat-room");
        assert_eq!(out[0], threat((1, 0), 150.0));
    }

    /// An owned room (dist 0) accidentally fed as an `ObservedRoom` is skipped — owned threats come from the
    /// existing owned-room scan, so the neighbour builder never double-counts one.
    #[test]
    fn neighbour_excludes_owned_rooms_no_double_count() {
        let owned_rooms = [owned((0, 0), 1.0)];
        let observed = [obs((0, 0), true, 5.0)];
        let out = neighbour_threats(&owned_rooms, &observed, DefensePolicy::default(), cheby);
        assert!(out.is_empty(), "owned rooms are covered by the owned-room scan, not the neighbour builder");
    }

    /// No owned rooms → nothing to defend → no neighbour threats (the leash is measured from an owned room).
    #[test]
    fn neighbour_no_owned_rooms_yields_none() {
        let out = neighbour_threats(&[], &[obs((1, 0), true, 5.0)], DefensePolicy::default(), cheby);
        assert!(out.is_empty());
    }

    /// Deterministic: same input → same output, in the caller's stable order.
    #[test]
    fn neighbour_threats_are_deterministic() {
        let owned_rooms = [owned((0, 0), 1.0)];
        let observed = [obs((1, 0), true, 5.0), obs((-1, 0), true, 7.0), obs((2, 0), true, 3.0)];
        let out = neighbour_threats(&owned_rooms, &observed, DefensePolicy::default(), cheby);
        assert_eq!(out, neighbour_threats(&owned_rooms, &observed, DefensePolicy::default(), cheby));
    }

    // ── observe_neighbours (ADR 0027 P0 — the lifted OBSERVATION decision) ───────────────────────────────

    use screeps::Part;

    /// Build a `RawObservation` from owned `(0,0)`: compute the Chebyshev nearest-owned distance for `r`.
    fn raw<'a>(r: Room, bodies: &'a [Vec<Part>], visible: bool, is_owned: bool) -> RawObservation<'a, Room> {
        RawObservation {
            room: r,
            hostile_bodies: bodies,
            visible,
            is_owned,
            nearest_owned_dist: Some(cheby((0, 0), r)),
            tower_danger: 0.0,
        }
    }

    /// Like [`raw`] but with a hostile-TOWER threat (ADR 0037 T1) — the towered-neighbour case.
    fn raw_towered<'a>(r: Room, bodies: &'a [Vec<Part>], tower_danger: f32) -> RawObservation<'a, Room> {
        RawObservation {
            room: r,
            hostile_bodies: bodies,
            visible: true,
            is_owned: false,
            nearest_owned_dist: Some(cheby((0, 0), r)),
            tower_danger,
        }
    }

    /// The lifted armed-check: armed creeps (Attack/RangedAttack), dismantlers (Work), controller-attackers
    /// (Claim), healers (Heal) all warrant a defender; pure scouts/haulers (Move/Carry/Tough) do not.
    #[test]
    fn observe_armed_check_matches_lifted_predicate() {
        assert!(hostile_warrants_defender(&[Part::Attack, Part::Move]));
        assert!(hostile_warrants_defender(&[Part::RangedAttack, Part::Move]));
        assert!(hostile_warrants_defender(&[Part::Work, Part::Move]));
        assert!(hostile_warrants_defender(&[Part::Claim, Part::Move])); // controller-attacker
        assert!(hostile_warrants_defender(&[Part::Heal, Part::Move]));
        assert!(!hostile_warrants_defender(&[Part::Move, Part::Carry]));
        assert!(!hostile_warrants_defender(&[Part::Tough, Part::Move]));
        assert!(!hostile_warrants_defender(&[]));
    }

    /// The lifted per-part danger estimate (Attack=30, RangedAttack=10, else 0).
    #[test]
    fn observe_danger_estimate_matches_lifted_fold() {
        assert_eq!(estimate_danger(&[Part::Attack, Part::Attack, Part::Move]), 60.0);
        assert_eq!(estimate_danger(&[Part::RangedAttack, Part::Move]), 10.0);
        assert_eq!(estimate_danger(&[Part::Heal, Part::Work, Part::Claim, Part::Move]), 0.0);
    }

    /// Dismantle danger is gated on our owning structures the creep can tear down — the fix for the live
    /// `Secure ... (dps=0, ... count=3)` (3 pure dismantlers reading as harmless) while NOT over-defending a
    /// room where we hold nothing. The neighbour creep-DPS path (`estimate_danger`) still keeps WORK at 0.
    #[test]
    fn dismantle_danger_gated_on_our_structures() {
        // We own structures here → WORK breaches → danger scales with WORK count.
        assert_eq!(dismantle_danger(5, true), 5.0 * DISMANTLE_DANGER_PER_WORK);
        // Nothing of ours to dismantle → harmless (same principle as the ADR 0037 neighbour suppression).
        assert_eq!(dismantle_danger(5, false), 0.0);
        assert_eq!(dismantle_danger(0, true), 0.0);
        // The neighbour creep-DPS estimate still ignores WORK (towered-neighbour suppression).
        assert_eq!(estimate_danger(&[Part::Work, Part::Work, Part::Move]), 0.0);
    }

    /// An ARMED hostile in a VISIBLE, NON-OWNED, within-leash room becomes one `ObservedRoom` with the
    /// folded armed flag + summed danger — the core P0 decision the live path used to do inline.
    #[test]
    fn observe_armed_visible_within_leash_becomes_one_observed_room() {
        let attacker = vec![Part::Attack, Part::Attack, Part::Move]; // danger 60
        let bodies = [attacker];
        let observations = [raw((1, 0), &bodies, true, false)];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], obs((1, 0), true, 60.0));
    }

    /// UNARMED (scout-only), INVISIBLE, OWNED, and BEYOND-LEASH rooms each produce NO `ObservedRoom` — the
    /// four filters the live path applied inline.
    #[test]
    fn observe_drops_unarmed_invisible_owned_and_beyond_leash() {
        let scout = vec![Part::Move, Part::Carry];
        let attacker = vec![Part::Attack, Part::Move];
        let scout_bodies = [scout];
        let attacker_bodies = [attacker];
        let observations = [
            raw((1, 0), &scout_bodies, true, false),    // unarmed → dropped
            raw((1, 1), &attacker_bodies, false, false), // invisible → dropped
            raw((0, 0), &attacker_bodies, true, true),   // owned → dropped (owned-room scan covers it)
            raw((3, 0), &attacker_bodies, true, false),  // beyond default leash=2 → dropped
        ];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert!(out.is_empty(), "all four filters drop their room");
    }

    /// A SWARM of N armed hostiles in ONE room is ONE `ObservedRoom` (danger SUMMED across the bodies), never
    /// N — so it produces one threat → one Secure downstream.
    #[test]
    fn observe_swarm_is_one_room_with_summed_danger() {
        let bodies = vec![
            vec![Part::Attack, Part::Move],        // 30
            vec![Part::Attack, Part::Move],        // 30
            vec![Part::RangedAttack, Part::Move],  // 10
        ];
        let observations = [raw((1, 0), &bodies, true, false)];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert_eq!(out.len(), 1, "a swarm in one room is a single ObservedRoom");
        assert_eq!(out[0], obs((1, 0), true, 70.0), "danger summed across the swarm");
    }

    // ── ADR 0037 T1: tower-aware neighbour threat SIGNAL (distinct from the creep danger) ────────────────

    /// ── ADR 0037 T2 (SUPPRESS the bare defense reflex) — the exact live W13N56 symptom ──
    /// A towered neighbour whose ONLY creep is a Work-only body (a dismantler — armed per
    /// `hostile_warrants_defender`, but danger 0 in `estimate_danger`) sitting under hostile towers is NOT
    /// attacking US. `observe_neighbours` must now SUPPRESS it: NO `ObservedRoom` → no `Threat` → no bare
    /// Secure (a floor defender can never beat towers; T3 routes it to offense). RED before T2 (it emitted).
    #[test]
    fn observe_suppresses_bare_secure_for_danger0_creep_under_towers() {
        // A Work-only creep: armed (dismantler warrants a defender) but danger 0. This is the exact live
        // W13N56 case: danger==0 && tower_danger>0 → the suppress gate drops the whole room.
        let dismantler = vec![Part::Work, Part::Move];
        let bodies = [dismantler];
        let observations = [raw_towered((1, 0), &bodies, 300.0)];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert!(
            out.is_empty(),
            "a danger==0 creep sitting under hostile towers is NOT attacking us — the bare Secure is suppressed (T2)"
        );
    }

    /// PRESERVE (i): a REAL attacker (danger>0) in a towered room STILL emits — a real attacker adjacent IS a
    /// threat, towered or not. The tower signal rides through DISTINCT from the (non-zero) creep danger.
    #[test]
    fn observe_preserves_real_attacker_even_under_towers() {
        let attacker = vec![Part::Attack, Part::Move]; // danger 30 (>0)
        let bodies = [attacker];
        let observations = [raw_towered((1, 0), &bodies, 300.0)];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert_eq!(out.len(), 1, "a REAL attacker (danger>0) under towers STILL emits — the suppress gate spares it");
        assert!(out[0].armed);
        assert_eq!(out[0].danger, 30.0, "the creep danger is UNCHANGED — the attacker scores its DPS");
        assert_eq!(out[0].tower_danger, 300.0, "the tower threat is carried DISTINCT (for T3), not folded into danger");
    }

    /// PRESERVE (ii): a danger==0 creep (Work/Claim/Heal) in a NON-towered room STILL emits — a dismantler
    /// razing our remote / a claimer attacking our reservation is a genuine non-tower threat that warrants a
    /// defender. Only the `danger==0 && tower_danger>0` case is dropped; `tower_danger==0` is NOT.
    #[test]
    fn observe_preserves_danger0_creep_in_non_towered_room() {
        let claimer = vec![Part::Claim, Part::Move]; // armed, danger 0, but NO towers
        let bodies = [claimer];
        let observations = [raw_towered((1, 0), &bodies, 0.0)];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert_eq!(out.len(), 1, "a danger==0 creep in a NON-towered room STILL emits (a genuine non-tower threat)");
        assert!(out[0].armed, "a Claim controller-attacker still warrants a defender");
        assert_eq!(out[0].danger, 0.0, "its creep danger is 0 (estimate_danger scores only Attack/RangedAttack)");
        assert_eq!(out[0].tower_danger, 0.0, "no towers ⇒ the suppress gate does not fire");
    }

    /// A neighbour with NO towers carries `tower_danger == 0` — the signal is only present when scouted
    /// towers exist. The creep danger is independent (an attacker still scores its DPS).
    #[test]
    fn observe_no_towers_carries_zero_tower_danger() {
        let attacker = vec![Part::Attack, Part::Move]; // danger 30
        let bodies = [attacker];
        let observations = [raw_towered((1, 0), &bodies, 0.0)];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].danger, 30.0, "the creep danger is unaffected by the tower signal");
        assert_eq!(out[0].tower_danger, 0.0, "no scouted towers ⇒ tower_danger is 0");
    }

    /// The tower signal is PURELY ADDITIVE: it does NOT alter the armed-check, the creep danger, OR the
    /// downstream emission. Two observations identical except for `tower_danger` yield the SAME creep
    /// danger + armed verdict, and the same `neighbour_threats`/`emit_defense` emission (priority/room/order
    /// byte-identical) — only the carried `tower_danger` differs. Guards the "T1 gates nothing" contract.
    #[test]
    fn tower_danger_does_not_alter_the_emission() {
        let attacker = vec![Part::Attack, Part::Move]; // danger 30, armed
        let bodies = [attacker];
        let no_tower = [raw_towered((1, 0), &bodies, 0.0)];
        let with_tower = [raw_towered((1, 0), &bodies, 500.0)];

        let obs_no = observe_neighbours(&no_tower, DefensePolicy::default());
        let obs_yes = observe_neighbours(&with_tower, DefensePolicy::default());
        // armed + creep danger are byte-identical; only tower_danger differs.
        assert_eq!(obs_no[0].armed, obs_yes[0].armed);
        assert_eq!(obs_no[0].danger, obs_yes[0].danger);
        assert_ne!(obs_no[0].tower_danger, obs_yes[0].tower_danger);

        // The full emission is UNCHANGED by the tower signal (T1 gates nothing).
        let owned_rooms = [owned((0, 0), 1.0)];
        let threats_no = neighbour_threats(&owned_rooms, &obs_no, DefensePolicy::default(), cheby);
        let threats_yes = neighbour_threats(&owned_rooms, &obs_yes, DefensePolicy::default(), cheby);
        let emit_no = emit_defense(&owned_rooms, &threats_no, DefensePolicy::default(), cheby);
        let emit_yes = emit_defense(&owned_rooms, &threats_yes, DefensePolicy::default(), cheby);
        assert_eq!(emit_no, emit_yes, "the Secure emission (room/priority/order) is byte-identical — T1 gates nothing");
        // The threat carries the tower signal through (distinct), while the emission ignores it.
        assert_eq!(threats_yes[0].tower_danger, 500.0);
        assert_eq!(threats_no[0].tower_danger, 0.0);
    }

    /// Deterministic: same input → same output, in the caller's stable order.
    #[test]
    fn observe_neighbours_is_deterministic() {
        let a = vec![Part::Attack, Part::Move];
        let ab = [a];
        let observations = [
            raw((1, 0), &ab, true, false),
            raw((-1, 0), &ab, true, false),
            raw((2, 0), &ab, true, false),
        ];
        let out = observe_neighbours(&observations, DefensePolicy::default());
        assert_eq!(out, observe_neighbours(&observations, DefensePolicy::default()));
    }
}
