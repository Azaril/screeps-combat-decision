//! Pure objective/squad lifecycle DECISIONS (P-OBJ #23, ADR 0027), lifted out of the live ECS
//! `SquadManager` so they are deterministically unit-testable offline — the codebase's pure-decision
//! + thin-adapter pattern (like [`crate::decide_squad`]). The live manager builds the snapshot from
//! the ECS each tick, calls these, and applies the action; the offline lifecycle harness composes the
//! SAME functions with the combat engine. One implementation, no live/sim drift.

/// Per-tick snapshot of one manager-owned squad and its objective. All `Copy`, no ECS/`game::` types,
/// so the decision is a pure function of plain data.
#[derive(Clone, Copy, Debug)]
pub struct ReconcileSnapshot {
    /// The objective this squad was fielded for no longer exists in the queue.
    pub objective_gone: bool,
    /// Another live squad already covers this objective this tick (a duplicate to consolidate).
    pub duplicate: bool,
    /// A `Defend` objective — an owned room we never abandon / back off (re-staff a wiped defender).
    pub is_defend: bool,
    /// The commitment lease (`deadline`) has elapsed.
    pub deadline_lapsed: bool,
    /// The squad had members and all are now dead (wave-wiped / overwhelmed).
    pub wiped: bool,
    /// The squad is actively closing on / fighting a target this tick (`decide_squad` returned a focus).
    pub has_focus: bool,
    /// The squad has reached combat at least once (latched on first `Engaged`).
    pub engaged_once: bool,
    /// At least one living member stands in the objective's room.
    pub in_target_room: bool,
    /// Any living member at all.
    pub has_members: bool,
    /// The squad is still ASSEMBLING its roster (has members but has not yet engaged and is not at the
    /// full requested count) — a legitimate forming/rallying state, not a stuck-en-route one.
    pub forming: bool,
    /// The squad made spawn progress THIS reconcile (its present-member count increased since the manager
    /// last looked). True only on the exact tick a new member appears, so it is self-bounding: a roster
    /// can increase at most `requested_slots` times, after which progress stays false and the lease lapses.
    pub forming_progress: bool,
}

/// Why a squad is being retired (drives logging + backoff).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireReason {
    ObjectiveGone,
    Duplicate,
    Wiped,
    /// Fought and cleared the target — a clean win.
    Resolved,
    /// Stuck en route / fought-and-withdrew without finishing before the lease lapsed.
    GaveUp,
}

/// The Phase-A reconcile outcome for one squad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Keep the squad and refresh its commitment lease (it is actively engaging — progress).
    KeepRefreshLease,
    /// Keep the squad as-is (forming / traveling — committed but not yet engaging).
    Keep,
    /// Retire the squad. `withdraw` clears the objective from the queue (a clean win, so no one re-fields
    /// it); `mark_unwinnable` backs the room off with the exponential give-up backoff (a loss/abandon —
    /// never for a `Defend` objective).
    Retire {
        reason: RetireReason,
        withdraw: bool,
        mark_unwinnable: bool,
    },
}

/// The SquadManager Phase-A reconcile decision (ADR 0027). Pure: snapshot in, action out.
///
/// RESOLVE: the squad fought (`engaged_once`) and now stands in the objective room with no target left
/// → the objective is cleared → withdraw + retire (clean win, no backoff). `engaged_once` is what
/// distinguishes a clear from the just-arrived tick (in-room, focus not yet computed) — a squad that
/// never engaged cannot have cleared anything.
///
/// GIVE-UP: the lease lapsed with no active focus and no clean clear → stuck en route, or fought and
/// withdrew → back the (non-Defend) room off so we don't immediately re-field into the same dead end.
///
/// Otherwise keep — refreshing the lease while a focus is held so a long fight or a brief vision gap
/// never lets the objective lapse underneath the squad.
///
/// FORMING-PROGRESS lease refresh (ADR 0028 follow-up, the rally-stall fix): a squad that legitimately
/// sits at home assembling its roster has no focus, so the base lease lapses at +400 → it would be
/// retired mid-assembly (and re-fielded → Generation churn that orphans the already-spawned members).
/// While the squad is `forming` AND `forming_progress` (its present-member count just increased), refresh
/// the lease so the assembly is not torn down. This is BOUNDED by construction: `forming_progress` is true
/// only on the tick a new member appears, and a roster grows at most `requested_slots` times — once the
/// count stops increasing (a genuinely-unfieldable squad that can never bank enough energy for the next
/// member) `forming_progress` stays false, the lease lapses, and the squad gives up + frees the slot.
pub fn reconcile(s: ReconcileSnapshot) -> ReconcileAction {
    let resolved = s.engaged_once && s.in_target_room && !s.has_focus && s.has_members;
    // A forming squad that is still gaining members keeps its lease alive past the deadline (bounded — see
    // the doc comment). It must NOT count as a give-up even though it has no focus and the lease lapsed.
    let forming_progressing = s.forming && s.forming_progress && s.has_members;
    let gave_up = s.deadline_lapsed && !s.has_focus && !resolved && !forming_progressing;

    if s.objective_gone || s.duplicate || s.wiped || resolved || gave_up {
        // Precedence: a clean clear (Resolved) is the most informative + drives withdraw; then the
        // objective simply being gone (no backoff — not our loss); then a wipe; then the lease give-up;
        // then a duplicate. (Ordering matters: `gave_up` must NOT mislabel an `objective_gone` retire.)
        let reason = if resolved {
            RetireReason::Resolved
        } else if s.objective_gone {
            RetireReason::ObjectiveGone
        } else if s.wiped {
            RetireReason::Wiped
        } else if gave_up {
            RetireReason::GaveUp
        } else {
            RetireReason::Duplicate
        };
        let withdraw = resolved;
        let mark_unwinnable = (s.wiped || gave_up) && !s.objective_gone && !s.is_defend;
        return ReconcileAction::Retire {
            reason,
            withdraw,
            mark_unwinnable,
        };
    }

    // Refresh the lease while actively engaging (a long fight / vision gap) OR while a forming squad is
    // still making spawn progress (the rally-stall fix — the assembly is not torn down mid-form).
    if s.has_focus || forming_progressing {
        ReconcileAction::KeepRefreshLease
    } else {
        ReconcileAction::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A baseline snapshot: a healthy committed squad mid-travel (forming, not engaged, lease alive).
    fn forming() -> ReconcileSnapshot {
        ReconcileSnapshot {
            objective_gone: false,
            duplicate: false,
            is_defend: false,
            deadline_lapsed: false,
            wiped: false,
            has_focus: false,
            engaged_once: false,
            in_target_room: false,
            has_members: true,
            forming: false,
            forming_progress: false,
        }
    }

    /// THE regression that would explain "squad never masses/reaches": a forming/traveling squad whose
    /// producer fell silent (stale intel) must be KEPT while its lease is alive — never retired early.
    #[test]
    fn forming_committed_squad_is_kept() {
        assert_eq!(reconcile(forming()), ReconcileAction::Keep);
    }

    /// Arrived but Phase B2 has not set the focus yet (the race the `engaged_once` latch guards): must
    /// NOT resolve (it never fought) and must NOT give up (lease alive) — just keep.
    #[test]
    fn just_arrived_not_yet_engaged_is_kept_not_resolved() {
        let s = ReconcileSnapshot { in_target_room: true, ..forming() };
        assert_eq!(reconcile(s), ReconcileAction::Keep);
    }

    /// Actively engaging → refresh the lease so a long fight never lapses.
    #[test]
    fn engaging_refreshes_the_lease() {
        let s = ReconcileSnapshot { in_target_room: true, has_focus: true, engaged_once: true, ..forming() };
        assert_eq!(reconcile(s), ReconcileAction::KeepRefreshLease);
    }

    /// Fought + cleared (in room, was engaged, no target left) → resolve: withdraw, no backoff.
    #[test]
    fn cleared_target_resolves_with_withdraw_no_backoff() {
        let s = ReconcileSnapshot { engaged_once: true, in_target_room: true, has_focus: false, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::Resolved, withdraw: true, mark_unwinnable: false }
        );
    }

    /// Stuck en route past the lease (never engaged, not in room, no focus) → give up + back the room off.
    #[test]
    fn stuck_en_route_gives_up_and_backs_off() {
        let s = ReconcileSnapshot { deadline_lapsed: true, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::GaveUp, withdraw: false, mark_unwinnable: true }
        );
    }

    /// A still-engaging squad past its (about-to-be-refreshed) lease must NOT give up — focus wins.
    #[test]
    fn engaging_past_lease_does_not_give_up() {
        let s = ReconcileSnapshot { deadline_lapsed: true, has_focus: true, in_target_room: true, engaged_once: true, ..forming() };
        assert_eq!(reconcile(s), ReconcileAction::KeepRefreshLease);
    }

    /// Wiped (had members, all dead) → retire + back off (non-Defend).
    #[test]
    fn wiped_retires_and_backs_off() {
        let s = ReconcileSnapshot { wiped: true, has_members: false, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::Wiped, withdraw: false, mark_unwinnable: true }
        );
    }

    /// Defense is exempt from backoff — a wiped defender retires but the owned room is never abandoned.
    #[test]
    fn wiped_defender_does_not_back_off() {
        let s = ReconcileSnapshot { wiped: true, has_members: false, is_defend: true, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::Wiped, withdraw: false, mark_unwinnable: false }
        );
    }

    /// Objective withdrawn out from under the squad → retire, no backoff (it wasn't a loss here).
    #[test]
    fn objective_gone_retires_without_backoff() {
        let s = ReconcileSnapshot { objective_gone: true, deadline_lapsed: true, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::ObjectiveGone, withdraw: false, mark_unwinnable: false }
        );
    }

    /// FIX 2 (rally-stall): a forming squad PAST its lease that is still making spawn progress (a member
    /// just appeared) must be KEPT (lease refreshed), not retired mid-assembly → no re-field/Generation
    /// churn. Mirrors the live "RALLY present=4/5, last member just spawned at tick 401" case.
    #[test]
    fn forming_progressing_past_lease_is_kept_not_retired() {
        let s = ReconcileSnapshot { deadline_lapsed: true, forming: true, forming_progress: true, ..forming() };
        assert_eq!(reconcile(s), ReconcileAction::KeepRefreshLease);
    }

    /// FIX 2 bound: a forming squad PAST its lease that has STOPPED making progress (can never bank energy
    /// for the next member — present count flat) must STILL give up + free the slot. The forming exemption
    /// is bounded by progress, so a genuinely-unfieldable squad is never immortal.
    #[test]
    fn forming_non_progressing_past_lease_still_gives_up() {
        let s = ReconcileSnapshot { deadline_lapsed: true, forming: true, forming_progress: false, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::GaveUp, withdraw: false, mark_unwinnable: true }
        );
    }

    /// A duplicate (another squad already covers it) retires quietly — no withdraw, no backoff.
    #[test]
    fn duplicate_retires_quietly() {
        let s = ReconcileSnapshot { duplicate: true, ..forming() };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::Duplicate, withdraw: false, mark_unwinnable: false }
        );
    }
}
