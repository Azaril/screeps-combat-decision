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
    /// A forming squad has a slot with a QUEUED or IN-FLIGHT spawn this tick (a member is banking/spawning).
    /// The deep-reach fix (Break #1): the inter-member banking gap can exceed the lease window, so refreshing
    /// ONLY on the exact `forming_progress` tick lets the lease lapse BETWEEN members → re-field churn that
    /// orphans the early roster. Refreshing while a member is in flight keeps a SLOW-but-fielding roster
    /// alive. BOUNDED by `forming_budget_remaining` so a genuinely-unfieldable squad still gives up.
    pub forming_in_flight: bool,
    /// The forming exemption budget has not yet been exhausted (a squad-age bound on how long the
    /// forming-in-flight refresh may extend the lease). False ⇒ the forming refresh stops and the squad
    /// gives up even if a member is still nominally in flight — so the immortal-squad failure can't recur.
    pub forming_budget_remaining: bool,
    /// The squad has its FULL roster (rally released) and is TRAVELING to the target — not yet engaged, not
    /// yet in the target room. The travel-phase has no focus + is not forming, so the base lease lapses
    /// mid-hop (Break #2 travel half — the live W7N7 1-slot lapse). Refresh while traveling + progressing.
    pub traveling: bool,
    /// The traveling squad made POSITIONAL progress toward the target this reconcile (closed distance). True
    /// only while the squad is actually advancing — a stuck/blocked traveler stops progressing and the lease
    /// lapses (the travel refresh is bounded by progress, like `forming_progress` bounds the forming one).
    pub travel_progress: bool,
    /// The travel exemption budget has not yet been exhausted (an absolute travel-time bound). False ⇒ the
    /// travel refresh stops and a squad that can never arrive gives up.
    pub travel_budget_remaining: bool,
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
///
/// DEEP-REACH FIX (the "fielded squad never reaches/engages" bug, two added lease refreshes):
/// (1) FORMING IN-FLIGHT — the inter-member BANKING GAP under spawn contention can exceed the lease window,
/// so `forming_progress` (the exact present++ tick) is too sparse: the lease lapses BETWEEN members → re-
/// field churn that orphans the early roster (the live W7N4 healer pile-up). While `forming` AND a member is
/// `forming_in_flight` (a slot has a queued/in-flight spawn) the lease is refreshed through the gap, BOUNDED
/// by `forming_budget_remaining` (a per-generation forming clock) so a truly-unfieldable squad still gives up.
/// (2) TRAVEL — a FULL-ROSTER squad that has departed home but not yet arrived/engaged is `traveling`: it has
/// no focus and is not forming, so the base lease lapses MID-HOP (the live W7N7 1-slot lapse). While it is
/// `traveling` AND making positional `travel_progress` (closing distance) the lease is refreshed, BOUNDED by
/// `travel_budget_remaining` (an absolute travel clock) so a squad that can never arrive still gives up.
pub fn reconcile(s: ReconcileSnapshot) -> ReconcileAction {
    let resolved = s.engaged_once && s.in_target_room && !s.has_focus && s.has_members;
    // A forming squad keeps its lease alive past the deadline (bounded — see the doc comment) while it is
    // making spawn PROGRESS (a member just appeared) OR a member is IN FLIGHT (banking/spawning the next
    // member, even on a tick the present count is flat). The in-flight refresh is bounded by
    // `forming_budget_remaining` so a genuinely-unfieldable squad still gives up (no immortal squad). It must
    // NOT count as a give-up even though it has no focus and the lease lapsed.
    let forming_progressing =
        s.forming && s.has_members && (s.forming_progress || (s.forming_in_flight && s.forming_budget_remaining));
    // A FULL-ROSTER squad TRAVELING to the target keeps its lease alive while it is making positional
    // progress (closing distance), bounded by `travel_budget_remaining` — so the lease does not lapse
    // mid-hop (the W7N7 travel-phase lapse) but a squad that can never arrive still gives up.
    let traveling_progressing = s.traveling && s.travel_progress && s.travel_budget_remaining && !s.engaged_once;
    let gave_up = s.deadline_lapsed && !s.has_focus && !resolved && !forming_progressing && !traveling_progressing;

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

    // Refresh the lease while actively engaging (a long fight / vision gap), while a forming squad is still
    // making spawn progress / has a member in flight (the rally-stall fix — the assembly is not torn down
    // mid-form), OR while a full-roster squad is traveling + closing on the target (the mid-hop lapse fix).
    if s.has_focus || forming_progressing || traveling_progressing {
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
            forming_in_flight: false,
            forming_budget_remaining: true,
            traveling: false,
            travel_progress: false,
            travel_budget_remaining: true,
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

    /// DEEP-REACH FIX (Break #1): a forming squad PAST its lease with present count FLAT (no `forming_progress`
    /// this tick) but a member still IN FLIGHT (banking/spawning the next member) must be KEPT — the lease is
    /// refreshed during the inter-member banking gap so the slow-but-fielding roster is not torn down + re-
    /// fielded. The pre-fix lease (refresh only on the exact present++ tick) lapsed in this gap → churn.
    #[test]
    fn forming_in_flight_past_lease_is_kept_not_retired() {
        let s = ReconcileSnapshot {
            deadline_lapsed: true,
            forming: true,
            forming_progress: false, // present is flat this tick (between members)
            forming_in_flight: true, // but a member is banking/spawning
            forming_budget_remaining: true,
            ..forming()
        };
        assert_eq!(reconcile(s), ReconcileAction::KeepRefreshLease);
    }

    /// DEEP-REACH FIX bound: the forming in-flight refresh is BOUNDED — once `forming_budget_remaining` is
    /// false (the squad has been forming too long) the lease lapses even with a member nominally in flight,
    /// so a genuinely-unfieldable squad still gives up (no immortal squad).
    #[test]
    fn forming_in_flight_past_budget_still_gives_up() {
        let s = ReconcileSnapshot {
            deadline_lapsed: true,
            forming: true,
            forming_progress: false,
            forming_in_flight: true,
            forming_budget_remaining: false, // budget exhausted
            ..forming()
        };
        assert_eq!(
            reconcile(s),
            ReconcileAction::Retire { reason: RetireReason::GaveUp, withdraw: false, mark_unwinnable: true }
        );
    }

    /// DEEP-REACH FIX (Break #2 travel half): a FULL-ROSTER squad TRAVELING to the target past its lease,
    /// still closing distance (positional progress), must be KEPT — the travel lease is refreshed so the
    /// squad does not lapse MID-HOP (the live W7N7 1-slot lapse). It has no focus + is not forming, so the
    /// base lease would otherwise give up before it ever arrives.
    #[test]
    fn traveling_full_roster_past_lease_is_kept_not_retired() {
        let s = ReconcileSnapshot {
            deadline_lapsed: true,
            forming: false, // full roster — past forming
            traveling: true,
            travel_progress: true,
            travel_budget_remaining: true,
            ..forming()
        };
        assert_eq!(reconcile(s), ReconcileAction::KeepRefreshLease);
    }

    /// DEEP-REACH FIX travel bound: the travel refresh is BOUNDED — a squad that can never arrive (no
    /// positional progress, or the absolute travel budget exhausted) still gives up.
    #[test]
    fn traveling_without_progress_or_budget_gives_up() {
        let stuck = ReconcileSnapshot { deadline_lapsed: true, traveling: true, travel_progress: false, travel_budget_remaining: true, ..forming() };
        assert_eq!(
            reconcile(stuck),
            ReconcileAction::Retire { reason: RetireReason::GaveUp, withdraw: false, mark_unwinnable: true }
        );
        let over_budget = ReconcileSnapshot { deadline_lapsed: true, traveling: true, travel_progress: true, travel_budget_remaining: false, ..forming() };
        assert_eq!(
            reconcile(over_budget),
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
