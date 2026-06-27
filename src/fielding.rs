//! Pure fielding kernel (P-OBJ harness / ADR 0028 K3): turn an objective's composition + the squad's
//! already-filled slots into the spawn-queue requests the [`crate::spawn_throughput`] model consumes.
//! Mirrors the bot's Phase B (`queue_slot_spawn`): build each UNFILLED slot's body at
//! `min(best_capacity, per_member_cap)`, skip a slot no in-range home can build (the `None` stall — the
//! live `queue_slot_spawn` early-returns on `None`, so that slot never queues and the roster stalls
//! there), and tag each request with the slot index + the combat spawn priority. No `game::*`.

use crate::bodies::MoveProfile;
use crate::composition::SquadComposition;
use crate::spawn_throughput::QueuedSpawn;

/// [`QueuedSpawn`] requests for every UNFILLED slot of `composition`. `filled[i] == true` ⇒ slot `i` has a
/// living member (skip it; a missing/short `filled` treats the slot as unfilled). Bodies are built at
/// `build_energy = min(best_capacity, per_member_cap)` so a sized spec and a template fallback alike stay
/// bankable (ADR 0028 — the per-member energy cap). A slot whose body can't be built at that energy is
/// skipped (mirrors the live stall). The request `id` is the slot index, so the driver can mark it filled.
pub fn slots_to_spawn(
    composition: &SquadComposition,
    filled: &[bool],
    best_capacity: u32,
    per_member_cap: u32,
    priority: f32,
    move_profile: MoveProfile,
) -> Vec<QueuedSpawn> {
    let build_energy = best_capacity.min(per_member_cap);
    composition
        .slots
        .iter()
        .enumerate()
        .filter(|(i, _)| !filled.get(*i).copied().unwrap_or(false))
        .filter_map(|(i, slot)| {
            let body = slot.body_type.build_body(build_energy, move_profile)?;
            let body_cost: u32 = body.iter().map(|p| p.cost()).sum();
            Some(QueuedSpawn { priority, body_cost, part_count: body.len() as u32, id: i as u64 })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition::{force_ceiling, SquadRole};

    /// A multi-slot composition (3 fighters + 5 healers) the fielding kernel can fan out, with no catalog
    /// constructor (ADR 0031 P4b) — the force ceiling is the template-free stand-in.
    fn multi_slot_comp() -> SquadComposition {
        force_ceiling(12_900, SquadRole::RangedDPS)
    }

    #[test]
    fn queues_every_unfilled_slot_capped_to_the_per_member_energy() {
        let comp = multi_slot_comp();
        let n = comp.slots.len();
        let none_filled = vec![false; n];
        let reqs = slots_to_spawn(&comp, &none_filled, 12_900, 3000, 75.0, MoveProfile::Plains);
        assert_eq!(reqs.len(), n, "all slots queue when none are filled and all are buildable");
        for r in &reqs {
            assert!(r.body_cost <= 3000, "body capped to the per-member ceiling, not the RCL8 home (got {})", r.body_cost);
            assert!((r.priority - 75.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn skips_filled_slots_and_maps_ids_to_slot_indices() {
        let comp = multi_slot_comp();
        let n = comp.slots.len();
        let mut filled = vec![false; n];
        filled[0] = true; // slot 0 already has a member
        let reqs = slots_to_spawn(&comp, &filled, 5300, 3000, 75.0, MoveProfile::Plains);
        assert_eq!(reqs.len(), n - 1, "the filled slot does not re-queue");
        assert!(reqs.iter().all(|r| r.id != 0), "slot 0 (filled) is absent");
        assert!(reqs.iter().any(|r| r.id == 1), "unfilled slots keep their index as id");
    }

    #[test]
    fn unbuildable_slots_are_skipped_the_stall() {
        // A home far too small to build any member → no requests (the live roster-stall: nothing queues).
        let comp = multi_slot_comp();
        let none_filled = vec![false; comp.slots.len()];
        let reqs = slots_to_spawn(&comp, &none_filled, 100, 3000, 75.0, MoveProfile::Plains);
        assert!(reqs.is_empty(), "no in-range home can build a member at 100e → the roster can't field");
    }
}
