//! Cohesion geometry — pure measures over a squad's member positions. This is the **validation
//! instrument** for the movement workstream (P2.M2/M3: "did cohesion improve?") and the basis for
//! H3's colony-health military term + seg-57 cohesion block. Only the *geometry* lives here (shared
//! by the sim and the live bot so they measure the same way); the seg-57 wiring + score term are
//! H3. No `game::*`, no serialization — pure value-type math over `screeps::Position`.

use screeps::{Position, RoomCoordinate};

/// A single tick's cohesion measurement of a squad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CohesionSample {
    /// Largest Chebyshev distance between any two members (the squad's "diameter"). 0 for ≤1 member.
    pub max_pairwise: u32,
    /// Mean Chebyshev distance of members to their centroid (tightness). 0.0 for ≤1 member.
    pub centroid_spread: f32,
    /// Fraction of members within `tol` of their assigned formation slot (`anchor + offset[i]`).
    /// 1.0 when no formation is supplied (nothing to be out of).
    pub in_formation_rate: f32,
}

/// The squad's centroid — the TRUE spatial mean of member positions in WORLD coordinates; `None` for
/// an empty slice. Shared by the live adapter and the sim so both derive the squad's coordinate frame
/// the SAME way (the cohesion + kite scoring all reference it).
///
/// ADR 0034 D1 (RC-1 fix): the mean is taken over each member's WORLD tile coords
/// (`Position::world_coords` = `room_x*50 + x`, `room_y*50 + y`) and converted back to a
/// room + in-room offset, so the centroid is the true spatial centre ACROSS room boundaries — not the
/// in-room (0–49) average stamped into `positions[0].room_name()` (which, for members in different
/// rooms, returned a point ~rooms off the real midpoint and fed the rally a meaningless approach).
///
/// BACKWARD-COMPATIBLE: for a SAME-ROOM squad this is byte-identical to the old in-room mean — the
/// world average of same-room positions lands back in that same room at the same `(x,y)` (the room
/// origin is common to all members, so `world_avg = origin + in_room_avg`, and the integer division
/// here matches the old `(sum/n).round()` to ≤1 ULP on the discrete grid; see the same-room test).
/// Integer world-coord math — no float branch, deterministic.
pub fn centroid(positions: &[Position]) -> Option<Position> {
    let n = positions.len();
    if n == 0 {
        return None;
    }
    // Sum in WORLD coords (each axis = room_x*50 + x), then take the rounded integer mean. Rounding is
    // done in integer space (`(sum + n/2) / n` for the magnitude, sign-preserving) so there is no float
    // that could flip a discrete branch — fully deterministic across platforms.
    let (sx, sy) = positions
        .iter()
        .fold((0i64, 0i64), |(ax, ay), p| {
            let (wx, wy) = p.world_coords();
            (ax + wx as i64, ay + wy as i64)
        });
    let n64 = n as i64;
    // Round-HALF-UP (toward +∞) integer division: `floor(s/n + 1/2) = (2s + n).div_euclid(2n)`. This
    // matches the OLD same-room behaviour byte-for-byte: the old code rounded the NON-NEGATIVE in-room
    // mean with `f32::round` (half-away-from-zero = half-up on a non-negative value); since for a
    // same-room squad `world = room_origin*n + sum_in_room` shares a common origin, rounding the world
    // mean half-UP reproduces the same in-room result (a plain half-away-from-zero would round negative
    // W-room world coords the WRONG way on a .5 tie). Pure integer math — no float, deterministic.
    let round_div = |s: i64, d: i64| -> i32 { (2 * s + d).div_euclid(2 * d) as i32 };
    let cx = round_div(sx, n64);
    let cy = round_div(sy, n64);
    Some(Position::from_world_coords(cx, cy))
}

/// `anchor + (dx,dy)`, or `None` if it leaves the room.
fn offset_pos(anchor: Position, (dx, dy): (i32, i32)) -> Option<Position> {
    let x = anchor.x().u8() as i32 + dx;
    let y = anchor.y().u8() as i32 + dy;
    if (0..50).contains(&x) && (0..50).contains(&y) {
        Some(Position::new(
            RoomCoordinate::new(x as u8).ok()?,
            RoomCoordinate::new(y as u8).ok()?,
            anchor.room_name(),
        ))
    } else {
        None
    }
}

/// Measure cohesion of `positions`. `formation` is the optional `(anchor, slot offsets)` the squad
/// is trying to hold — member `i` is "in formation" when it is within `tol` (Chebyshev) of
/// `anchor + offsets[i]`. Members and offsets are paired by index (the caller orders them); extra
/// members beyond the offset count are ignored for the in-formation rate.
pub fn measure(positions: &[Position], formation: Option<(Position, &[(i32, i32)])>, tol: u32) -> CohesionSample {
    let n = positions.len();
    if n == 0 {
        return CohesionSample { max_pairwise: 0, centroid_spread: 0.0, in_formation_rate: 1.0 };
    }

    let mut max_pairwise = 0u32;
    for i in 0..n {
        for j in (i + 1)..n {
            max_pairwise = max_pairwise.max(positions[i].get_range_to(positions[j]));
        }
    }

    // Centroid (rounded, clamped into the room), then mean distance to it.
    let centroid = centroid(positions).expect("non-empty checked above");
    let centroid_spread = positions.iter().map(|p| p.get_range_to(centroid) as f32).sum::<f32>() / n as f32;

    let in_formation_rate = match formation {
        Some((anchor, offsets)) if !offsets.is_empty() => {
            let m = n.min(offsets.len());
            let in_form = (0..m)
                .filter(|&i| offset_pos(anchor, offsets[i]).is_some_and(|slot| positions[i].get_range_to(slot) <= tol))
                .count();
            in_form as f32 / m as f32
        }
        _ => 1.0,
    };

    CohesionSample { max_pairwise, centroid_spread, in_formation_rate }
}

#[cfg(test)]
mod tests {
    use super::*;
    use screeps::RoomName;

    fn room() -> RoomName {
        "W1N1".parse().unwrap()
    }
    fn pos(x: u8, y: u8) -> Position {
        Position::new(RoomCoordinate::new(x).unwrap(), RoomCoordinate::new(y).unwrap(), room())
    }

    const QUAD: [(i32, i32); 4] = [(0, 0), (1, 0), (0, 1), (1, 1)];

    #[test]
    fn a_perfect_box_is_fully_in_formation_and_tight() {
        let anchor = pos(25, 25);
        let members = vec![pos(25, 25), pos(26, 25), pos(25, 26), pos(26, 26)];
        let s = measure(&members, Some((anchor, &QUAD)), 0);
        assert_eq!(s.in_formation_rate, 1.0);
        assert_eq!(s.max_pairwise, 1, "2×2 diameter is 1");
        assert!(s.centroid_spread < 1.0, "very tight, got {}", s.centroid_spread);
    }

    #[test]
    fn a_scattered_squad_scores_low_and_wide() {
        let anchor = pos(25, 25);
        // Members flung to the corners — none near their slot.
        let members = vec![pos(2, 2), pos(47, 2), pos(2, 47), pos(47, 47)];
        let s = measure(&members, Some((anchor, &QUAD)), 1);
        assert_eq!(s.in_formation_rate, 0.0, "nobody near their slot");
        assert!(s.max_pairwise >= 45, "spread across the room, got {}", s.max_pairwise);
        assert!(s.centroid_spread > 20.0, "loose, got {}", s.centroid_spread);
    }

    #[test]
    fn max_pairwise_grows_as_members_separate() {
        let tight = measure(&[pos(25, 25), pos(26, 25)], None, 0);
        let loose = measure(&[pos(25, 25), pos(35, 25)], None, 0);
        assert_eq!(tight.max_pairwise, 1);
        assert_eq!(loose.max_pairwise, 10);
        assert!(loose.max_pairwise > tight.max_pairwise);
    }

    #[test]
    fn partial_formation_is_a_fraction() {
        let anchor = pos(25, 25);
        // 2 of 4 on their slots, 2 displaced beyond tol.
        let members = vec![pos(25, 25), pos(26, 25), pos(40, 40), pos(41, 41)];
        let s = measure(&members, Some((anchor, &QUAD)), 0);
        assert_eq!(s.in_formation_rate, 0.5);
    }

    /// ADR 0034 D1 backward-compatibility: for a SAME-ROOM squad the world-coord centroid is
    /// byte-identical to the old in-room mean — same room, same `(x,y)`. The world average of
    /// same-room positions lands back in that room (the room origin is common to all members), so the
    /// fix changes NOTHING for the in-room cohesion/kite scoring that the existing tests rely on.
    #[test]
    fn same_room_centroid_is_unchanged_by_the_world_coord_fix() {
        let r = room();
        let members = vec![pos(25, 25), pos(26, 25), pos(25, 26), pos(26, 26)];
        let c = centroid(&members).unwrap();
        assert_eq!(c.room_name(), r, "same-room squad → same room");
        // Old in-room mean: ((25+26+25+26)/4, (25+25+26+26)/4) = (25.5,25.5).round() = (26,26).
        assert_eq!((c.x().u8(), c.y().u8()), (26, 26), "same-room centroid byte-identical to the in-room mean");
        // An exact-mean cluster: (10,40)/(20,40)/(30,40) → (20,40).
        let exact = vec![pos(10, 40), pos(20, 40), pos(30, 40)];
        let ce = centroid(&exact).unwrap();
        assert_eq!((ce.room_name(), ce.x().u8(), ce.y().u8()), (r, 20, 40));
    }

    /// ADR 0034 D1 (RC-1): a CROSS-ROOM squad now gets a TRUE spatial centroid — a room BETWEEN the
    /// members, not `positions[0]`'s room. Members at W3N2(25,25) + W4N7(25,25) → the world midpoint
    /// room W3N4 (NOT the old W3N2). This is the headline centroid fix.
    #[test]
    fn cross_room_centroid_is_the_true_spatial_midpoint() {
        let a = Position::new(RoomCoordinate::new(25).unwrap(), RoomCoordinate::new(25).unwrap(), "W3N2".parse::<RoomName>().unwrap());
        let b = Position::new(RoomCoordinate::new(25).unwrap(), RoomCoordinate::new(25).unwrap(), "W4N7".parse::<RoomName>().unwrap());
        let c = centroid(&[a, b]).unwrap();
        assert_eq!(c.room_name(), "W3N4", "true spatial midpoint room, NOT W3N2 (old broken centroid)");
        assert_ne!(c.room_name(), a.room_name(), "no longer stamped into positions[0]'s room");
    }

    #[test]
    fn degenerate_cases() {
        let empty = measure(&[], None, 0);
        assert_eq!(empty, CohesionSample { max_pairwise: 0, centroid_spread: 0.0, in_formation_rate: 1.0 });
        let solo = measure(&[pos(25, 25)], Some((pos(25, 25), &QUAD)), 0);
        assert_eq!(solo.max_pairwise, 0);
        assert_eq!(solo.centroid_spread, 0.0);
        assert_eq!(solo.in_formation_rate, 1.0, "the one member is on slot 0");
    }
}
