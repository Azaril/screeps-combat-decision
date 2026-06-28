//! Pure rally / boundary-cohesion gates (P-OBJ #23 / ADR 0028) — the "wait + group up, then depart and
//! cross as a bloc" decisions, lifted out of the bot (`military::formation`) so the live path AND the
//! offline lifecycle harness share ONE implementation (parity, like `decide_squad` / `lifecycle`). JS-free
//! value-type math over `screeps::Position` — no `game::*`, no ECS.

use screeps::{Position, RoomCoordinate, RoomName};

/// Cohesion quorum: this fraction of the *living* (positioned) squad must be gathered near the boundary
/// before the box crosses into a contested room, so fast creeps don't trickle in one at a time.
pub const STRICT_QUORUM_RATIO: f32 = 0.75;

/// READY to leave the rally and travel to the objective as a bloc. Until then the manager holds the squad
/// at home and groups up; it must NOT send a lone lead toward the target (a single creep can't solo the
/// objective, dies, and the squad wipes → re-field loop — the P-OBJ #23 invader no-engage root cause).
/// Measured against the objective's REQUESTED slot count so death-degrade of the layout can't shrink
/// "full". `requested_slots == 0` (unknown) does not gate (preserves legacy behaviour).
pub fn squad_ready_to_depart(member_positions: &[Option<Position>], requested_slots: usize) -> bool {
    if requested_slots == 0 {
        return true;
    }
    // Enough members are PRESENT (spawned + in the world) to depart. Count present members, NOT
    // "all members" — an EXTRA still-spawning member (left over when the objective's requested size
    // oscillates DOWN, e.g. 2→1) must not jam the gate: the squad departs once the requested count is
    // present, orphaning any surplus. (Counting all-Some made an oscillating-size objective rally forever
    // at "present=requested but holding" — the live W9N8 stuck-at-1/1 bug.)
    member_positions.iter().filter(|p| p.is_some()).count() >= requested_slots
}

/// Minimum viable group to commit to a fight: a lone member is picked off, a pair (a fighter + a healer) can
/// trade and sustain. The quorum floor so we never deploy a solo to a fight it can't survive.
pub const MIN_VIABLE_GROUP: usize = 2;

/// READY to DEPLOY as a grouped bloc at a QUORUM (not the full roster). For objectives where waiting for the
/// LAST member deadlocks — it may never spawn under spawn contention — yet committing a single member loses it
/// under-powered (operator 2026-06-27: defenders must group up, not trickle in one-at-a-time and die). Deploy
/// once a quorum is present: `STRICT_QUORUM_RATIO` of the requested roster, floored at `MIN_VIABLE_GROUP`,
/// capped at the requested count (a 1-slot objective deploys its 1). The remaining members reinforce by
/// formation-following the deployed bloc. (The all-or-nothing [`squad_ready_to_depart`] stays for an OFFENSE
/// bloc that must cross into a contested room together; ADR 0030 will tune the quorum by lifetime/wave.)
pub fn squad_ready_to_depart_at_quorum(member_positions: &[Option<Position>], requested_slots: usize) -> bool {
    if requested_slots == 0 {
        return true;
    }
    let present = member_positions.iter().filter(|p| p.is_some()).count();
    let quorum = ((requested_slots as f32 * STRICT_QUORUM_RATIO).ceil() as usize)
        .max(MIN_VIABLE_GROUP)
        .min(requested_slots);
    present >= quorum
}

/// Whether the combat DTOs for a target room come from a TRUSTWORTHY source — i.e. empty hostiles/towers
/// genuinely mean "clear", not merely "unseen". An offense target is reliable when EITHER it is `mapped`
/// (a scouted `RoomData` ECS entity whose cached last-scouted intel persists even without current live
/// vision) OR it is `live_visible` (a member stands in it this tick). Only a GENUINELY-UNKNOWN room
/// (unmapped AND not live-visible) is unreliable.
///
/// This is the stability property the rally-oscillation fix turns on: a MAPPED target stays reliable
/// REGARDLESS of live vision, so `target_is_uncontested` no longer flaps as a solo member crosses the
/// W6N5↔W7N5 boundary (toggling raw `game::rooms().get().is_some()` live vision). Cached intel is the
/// single source of truth; the cache outlives the transient loss of vision.
pub fn rally_intel_reliable(mapped: bool, live_visible: bool) -> bool {
    mapped || live_visible
}

/// Whether a target room is PROVEN UNCONTESTED — safe to deploy a sub-roster quorum into rather than
/// holding the full all-or-nothing rally bloc. The classification (rally-stall fix): an undefended,
/// towerless, not-safe-moded room for which we have TRUSTWORTHY intel.
///
/// `intel_reliable` is LOAD-BEARING — when the target DTOs come from a GENUINELY-UNKNOWN room (unmapped
/// AND no live vision) the empty hostiles/structures DTOs simply mean we have no information, NOT that the
/// room is clear; gating on `no_hostiles` alone would mis-classify a defended-but-unseen room as
/// uncontested and trickle a sub-roster into it to be picked off. So we require TRUSTWORTHY intel (cached
/// scouted `RoomData` OR current live vision — see [`rally_intel_reliable`]) AND no hostiles AND no hostile
/// towers AND no enemy safe mode. Any of those false ⇒ keep the hard full-roster rally.
///
/// NOTE (rally-oscillation fix): the first param was `room_visible` (raw current live vision) which FLAPPED
/// as a solo member crossed a room boundary, flipping `uncontested` and oscillating the shared rally room.
/// It is now `intel_reliable`, which is stably true for a mapped (scouted) target — breaking the feedback
/// loop at its source. The boolean logic is UNCHANGED; only what the caller PASSES changed.
pub fn target_is_uncontested(intel_reliable: bool, no_hostiles: bool, no_hostile_towers: bool, no_enemy_safe_mode: bool) -> bool {
    intel_reliable && no_hostiles && no_hostile_towers && no_enemy_safe_mode
}

/// Select the rally/deploy gate (rally-stall fix). For a PROVEN-uncontested target the squad need not wait
/// for the LAST member (which may lose the within-tier spawn race on a young colony, deadlocking the
/// all-or-nothing gate forever — the live W7N7 stall) — an oversized force advancing + dismantling as
/// members arrive is HARMLESS against an undefended objective. So deploy at the MIN-VIABLE group: enough to
/// not send a lone creep that could get unluckily picked off (`MIN_VIABLE_GROUP`), but NOT 0.75 of the
/// roster (the survival-axis quorum, which is for a DEFENDED room and would re-introduce the very deadlock
/// against an undefended one — 3/5 < ceil(0.75·5)=4 would still hold). Capped at the requested count so a
/// 1-slot objective deploys its 1. For ANY contested or UNSEEN target keep the full-roster
/// [`squad_ready_to_depart`] (the hard-rally protection: a defended room must be entered together).
pub fn ready_to_depart_gate(member_positions: &[Option<Position>], requested_slots: usize, uncontested: bool) -> bool {
    if !uncontested {
        return squad_ready_to_depart(member_positions, requested_slots);
    }
    if requested_slots == 0 {
        return true;
    }
    let present = member_positions.iter().filter(|p| p.is_some()).count();
    let min_viable = MIN_VIABLE_GROUP.min(requested_slots);
    present >= min_viable
}

/// Whether to HOLD the squad's virtual anchor at a room boundary for cohesion (don't advance across until
/// enough members are gathered near the edge), instead of letting fast creeps trickle into a contested
/// room one at a time. The P-OBJ #23 fix lives here: counts ONLY members with a resolved position — a
/// still-spawning member (`None`, no body in the world) must NEVER inflate the quorum denominator, or it
/// jams the gate so a lone in-room lead is frozen at the edge forever. Returns false when not at a
/// boundary, or with ≤1 member present (a lone lead just crosses).
pub fn should_hold_at_boundary(member_positions: &[Option<Position>], virtual_pos: Position, destination: Position) -> bool {
    let positioned: Vec<Position> = member_positions.iter().filter_map(|p| *p).collect();
    let living_count = positioned.len();
    let at_room_boundary = virtual_pos.room_name() != destination.room_name();
    if !at_room_boundary || living_count <= 1 {
        return false;
    }
    let vp_room = virtual_pos.room_name();
    // Gathered = in the anchor's room OR already across into the destination.
    let gathered = positioned
        .iter()
        .filter(|p| p.room_name() == vp_room || p.room_name() == destination.room_name())
        .count();
    let quorum_met = gathered as f32 >= living_count as f32 * STRICT_QUORUM_RATIO;
    // Near-edge = already crossed (different room) OR within the edge band toward the destination.
    let near_edge = positioned
        .iter()
        .filter(|p| p.room_name() != vp_room || is_near_room_edge_toward(**p, destination))
        .count();
    let near_edge_quorum = near_edge as f32 >= living_count as f32 * STRICT_QUORUM_RATIO;
    !(quorum_met && near_edge_quorum)
}

// ─── Shared rally / gather-quorum kernel (ADR 0028 K0 movement-stall fix) ───────────────────────────
//
// DECOUPLE long-distance TRAVEL from FORMATION (operator-directed, 2026-06-28). A squad spawns from many
// homes (multi-home spawn preserved), each member paths SOLO to ONE shared rally Position near the target,
// and only ASSAULTS in formation once a quorum has gathered there. The gather quorum below is the SINGLE
// source of truth shared by the live bot AND the agent-sim (the sim's `near_anchor >= ADVANCE_QUORUM`
// assault gate IS this kernel) so the two cohesion paths can never drift again — that drift was the root
// cause of the frozen-formation-anchor stall.

/// Cohesion radius (Chebyshev) within which a member counts as GATHERED at the shared rally — the loose
/// staging cluster, matching the sim's `LOOSE_RADIUS`. Wide enough that members don't have to stack on one
/// tile (the sim has no shoving), tight enough that the bloc departs together.
pub const RALLY_GATHER_RADIUS: u32 = 3;

/// Fraction of the LIVING roster that must be gathered at the shared rally before a CONTESTED assault
/// advances — the sim's `ADVANCE_QUORUM`. Lifted here so the bot and the sim share one constant.
pub const GATHER_QUORUM_RATIO: f32 = 0.75;

/// How many living (positioned) members are gathered within `radius` of the shared `rally` point — the
/// staging-cluster count. Pure value-math over `Position` (Chebyshev range), no terrain. The instrument
/// the gather quorum is measured against; the sim measures the identical thing against its anchor.
pub fn members_gathered_at(member_positions: &[Option<Position>], rally: Position, radius: u32) -> usize {
    member_positions.iter().filter_map(|p| *p).filter(|p| p.get_range_to(rally) <= radius).count()
}

/// THE UNIFIED gather-quorum kernel (movement-stall fix). Returns whether enough living members have
/// converged on the shared `rally` point to transition from SOLO travel to a grouped ASSAULT.
///
/// Both the live bot AND the agent-sim call this so their cohesion logic cannot drift (the root-cause
/// regression). Semantics:
/// - `requested_slots == 0` (unknown roster) ⇒ do not gate (legacy parity).
/// - `uncontested` (a proven-clear target — nothing shoots back) ⇒ a MIN-VIABLE quorum may trickle in:
///   even ONE gathered member is enough (an oversized force advancing as members arrive is harmless), and
///   no fighter is required (a lone dismantler can raze an undefended core).
/// - CONTESTED ⇒ require the (near-)full roster gathered at the rally (`GATHER_QUORUM_RATIO` of the LIVING
///   members, floored so a lone member never solo-assaults a defended room) AND at least one FIGHTER
///   present (no healer-only assault). A defended room must be entered together or the trickle is picked off.
pub fn gather_quorum_met(
    member_positions: &[Option<Position>],
    rally: Position,
    requested_slots: usize,
    uncontested: bool,
    has_fighter_gathered: bool,
    radius: u32,
) -> bool {
    if requested_slots == 0 {
        return true;
    }
    let gathered = members_gathered_at(member_positions, rally, radius);
    if uncontested {
        // Uncontested: a single gathered member may trickle in (nothing shoots back); no fighter required.
        return gathered >= 1;
    }
    // Contested: the (near-)full LIVING roster must be massed at the rally AND a fighter must be present.
    let living = member_positions.iter().filter(|p| p.is_some()).count();
    if living == 0 {
        return false;
    }
    let quorum = ((living as f32 * GATHER_QUORUM_RATIO).ceil() as usize).max(MIN_VIABLE_GROUP).min(living);
    gathered >= quorum && has_fighter_gathered
}

/// Compute the squad's ONE shared rally/staging Position for an approach toward `target` from the squad's
/// current `approach` position (its centroid / lead). DETERMINISTIC pure value-math (no `game::*`), so the
/// bot derives it fresh each tick — no stored field, no `WORLD_FORMAT_VERSION` bump.
///
/// - UNCONTESTED target ⇒ stage at the TARGET ROOM ENTRANCE: the target room's centre. Nothing shoots
///   back, so members may converge inside the target room and trickle onto the objective.
/// - CONTESTED target ⇒ stage ONE ROOM SHORT of the target, on the approach side (out of tower range): the
///   centre of the neighbour room between the approach and the target. If the approach is already in the
///   target room (we arrived contested) fall back to the target-room centre (the in-room brain takes over).
///
/// The staging tile is the room CENTRE (25,25) — safely off the exposed border ring and a stable gather
/// point all members can path to. Members travel SOLO here; the assault advances rally→target only once the
/// gather quorum fires.
pub fn shared_rally_point(approach: Position, target: Position, uncontested: bool) -> Position {
    let centre = |room: RoomName| {
        Position::new(
            RoomCoordinate::new(25).expect("25 is valid"),
            RoomCoordinate::new(25).expect("25 is valid"),
            room,
        )
    };
    if uncontested || approach.room_name() == target.room_name() {
        return centre(target.room_name());
    }
    // Contested + still outside the target room: stage one room SHORT, on the approach side. Step the room
    // coordinate ONE room from the target toward the approach (Chebyshev), so the staging room is the
    // neighbour the squad will cross from — out of the target's tower range. `RoomName - RoomName` is the
    // (dx,dy) room-delta; its sign points from the target toward the approach.
    let delta = approach.room_name() - target.room_name();
    let dx = delta.0.signum();
    let dy = delta.1.signum();
    if dx == 0 && dy == 0 {
        return centre(target.room_name()); // same room (shouldn't reach here) — stage in-room
    }
    let staging_room = target.room_name() + (dx, dy);
    centre(staging_room)
}

/// Check if a position is near the room edge leading toward a destination in another room. "Near" means
/// within 8 tiles of the relevant border.
fn is_near_room_edge_toward(pos: Position, destination: Position) -> bool {
    let (cur_wx, cur_wy) = pos.world_coords();
    let (dst_wx, dst_wy) = destination.world_coords();
    let pos_room = pos.room_name();
    let dst_room = destination.room_name();

    if pos_room == dst_room {
        return true; // Already in the destination room.
    }

    let x = pos.x().u8();
    let y = pos.y().u8();
    let near_threshold = 8;

    // Check which direction we need to go based on world coordinates.
    let room_dx = (dst_wx - cur_wx).signum();
    let room_dy = (dst_wy - cur_wy).signum();

    let near_x_edge = if room_dx > 0 {
        x >= 49 - near_threshold
    } else if room_dx < 0 {
        x <= near_threshold
    } else {
        true // Same x-axis; no x-boundary to cross.
    };

    let near_y_edge = if room_dy > 0 {
        y >= 49 - near_threshold
    } else if room_dy < 0 {
        y <= near_threshold
    } else {
        true // Same y-axis; no y-boundary to cross.
    };

    near_x_edge && near_y_edge
}

#[cfg(test)]
mod tests {
    use super::*;
    use screeps::{RoomCoordinate, RoomName};

    fn pos(x: u8, y: u8, room: &str) -> Position {
        Position::new(
            RoomCoordinate::new(x).unwrap(),
            RoomCoordinate::new(y).unwrap(),
            room.parse::<RoomName>().unwrap(),
        )
    }

    /// P-OBJ #23 ROOT-CAUSE regression: a still-spawning member (position `None`) must NOT jam the boundary
    /// cohesion gate. Pre-fix it inflated `living_count` and failed every quorum, freezing a lone in-room
    /// lead at the room edge → the squad never massed → never engaged the invader core. The gate must count
    /// only members present in the world.
    #[test]
    fn boundary_does_not_hold_for_a_spawning_member() {
        let vp = pos(25, 25, "W3N6");
        let dest = pos(25, 25, "W3N5");
        let lead = pos(25, 2, "W3N5"); // already crossed
        assert!(
            !should_hold_at_boundary(&[Some(lead), None], vp, dest),
            "a still-spawning (None) member must not jam the boundary hold for a lone in-room lead"
        );
    }

    /// The gate still HOLDS when a real (positioned) squadmate lags far from the edge — cohesion preserved.
    #[test]
    fn boundary_holds_for_a_lagging_member() {
        let vp = pos(25, 25, "W3N6");
        let dest = pos(25, 25, "W3N5");
        let lead = pos(25, 2, "W3N5");
        let lagger = pos(25, 25, "W3N6"); // home centre, far from edge
        assert!(
            should_hold_at_boundary(&[Some(lead), Some(lagger)], vp, dest),
            "hold while a real member lags far from the boundary edge"
        );
    }

    /// The gate RELEASES once the whole (positioned) squad has crossed into the destination room.
    #[test]
    fn boundary_releases_when_all_crossed() {
        let vp = pos(25, 25, "W3N6");
        let dest = pos(25, 25, "W3N5");
        let a = pos(25, 2, "W3N5");
        let b = pos(26, 2, "W3N5");
        assert!(
            !should_hold_at_boundary(&[Some(a), Some(b)], vp, dest),
            "release once the whole squad has crossed"
        );
    }

    /// P-OBJ #23 rally gate: the squad departs home ONLY when the full roster has spawned AND every member
    /// is present in the world — otherwise it holds + groups up. This stops the lone slot-0 lead from
    /// creeping in alone, dying, and tripping the wipe → re-field loop.
    #[test]
    fn squad_ready_only_when_full_roster_present() {
        let p = pos(25, 25, "W1N1");
        assert!(squad_ready_to_depart(&[Some(p), Some(p)], 2), "full + all present → depart");
        assert!(!squad_ready_to_depart(&[Some(p), None], 2), "a still-spawning member → hold + rally");
        assert!(!squad_ready_to_depart(&[Some(p)], 2), "roster not fully spawned → hold + rally");
        assert!(squad_ready_to_depart(&[Some(p)], 0), "unknown roster size → do not gate (legacy)");
        // An EXTRA still-spawning member (requested oscillated down 2→1) must NOT jam the gate: the
        // requested count IS present, so depart (orphaning the surplus). The live W9N8 stuck-at-1/1 bug.
        assert!(squad_ready_to_depart(&[Some(p), None], 1), "requested present + surplus spawning → depart");
    }

    /// Quorum deploy (operator 2026-06-27): a defender deploys when GROUPED (a quorum), not one-at-a-time
    /// (picked off under-powered) and not strictly full (the N-1 deadlock when the last member never spawns).
    #[test]
    fn quorum_deploys_grouped_not_solo_not_full() {
        let p = pos(25, 25, "W1N1");
        // 4-member roster: deploy at the 3/4 quorum (don't wait for the unspawnable 4th); hold below it.
        assert!(!squad_ready_to_depart_at_quorum(&[Some(p), None, None, None], 4), "1/4 lone trickle → hold");
        assert!(!squad_ready_to_depart_at_quorum(&[Some(p), Some(p), None, None], 4), "2/4 below quorum → hold");
        assert!(squad_ready_to_depart_at_quorum(&[Some(p), Some(p), Some(p), None], 4), "3/4 quorum → deploy");
        // Small rosters: a duo deploys at the min viable group; a solo deploys its 1; a lone-of-2 holds.
        assert!(squad_ready_to_depart_at_quorum(&[Some(p), Some(p)], 2), "2/2 → deploy");
        assert!(!squad_ready_to_depart_at_quorum(&[Some(p), None], 2), "1/2 below the min viable group → hold");
        assert!(squad_ready_to_depart_at_quorum(&[Some(p)], 1), "1/1 single-slot objective → deploy");
        assert!(squad_ready_to_depart_at_quorum(&[], 0), "unknown roster → do not gate");
    }

    /// FIX 1 (rally-stall): a 3/5-present force DEPLOYS when the target is proven-uncontested (the
    /// quorum gate) but HOLDS for the full roster when the target is contested/unseen (the W7N7 stall:
    /// member 4/5 loses the spawn race forever, so the all-or-nothing gate never releases against an
    /// undefended core). Mirrors `squad_ready_only_when_full_roster_present` + the quorum test.
    #[test]
    fn gate_quorum_when_uncontested_full_roster_when_contested() {
        let p = pos(25, 25, "W7N7");
        let three_of_five = [Some(p), Some(p), Some(p), None, None];
        assert!(
            ready_to_depart_gate(&three_of_five, 5, true),
            "3/5 present + UNCONTESTED → deploy at quorum (advance + dismantle as members arrive)"
        );
        assert!(
            !ready_to_depart_gate(&three_of_five, 5, false),
            "3/5 present + CONTESTED/UNSEEN → hold for the full roster (enter together or be picked off)"
        );
        // Full roster departs either way; the gate never blocks a complete squad.
        let full = [Some(p), Some(p), Some(p), Some(p), Some(p)];
        assert!(ready_to_depart_gate(&full, 5, true), "5/5 uncontested → depart");
        assert!(ready_to_depart_gate(&full, 5, false), "5/5 contested → depart (full roster present)");
    }

    // ── Movement-stall fix (ADR 0028 K0): SOLO travel to a SHARED rally, then ASSAULT in formation ──

    /// One member at the home centre, far from the rally; one already at the rally. The shared rally is on
    /// the approach to the target. Stepping the (pure) convergence model, both members must end up at the
    /// shared rally (gathered), and only THEN may the assault anchor advance rally→target.
    ///
    /// SPATIAL repro: members in DIFFERENT rooms (W2N9 + W3N2) solo-travel to a shared rally, converge,
    /// then the anchor advances toward the target room (W4N2) crossing borders. RED before the fix because
    /// the bug rallied each member to its OWN home and froze the box-formation anchor → no convergence.
    #[test]
    fn scattered_members_converge_at_shared_rally_then_assault_advances() {
        // Two members in different rooms; a shared rally on the approach; a target a room beyond the rally.
        let rally = pos(5, 25, "W3N2"); // safe staging on the approach to the target
        let target = pos(25, 25, "W4N2");
        let mut a = pos(25, 25, "W2N9"); // far member, different room
        let mut b = pos(8, 25, "W3N2"); // near member, already in the rally room

        // 1. SOLO TRAVEL: each member steps toward the SHARED rally independently (no cross-room
        //    box-formation cohesion). Model one Chebyshev step/tick toward the rally in world coords.
        let step_toward = |from: Position, to: Position| -> Position {
            if from == to {
                return from;
            }
            let (fx, fy) = from.world_coords();
            let (tx, ty) = to.world_coords();
            let (nx, ny) = (fx + (tx - fx).signum(), fy + (ty - fy).signum());
            // Reconstruct a Position from world coords (room + in-room offset).
            let room_x = nx.div_euclid(50);
            let room_y = ny.div_euclid(50);
            let in_x = nx.rem_euclid(50) as u8;
            let in_y = ny.rem_euclid(50) as u8;
            let room: RoomName = format!("W{}N{}", -room_x - 1, -room_y - 1).parse().unwrap();
            Position::new(RoomCoordinate::new(in_x).unwrap(), RoomCoordinate::new(in_y).unwrap(), room)
        };

        let mut gathered_tick = None;
        for t in 0..400 {
            // Gather quorum over BOTH members against the SHARED rally (contested → near-full + fighter).
            if gather_quorum_met(&[Some(a), Some(b)], rally, 2, false, true, RALLY_GATHER_RADIUS) {
                gathered_tick = Some(t);
                break;
            }
            a = step_toward(a, rally);
            b = step_toward(b, rally);
        }
        let gathered_tick = gathered_tick.expect("both members converge at the shared rally (solo travel)");
        assert!(a.get_range_to(rally) <= RALLY_GATHER_RADIUS, "member A reached the shared rally");
        assert!(b.get_range_to(rally) <= RALLY_GATHER_RADIUS, "member B reached the shared rally");
        // Both in the rally's room (room-distance to rally == 0).
        assert_eq!(a.room_name(), rally.room_name(), "A converged into the rally room");
        assert_eq!(b.room_name(), rally.room_name(), "B converged into the rally room");

        // 2. ASSAULT: once gathered, the anchor advances rally→target. Room-distance to the target room
        //    must strictly decrease and cross a border (W3N2 → W4N2).
        let room_dist = |p: Position| {
            let d = p.room_name() - target.room_name();
            d.0.unsigned_abs().max(d.1.unsigned_abs())
        };
        let mut anchor = rally;
        let start_dist = room_dist(anchor);
        assert!(start_dist >= 1, "the rally is at least one room short of the target (W3N2 → W4N2)");
        let mut crossed = false;
        for _ in gathered_tick..(gathered_tick + 200) {
            let prev_room = anchor.room_name();
            anchor = step_toward(anchor, target);
            if anchor.room_name() != prev_room {
                crossed = true;
            }
            if anchor.room_name() == target.room_name() {
                break;
            }
        }
        assert!(crossed, "the assault anchor crossed a room border advancing rally→target");
        assert!(room_dist(anchor) < start_dist, "the assault anchor strictly closed the room-distance to the target");
    }

    /// The shared rally geometry: uncontested → the target-room centre; contested → ONE room short on the
    /// approach side (out of tower range); arrived-contested → the target-room centre (the in-room brain).
    #[test]
    fn shared_rally_stages_short_when_contested_at_room_when_uncontested() {
        let target = pos(25, 25, "W4N2");
        let approach = pos(25, 25, "W2N2"); // two rooms WEST of the target (W3 is the neighbour)

        // Uncontested → stage at the target room centre.
        let r = shared_rally_point(approach, target, true);
        assert_eq!(r.room_name(), target.room_name(), "uncontested → target-room entrance");
        assert_eq!((r.x().u8(), r.y().u8()), (25, 25), "room centre");

        // Contested → stage ONE room short, toward the approach (W3N2, the neighbour between W2 and W4).
        let r = shared_rally_point(approach, target, false);
        assert_eq!(r.room_name(), "W3N2".parse::<RoomName>().unwrap(), "contested → one room short on the approach side");
        // The staging room is strictly closer to the approach than the target room is.
        let rd = |a: RoomName, b: RoomName| {
            let d = a - b;
            d.0.unsigned_abs().max(d.1.unsigned_abs())
        };
        assert!(rd(r.room_name(), approach.room_name()) < rd(target.room_name(), approach.room_name()), "staging is closer to the approach than the target");
        assert_eq!(rd(r.room_name(), target.room_name()), 1, "staging is exactly one room from the target");

        // Already in the target room (arrived contested) → fall back to the target-room centre.
        let arrived = pos(10, 10, "W4N2");
        assert_eq!(shared_rally_point(arrived, target, false).room_name(), target.room_name(), "arrived → target room");
    }

    /// The gather quorum: UNCONTESTED targets may trickle a single gathered member; CONTESTED targets
    /// require the (near-)full living roster massed at the shared rally AND a fighter present.
    #[test]
    fn gather_quorum_trickles_uncontested_but_masses_contested() {
        let rally = pos(25, 25, "W3N2");
        let near = pos(26, 25, "W3N2"); // within RALLY_GATHER_RADIUS
        let far = pos(25, 25, "W2N9"); // a room away — not gathered

        // Uncontested: ONE gathered member is enough (nothing shoots back); no fighter required.
        assert!(
            gather_quorum_met(&[Some(near), None], rally, 2, true, false, RALLY_GATHER_RADIUS),
            "uncontested → a single gathered member trickles in"
        );
        // Contested, only 1 of 2 gathered (the other a room away) → HOLD (don't feed it in piecemeal).
        assert!(
            !gather_quorum_met(&[Some(near), Some(far)], rally, 2, false, true, RALLY_GATHER_RADIUS),
            "contested → hold until the near-full roster is massed at the rally"
        );
        // Contested, both gathered + a fighter present → ASSAULT.
        assert!(
            gather_quorum_met(&[Some(near), Some(rally)], rally, 2, false, true, RALLY_GATHER_RADIUS),
            "contested + full roster gathered + fighter → assault"
        );
        // Contested, both gathered but NO fighter (healer-only) → HOLD (no healer-only assault).
        assert!(
            !gather_quorum_met(&[Some(near), Some(rally)], rally, 2, false, false, RALLY_GATHER_RADIUS),
            "contested + no fighter gathered → never a healer-only assault"
        );
        // Unknown roster size → do not gate (legacy parity).
        assert!(gather_quorum_met(&[], rally, 0, false, false, RALLY_GATHER_RADIUS), "unknown roster → ungated");
    }

    /// FIX 1 visibility guard: `target_is_uncontested` is true ONLY with POSITIVE room visibility. An
    /// UNSEEN room (empty DTOs because no vision, not because clear) is NEVER uncontested even though its
    /// hostiles/towers read empty — preventing a defended-but-unseen room from mis-classifying as clear
    /// and trickling a sub-roster in to be picked off.
    #[test]
    fn uncontested_requires_positive_visibility() {
        // Seen + clear + no towers + no safe mode → uncontested.
        assert!(target_is_uncontested(true, true, true, true), "visible + clear → uncontested");
        // Unseen: empty DTOs (no_hostiles/no_towers read true) must NOT count as uncontested.
        assert!(!target_is_uncontested(false, true, true, true), "UNSEEN room (empty DTOs) → NOT uncontested");
        // Each contesting condition vetoes uncontested on a visible room.
        assert!(!target_is_uncontested(true, false, true, true), "hostiles present → not uncontested");
        assert!(!target_is_uncontested(true, true, false, true), "hostile tower present → not uncontested");
        assert!(!target_is_uncontested(true, true, true, false), "enemy safe mode → not uncontested");
    }

    // ── RALLY-OSCILLATION FIX (intel-reliability, not raw live vision) ──────────────────────────────

    /// The stability property: a MAPPED (scouted) target is reliable REGARDLESS of current live vision,
    /// so its `intel_reliable` does not flap as a member crosses a room boundary. An UNMAPPED room is
    /// reliable only while we actually see it; a genuinely-unknown room (neither) stays guarded.
    #[test]
    fn intel_reliable_mapped_is_stable_regardless_of_live_vision() {
        // Mapped → reliable whether or not we currently see it (the stability that kills the oscillation).
        assert!(rally_intel_reliable(true, false), "mapped + no live vision → reliable (cached intel)");
        assert!(rally_intel_reliable(true, true), "mapped + live vision → reliable");
        // Unmapped → reliable only with current live vision.
        assert!(rally_intel_reliable(false, true), "unmapped + live-visible → reliable (we see it now)");
        // Genuinely unknown → never reliable (never trust no-vision emptiness).
        assert!(!rally_intel_reliable(false, false), "unmapped + unseen → NOT reliable (guarded)");
    }

    /// REPRODUCE the oscillation (RED before the fix): the OLD raw-live-vision input flaps
    /// `[false,true,false,true]` as a solo member crosses the room boundary, while cached hostiles are
    /// stably empty. Feeding that flapping flag straight into `target_is_uncontested` (the pre-fix
    /// behaviour) flips `uncontested` every tick → `shared_rally_point` flips the rally ROOM between the
    /// target (uncontested) and one room short (contested). Asserts the rally room OSCILLATES — the bug.
    #[test]
    fn shared_rally_oscillates_when_fed_raw_live_vision() {
        let approach = pos(25, 25, "W6N5"); // one room short of the target, on the approach side
        let target = pos(25, 25, "W7N5");
        // A solo member crossing the boundary makes raw live vision of the target room flap each tick.
        let raw_live_vision = [false, true, false, true];
        let no_hostiles = true; // cached intel is stably clear

        let rally_rooms: Vec<RoomName> = raw_live_vision
            .iter()
            .map(|&room_visible| {
                // PRE-FIX: the bot passed raw live vision as the first arg.
                let uncontested = target_is_uncontested(room_visible, no_hostiles, true, true);
                shared_rally_point(approach, target, uncontested).room_name()
            })
            .collect();

        let target_room = target.room_name();
        let short_room = "W6N5".parse::<RoomName>().unwrap();
        // The rally room flips target ⇄ one-room-short across the tick sequence — the stall feedback loop.
        assert_eq!(rally_rooms[0], short_room, "vision lost → contested → stage one room short");
        assert_eq!(rally_rooms[1], target_room, "vision gained → uncontested → stage at the target");
        assert_ne!(rally_rooms[0], rally_rooms[1], "the rally room OSCILLATES (the reproduced bug)");
        assert_eq!(rally_rooms[2], short_room);
        assert_eq!(rally_rooms[3], target_room);
    }

    /// PROVE the fix (GREEN): a MAPPED target → `intel_reliable` is stably TRUE (computed via
    /// `rally_intel_reliable(mapped=true, live)`) under the SAME flapping live vision and stable empty
    /// cached hostiles. The rally room is now CONSTANT across the whole tick sequence — the feedback loop
    /// is broken at its source.
    #[test]
    fn shared_rally_is_constant_for_a_mapped_target_under_flapping_vision() {
        let approach = pos(25, 25, "W6N5");
        let target = pos(25, 25, "W7N5");
        let flapping_live_vision = [false, true, false, true];
        let no_hostiles = true;
        let mapped = true; // an assault objective is always a mapped (scouted) room

        let rally_rooms: Vec<RoomName> = flapping_live_vision
            .iter()
            .map(|&live_visible| {
                // POST-FIX: the bot passes reliable intel, not raw live vision.
                let intel_reliable = rally_intel_reliable(mapped, live_visible);
                let uncontested = target_is_uncontested(intel_reliable, no_hostiles, true, true);
                shared_rally_point(approach, target, uncontested).room_name()
            })
            .collect();

        let first = rally_rooms[0];
        assert_eq!(first, target.room_name(), "mapped + clear → uncontested → stage at the target");
        assert!(
            rally_rooms.iter().all(|&r| r == first),
            "the rally room is CONSTANT across the flapping-vision sequence (oscillation fixed): {:?}",
            rally_rooms
        );
    }
}
