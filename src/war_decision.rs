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
        Threat { room: r, danger }
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
}
