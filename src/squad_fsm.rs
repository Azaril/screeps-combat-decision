//! Pure squad-combat FSM transition kernel (P-OBJ #23 / ADR 0028 K2) — the state-machine TRANSITIONS of
//! the live `SquadCombatJob` (`jobs/squad_combat.rs`), lifted out of the ECS tick so the offline lifecycle
//! harness drives the SAME transitions the bot does, and the table is unit-tested. The ECS ACTIONS
//! (movement, combat, the orphan recall-to-recycle) stay in the bot; only the pure transition DECISION
//! lives here. No `game::*`, no `specs`.
//!
//! Each arm mirrors the `return Some(state)` decisions in the matching `*::tick`, in the SAME priority
//! order. The orphan-recall (squad retired → recall) is an ECS side-effect the bot performs BEFORE
//! consulting this, so it is intentionally not a transition here.

/// The squad-combat per-member state (mirrors `SquadCombatState` in `jobs/squad_combat.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SquadFsmState {
    /// Traveling to the target room.
    MoveToRoom,
    /// Temporarily engaged in combat while en route (ambush response).
    CombatResponse,
    /// In the target room, actively fighting the objective.
    Engaged,
    /// Withdrawing due to low HP or a squad retreat signal.
    Retreating,
}

/// The pure inputs a transition depends on — the bot builds this from the live `Creep`/squad, the harness
/// from its sim state.
#[derive(Clone, Copy, Debug)]
pub struct SquadFsmSnapshot {
    /// This member is in the objective's target room.
    pub in_target_room: bool,
    /// A hostile is within the state-appropriate threat range (the caller uses the live ranges: ≤5 for the
    /// MoveToRoom ambush check, ≤6 for the CombatResponse threats-cleared check).
    pub hostiles_nearby: bool,
    /// This member's HP as a fraction of max (`hits / hits_max`, 0.0..=1.0).
    pub hp_fraction: f32,
    /// The CombatResponse window (`COMBAT_RESPONSE_TIMEOUT`) has elapsed.
    pub combat_response_timed_out: bool,
    /// The squad signals retreat (`squad_state == Retreating`).
    pub squad_retreating: bool,
    /// The squad has progressed past rallying and is ready to advance/engage (`squad_state >= Moving`;
    /// the caller passes `true` when the squad ref is absent, preserving the live default).
    pub squad_ready_to_engage: bool,
    /// The squad actively wants to engage (`squad_state` in {Moving, Engaged}; `false` when absent) — the
    /// Retreating→Engaged re-engage signal.
    pub squad_wants_engage: bool,
}

// HP thresholds as fractions of max — mirror the live integer ratios in `squad_combat.rs`.
const COMBAT_RESPONSE_RETREAT_HP: f32 = 2.0 / 5.0; // < 40% while responding to an ambush
const ENGAGED_RETREAT_HP: f32 = 1.0 / 2.0; // < 50% while engaged at the objective
const REENGAGE_HP: f32 = 4.0 / 5.0; // > 80% recovers
const REENGAGE_HP_IF_SQUAD_WANTS: f32 = 3.0 / 5.0; // > 60% if the squad signals engage

/// The next FSM state, or `None` to stay. Mirrors the `return Some(state)` decisions in each
/// `SquadCombatState::tick`, in the SAME priority order.
pub fn next_state(state: SquadFsmState, s: &SquadFsmSnapshot) -> Option<SquadFsmState> {
    use SquadFsmState::*;
    match state {
        MoveToRoom => {
            // Ambush en route → respond; then the squad retreat signal; then arrival-engage.
            if !s.in_target_room && s.hostiles_nearby {
                return Some(CombatResponse);
            }
            if s.squad_retreating {
                return Some(Retreating);
            }
            // Engage ONLY once arrived AND the squad has progressed past rallying (no lone early engage
            // while the rest are still gathering).
            if s.in_target_room && s.squad_ready_to_engage {
                return Some(Engaged);
            }
            None
        }
        CombatResponse => {
            if s.squad_retreating || s.hp_fraction < COMBAT_RESPONSE_RETREAT_HP {
                return Some(Retreating);
            }
            if s.in_target_room {
                return Some(Engaged);
            }
            // Threats cleared OR the response window elapsed → resume travel.
            if !s.hostiles_nearby || s.combat_response_timed_out {
                return Some(MoveToRoom);
            }
            None
        }
        Engaged => {
            if s.squad_retreating || s.hp_fraction < ENGAGED_RETREAT_HP {
                return Some(Retreating);
            }
            if !s.in_target_room {
                return Some(MoveToRoom);
            }
            None
        }
        Retreating => {
            // Re-engage once HP recovers (or at a lower bar if the squad signals engage) — but NEVER while
            // the squad itself signals retreat, else a healthy creep ping-pongs Engaged<->Retreating.
            if !s.squad_retreating
                && (s.hp_fraction > REENGAGE_HP || (s.squad_wants_engage && s.hp_fraction > REENGAGE_HP_IF_SQUAD_WANTS))
            {
                return Some(Engaged);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SquadFsmState::*;
    use super::*;

    /// A baseline snapshot: in the target room, no threats, full HP, squad ready/engaging.
    fn base() -> SquadFsmSnapshot {
        SquadFsmSnapshot {
            in_target_room: true,
            hostiles_nearby: false,
            hp_fraction: 1.0,
            combat_response_timed_out: false,
            squad_retreating: false,
            squad_ready_to_engage: true,
            squad_wants_engage: true,
        }
    }

    #[test]
    fn moveto_responds_to_ambush_then_retreat_then_engages_on_arrival() {
        // Ambush en route (not in target, threats) → CombatResponse — and it OUTRANKS the retreat signal.
        let ambush = SquadFsmSnapshot { in_target_room: false, hostiles_nearby: true, squad_retreating: true, ..base() };
        assert_eq!(next_state(MoveToRoom, &ambush), Some(CombatResponse));
        // Retreat signal while traveling clear → Retreating.
        let retreat = SquadFsmSnapshot { in_target_room: false, hostiles_nearby: false, squad_retreating: true, ..base() };
        assert_eq!(next_state(MoveToRoom, &retreat), Some(Retreating));
        // Arrived + squad ready → Engaged; arrived but NOT ready (still rallying) → stay (no lone engage).
        let arrived_ready = SquadFsmSnapshot { in_target_room: true, squad_ready_to_engage: true, ..base() };
        assert_eq!(next_state(MoveToRoom, &arrived_ready), Some(Engaged));
        let arrived_rallying = SquadFsmSnapshot { in_target_room: true, squad_ready_to_engage: false, ..base() };
        assert_eq!(next_state(MoveToRoom, &arrived_rallying), None, "no lone engage while the squad still rallies");
        // En route, nothing happening → stay.
        let traveling = SquadFsmSnapshot { in_target_room: false, hostiles_nearby: false, squad_retreating: false, ..base() };
        assert_eq!(next_state(MoveToRoom, &traveling), None);
    }

    #[test]
    fn combatresponse_retreats_engages_or_resumes() {
        // Low HP (<40%) or squad retreat → Retreating.
        assert_eq!(next_state(CombatResponse, &SquadFsmSnapshot { hp_fraction: 0.39, in_target_room: false, ..base() }), Some(Retreating));
        assert_eq!(next_state(CombatResponse, &SquadFsmSnapshot { squad_retreating: true, in_target_room: false, ..base() }), Some(Retreating));
        // Reached the target room → full engagement.
        assert_eq!(next_state(CombatResponse, &SquadFsmSnapshot { in_target_room: true, ..base() }), Some(Engaged));
        // Threats cleared OR timed out (and not in target) → resume travel.
        assert_eq!(next_state(CombatResponse, &SquadFsmSnapshot { in_target_room: false, hostiles_nearby: false, ..base() }), Some(MoveToRoom));
        assert_eq!(next_state(CombatResponse, &SquadFsmSnapshot { in_target_room: false, hostiles_nearby: true, combat_response_timed_out: true, ..base() }), Some(MoveToRoom));
        // Still fighting an ambush, healthy, threats present, not timed out → stay.
        assert_eq!(next_state(CombatResponse, &SquadFsmSnapshot { in_target_room: false, hostiles_nearby: true, ..base() }), None);
    }

    #[test]
    fn engaged_retreats_low_hp_or_signal_else_moves_back_if_left_room() {
        // Engaged retreats below 50% (a higher bar than CombatResponse's 40%).
        assert_eq!(next_state(Engaged, &SquadFsmSnapshot { hp_fraction: 0.49, ..base() }), Some(Retreating));
        assert_eq!(next_state(Engaged, &SquadFsmSnapshot { hp_fraction: 0.45, ..base() }), Some(Retreating), "40% would survive in CombatResponse but not Engaged");
        assert_eq!(next_state(Engaged, &SquadFsmSnapshot { squad_retreating: true, ..base() }), Some(Retreating));
        // Left the target room → go back.
        assert_eq!(next_state(Engaged, &SquadFsmSnapshot { in_target_room: false, ..base() }), Some(MoveToRoom));
        // Healthy, in room, no retreat → keep fighting.
        assert_eq!(next_state(Engaged, &base()), None);
    }

    #[test]
    fn retreating_reengages_on_recovery_but_never_while_squad_retreats() {
        // Recovered above 80% → re-engage.
        assert_eq!(next_state(Retreating, &SquadFsmSnapshot { hp_fraction: 0.81, ..base() }), Some(Engaged));
        // Squad wants engage + above 60% → re-engage at the lower bar.
        assert_eq!(next_state(Retreating, &SquadFsmSnapshot { hp_fraction: 0.61, squad_wants_engage: true, ..base() }), Some(Engaged));
        // Above 60% but squad does NOT want engage → stay retreating (needs >80%).
        assert_eq!(next_state(Retreating, &SquadFsmSnapshot { hp_fraction: 0.61, squad_wants_engage: false, ..base() }), None);
        // The anti-ping-pong guard: even at full HP, NEVER re-engage while the squad signals retreat.
        assert_eq!(next_state(Retreating, &SquadFsmSnapshot { hp_fraction: 1.0, squad_retreating: true, ..base() }), None);
    }
}
