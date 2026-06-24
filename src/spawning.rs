//! Pure spawn-body construction: a static [`SpawnBodyDefinition`] template (pre/repeat/post part
//! slices + an energy budget) expanded to a concrete `Vec<Part>` by [`create_body`], honoring the
//! energy budget, the `minimum_repeat`/`maximum_repeat` clamps, and the 50-part engine cap.
//!
//! Lives here (not in the bot) so the sim/eval can build the bot's real bodies without depending on
//! the whole bot — pure logic over `screeps-game-api` value types (`Part`/`Part::cost`), no `game::*`.
//! The bot re-exports `SpawnBodyDefinition` + `create_body` from `crate::creep` so existing spawn
//! call sites are unchanged; the specs-coupled `spawning::build` stays bot-side.

use screeps::{Part, MAX_CREEP_SIZE};

/// A repeat-template body definition: a fixed `pre_body` + `post_body` framing a `repeat_body` unit
/// expanded as many times as `maximum_energy` and the 50-part cap allow (bounded by
/// `minimum_repeat`/`maximum_repeat`). Borrows `&'static`-friendly part slices, so the predefined
/// body factories return `SpawnBodyDefinition<'static>`.
pub struct SpawnBodyDefinition<'a> {
    pub maximum_energy: u32,
    pub minimum_repeat: Option<usize>,
    pub maximum_repeat: Option<usize>,
    pub pre_body: &'a [Part],
    pub repeat_body: &'a [Part],
    pub post_body: &'a [Part],
}

fn clamp<T: PartialOrd>(val: T, min: T, max: T) -> T {
    if val < min {
        min
    } else if val > max {
        max
    } else {
        val
    }
}

/// Expand a [`SpawnBodyDefinition`] into a concrete body, or `Err(())` if even the fixed/minimum body
/// can't be afforded or fits within the 50-part cap. Repeats are clamped to the energy budget, the
/// part cap, and the definition's `minimum_repeat`/`maximum_repeat`. (`Err(())` is a deliberate
/// minimal "can't build at this budget" signal — callers branch on `.is_err()`/`.ok()`.)
#[allow(clippy::result_unit_err)]
pub fn create_body(definition: &SpawnBodyDefinition) -> Result<Vec<Part>, ()> {
    let pre_body_cost: u32 = definition.pre_body.iter().map(|p| p.cost()).sum();
    let post_body_cost: u32 = definition.post_body.iter().map(|p| p.cost()).sum();

    let fixed_body_cost = pre_body_cost + post_body_cost;

    if fixed_body_cost > definition.maximum_energy {
        return Err(());
    }

    let repeat_body_cost: u32 = definition.repeat_body.iter().map(|p| p.cost()).sum();

    let remaining_available_energy: u32 = definition.maximum_energy - fixed_body_cost;

    let max_possible_repeat_parts_by_cost = ((remaining_available_energy as f32) / (repeat_body_cost as f32)).floor() as usize;

    let fixed_body_length = definition.pre_body.len() + definition.post_body.len();
    if fixed_body_length > MAX_CREEP_SIZE as usize {
        return Err(());
    }

    let max_possible_repeat_parts_by_length = if !definition.repeat_body.is_empty() {
        (MAX_CREEP_SIZE as usize - fixed_body_length) / definition.repeat_body.len()
    } else {
        0usize
    };

    let max_possible_repeat_parts = max_possible_repeat_parts_by_cost.min(max_possible_repeat_parts_by_length);

    if let Some(min_parts) = definition.minimum_repeat {
        if max_possible_repeat_parts < min_parts {
            return Err(());
        }
    }

    let repeat_parts = clamp(
        max_possible_repeat_parts,
        definition.minimum_repeat.unwrap_or(0),
        definition.maximum_repeat.unwrap_or(usize::MAX),
    );

    let full_repeat_body = definition
        .repeat_body
        .iter()
        .cycle()
        .take(repeat_parts * definition.repeat_body.len());

    let body = definition
        .pre_body
        .iter()
        .chain(full_repeat_body)
        .chain(definition.post_body.iter())
        .cloned()
        .collect::<Vec<Part>>();

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins for `create_body`'s clamping behavior. The review's body-sizing
    // seed (IBEX-022) was REFUTED -- the min-cost clamp is present and
    // correct. These tests lock it so later refactors cannot silently break it.

    #[test]
    fn create_body_rejects_unaffordable_fixed_body() {
        // Work (100) + Move (50) = 150 > 100 maximum energy.
        let definition = SpawnBodyDefinition {
            maximum_energy: 100,
            minimum_repeat: None,
            maximum_repeat: None,
            pre_body: &[Part::Work],
            repeat_body: &[],
            post_body: &[Part::Move],
        };

        assert!(create_body(&definition).is_err());
    }

    #[test]
    fn create_body_rejects_unaffordable_minimum_repeat() {
        // 3 x (Work + Move) = 450 > 300 maximum energy.
        let definition = SpawnBodyDefinition {
            maximum_energy: 300,
            minimum_repeat: Some(3),
            maximum_repeat: None,
            pre_body: &[],
            repeat_body: &[Part::Work, Part::Move],
            post_body: &[],
        };

        assert!(create_body(&definition).is_err());
    }

    #[test]
    fn create_body_clamps_repeat_to_maximum_repeat() {
        // Plenty of energy, but maximum_repeat caps the body at 2 repeats.
        let definition = SpawnBodyDefinition {
            maximum_energy: 10_000,
            minimum_repeat: Some(1),
            maximum_repeat: Some(2),
            pre_body: &[],
            repeat_body: &[Part::Work, Part::Move],
            post_body: &[],
        };

        let body = create_body(&definition).expect("expected body");

        assert_eq!(body, vec![Part::Work, Part::Move, Part::Work, Part::Move]);
    }

    #[test]
    fn create_body_clamps_repeat_to_energy_budget() {
        // 500 energy / 150 per (Work + Move) repeat = 3 full repeats.
        let definition = SpawnBodyDefinition {
            maximum_energy: 500,
            minimum_repeat: Some(1),
            maximum_repeat: None,
            pre_body: &[],
            repeat_body: &[Part::Work, Part::Move],
            post_body: &[],
        };

        let body = create_body(&definition).expect("expected body");

        assert_eq!(body.len(), 6);
        let cost: u32 = body.iter().map(|p| p.cost()).sum();
        assert!(cost <= 500, "body cost {} exceeded energy budget", cost);
    }

    #[test]
    fn create_body_caps_total_parts_at_max_creep_size() {
        // Effectively unlimited energy: the length clamp must hold.
        let definition = SpawnBodyDefinition {
            maximum_energy: 1_000_000,
            minimum_repeat: Some(1),
            maximum_repeat: None,
            pre_body: &[Part::Carry],
            repeat_body: &[Part::Move],
            post_body: &[Part::Carry],
        };

        let body = create_body(&definition).expect("expected body");

        assert!(body.len() <= MAX_CREEP_SIZE as usize);
    }
}
