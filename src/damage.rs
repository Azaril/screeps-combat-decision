//! The canonical tower attack / heal / repair falloff curves — re-exported from the engine (the
//! mechanics ground truth, `screeps_combat_engine::damage`). Combat policy (this crate) and the bot
//! reach the curve THROUGH here, so there is one source of truth and no duplicated f32 copy. The
//! engine returns `u32`; callers needing `f32` cast at the use site.

pub use screeps_combat_engine::damage::{tower_attack_damage_at_range, tower_heal_at_range, tower_repair_at_range};
