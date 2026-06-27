//! Pure claim-pacing kernel (P-OBJ harness / ADR 0028 K4): the `SquadManager` Phase C gate — how many NEW
//! objectives may be claimed this tick given the live squad counts and the caps. Mirrors the live
//! `while active < MAX_CONCURRENT_SQUADS && forming < MAX_FORMING_SQUADS` loop, so the harness reproduces
//! the forming-cap LOCKUP that backfired live (a stuck-forming squad blocks all new claims). No `game::*`.

/// Number of NEW objectives that may be claimed this tick.
///
/// - `active` = total live manager squads (forming + out-fighting).
/// - `forming` = squads with an incomplete roster (still spawning/rallying).
///
/// A complete squad frees a forming slot but still holds a concurrent slot; a forming squad holds both.
/// The budget is the tighter of the two headrooms — so a colony that can't complete rosters
/// (`forming == max_forming`) claims nothing new (the lockup), and one at the concurrent cap claims
/// nothing regardless of forming.
pub fn claims_allowed(active: usize, forming: usize, max_concurrent: usize, max_forming: usize) -> usize {
    let by_concurrent = max_concurrent.saturating_sub(active);
    let by_forming = max_forming.saturating_sub(forming);
    by_concurrent.min(by_forming)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_the_tighter_of_concurrent_and_forming_headroom() {
        // Nothing live → claim up to the forming cap (the binding one here).
        assert_eq!(claims_allowed(0, 0, 4, 2), 2);
        // One forming squad → one forming slot left, three concurrent → forming binds.
        assert_eq!(claims_allowed(1, 1, 4, 2), 1);
        // Two out-fighting (complete) squads, none forming → forming cap is the budget, not concurrent.
        assert_eq!(claims_allowed(2, 0, 4, 2), 2);
    }

    #[test]
    fn forming_cap_lockup_blocks_all_new_claims() {
        // A stuck-forming squad at the forming cap → zero new claims even with concurrent headroom (the
        // live `forming-cap=1` lockup that zeroed offense when a roster could not complete).
        assert_eq!(claims_allowed(1, 1, 4, 1), 0);
        assert_eq!(claims_allowed(2, 2, 4, 2), 0);
    }

    #[test]
    fn concurrent_cap_blocks_regardless_of_forming() {
        // At the concurrent cap → nothing new, even if none are forming.
        assert_eq!(claims_allowed(4, 0, 4, 2), 0);
    }
}
