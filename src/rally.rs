//! Pure rally / boundary-cohesion gates (P-OBJ #23 / ADR 0028) — the "wait + group up, then depart and
//! cross as a bloc" decisions, lifted out of the bot (`military::formation`) so the live path AND the
//! offline lifecycle harness share ONE implementation (parity, like `decide_squad` / `lifecycle`). JS-free
//! value-type math over `screeps::Position` — no `game::*`, no ECS.

use screeps::Position;

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
}
