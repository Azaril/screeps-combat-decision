//! ADR 0032 v1.2 — the GLOBAL EV-maximizing squad↔objective matching (P-AUCTION).
//!
//! v1.1 (LIVE) scores each `(squad, objective)` pairing in a common energy-equivalent currency
//! (`objective_value::value_e` × `composition::pairing_ev`/`quantize_ev`) and picks each squad's
//! target GREEDILY (`best_by_ev` per squad). The greedy loop has the defect ADR 0032 §Problem names:
//! Phase A iterates squads in order, each claims its own best + marks it `covered`, so squad A can
//! grab the objective squad B was strictly better suited for. v1.2 REPLACES that per-squad selection
//! with ONE global assignment solve per scan — a deterministic Hungarian / Kuhn–Munkres maximizing
//! TOTAL EV over the rectangular `N×K` matrix (rows = assignable squads, cols = top-C objectives +
//! `StayPut` + `Recycle`). No merge column yet (that is v2, ADR 0032 §"Merge / attach").
//!
//! **Why this is a pure kernel** (in the decision crate, not the bot): the matrix is built from
//! value-type facts (each squad's `SquadCapabilities` + each objective's projected
//! `value_e`/`DefenseProfile`/travel) and the solve is pure integer combinatorics — so the bot
//! (`squad_manager`) and the offline eval (`run_auction_flow`) run the SAME optimizer, no fork. The
//! bot projects its `ObjectiveKind`/intel into the [`ObjectiveCell`] facts (exactly as it already
//! projects into `objective_ev_q` for v1.1); the harness builds synthetic ones.
//!
//! **Determinism (the load-bearing risk, ADR 0032 §Determinism / ADR 0020 §6):**
//! - the EV is INTEGER-quantized (`i64`, via `quantize_ev`) BEFORE it ever enters the combinatorial
//!   branch — no `f32` feeds a result-affecting comparison;
//! - rows are `Vec`-ordered by the caller (the bot orders by STABLE squad id, never an `Entity`
//!   index); columns are `Vec`-ordered (top-C objectives by the caller's cheap pre-rank, then the
//!   fixed `StayPut`/`Recycle` columns);
//! - the augmenting-path search visits columns in `Vec` order and ties break on the smallest
//!   `(row, col)` index — a stable lexicographic tie-break;
//! - NO `HashMap` on any path (only `Vec`s + fixed-size scratch arrays).
//!
//! The result is a stable `row -> col` assignment that the bot then APPLIES (current-objective →
//! Keep, a new objective → the v1.1 in-place rebind, `Recycle` → retire + zero-orphan recall).

use crate::composition::{pairing_ev, pairing_p_win, quantize_ev, PairingParams, SquadCapabilities, SquadRole};
use crate::doctrine::EnemyForce;
use crate::force_sizing::DefenseProfile;
use crate::objective_value::{value_e, ObjectiveIntel, ObjectiveValueKind};

/// The `EV = −∞` sentinel for an INFEASIBLE pairing (ADR 0032 §Integration: claimed-by-another /
/// backoff/unwinnable / capability-incompatible). A solve never picks an infeasible cell unless it is
/// FORCED to (a row with no feasible column at all) — and the apply layer treats a forced-infeasible
/// assignment as "no admissible move" (the row keeps its current objective / recycles), so an
/// infeasible pairing is never acted on. Kept well clear of `i64::MIN` so the Hungarian's row/column
/// potential arithmetic (subtractions) cannot overflow.
pub const INFEASIBLE_EV: i64 = i64::MIN / 4;

/// What a matrix COLUMN represents — an objective the squad could (re)bind to, or one of the two
/// fixed alternative columns the EV-positive gate needs (ADR 0032 §EV-positive gate). The bot maps a
/// chosen column back to an action: an `Objective`'s `id` matching the row's current objective →
/// Keep; a different `id` → the in-place rebind; `StayPut` → Keep (re-score of the current fight);
/// `Recycle` → retire + the zero-orphan recall.
///
/// `StayPut`/`Recycle` are **per-row** columns (one of each per assignable squad, carrying the row
/// index) — NOT shared. Hungarian column-exclusivity is exactly what we want for an OBJECTIVE (no
/// double-claim), but two squads must be able to BOTH recycle (or both stay put) in the same solve, so
/// each row owns a private StayPut + Recycle column that is feasible only for itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    /// A real objective the squad could be matched to, identified by its stable objective id (the bot's
    /// `ObjectiveId.0`; the harness uses a synthetic id). Top-`C` of these by the caller's cheap pre-rank.
    /// EXCLUSIVE — at most one squad per objective (the column-exclusivity that retires the v1 double-
    /// claim guard, ADR 0032 §Integration).
    Objective { id: u32 },
    /// Row `row` CONTINUES its current objective, re-scored with its current survivors (the gate's
    /// "beat the fight you're already in" alternative). Private to `row` (`INFEASIBLE_EV` for every
    /// other row), so it never contends with another squad's StayPut.
    StayPut { row: usize },
    /// Row `row` RECYCLES — `recycle_ev` (`value_e(recycle_refund) − walk`). The floor that prevents a
    /// net-negative commit. Private to `row` so every surplus squad can recycle in the same solve.
    Recycle { row: usize },
    /// ADR 0032 v2 / ADR 0027 — the MERGE column (the pending-slot Lanchester transfer, lines 256-312
    /// of ADR 0027 + ADR 0032 §"Merge / attach as a first-class column", lines 101-107). One column per
    /// FORMING RECEIVER squad `receiver_row` that has an OPEN pending spawn slot. The DONOR row matched
    /// to this column sheds its role-matched member(s) INTO `receiver_row`'s pending slot; the cell EV is
    /// the receiver's MARGINAL P(win) lift (NOT a fresh pairing). EXCLUSIVE like an `Objective` column —
    /// at most one donor merges into a given receiver per solve (one open slot ⇒ one transfer). The
    /// receiver itself is INFEASIBLE in its own Merge column (no self-merge — ADR 0027 line 274).
    Merge { receiver_row: usize },
}

/// A capability-class pre-filter tag (ADR 0032 §Integration: "`capability_class` stays a cheap
/// pre-filter"). A squad whose class does not match an objective's class yields an `INFEASIBLE_EV`
/// cell — the column-feasibility filter that REPLACES the v1 `best_reassignment_near` `compatible`
/// predicate. Mirrors the bot's `CapabilityClass`; the kernel stays bot-enum-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CapClass {
    Defense,
    #[default]
    Offense,
    Declaim,
}

/// One assignable squad ROW: its surviving capability (read once off its fielded composition — NOT an
/// `optimize_composition` candidate search) + its class + its CURRENT objective (for the `StayPut`
/// re-score) + the recycle refund. The bot builds one per admitted squad in STABLE id order; the
/// harness builds synthetic ones.
#[derive(Clone, Copy, Debug, Default)]
pub struct SquadRow {
    /// The squad's surviving capability vector (`composition.capabilities(member_energy)`).
    pub caps: SquadCapabilities,
    /// The squad's capability class — the cheap pre-filter against each objective's class.
    pub class: CapClass,
    /// The squad's CURRENT objective id (the `StayPut` column re-scores THIS) — `None` if its objective
    /// vanished (`objective_gone`), in which case `StayPut` is `INFEASIBLE_EV` (any positive move wins).
    pub current_objective: Option<u32>,
    /// The energy-equivalent refund of recycling this squad's bodies, minus the walk-home cost — the
    /// `Recycle` column's EV (ADR 0032 §EV-positive gate). Usually small/zero; the floor below which a
    /// net-negative objective is never taken.
    pub recycle_ev: i64,
    /// ── ADR 0032 v2 / ADR 0027 MERGE fields (the pending-slot Lanchester transfer) ──────────────────
    /// As a DONOR: this squad is MERGE-ELIGIBLE (ADR 0027 line 273) — terminal Resolved/ObjectiveGone WITH
    /// survivors, OR over-rostered, OR a forming squad consolidating. `false` ⇒ never a merge donor (every
    /// `Merge` cell with this row as donor is `INFEASIBLE_EV`). A mid-fight squad is NOT eligible (it never
    /// weakens mid-engagement — ADR 0027 line 273 "donor sheds, never weakens mid-fight").
    pub merge_eligible: bool,
    /// As a DONOR: the combined capability of the member(s) this squad would SHED into a receiver's pending
    /// slot (added to the receiver's caps for the marginal-lift P(win)). Zero ⇒ nothing to shed.
    pub sheddable: SquadCapabilities,
    /// As a DONOR: the ROLE bitmask of the sheddable member(s) ([`role_bit`]). A `Merge` is feasible only if
    /// one of these roles matches an OPEN pending slot role of the receiver (`sheddable_roles &
    /// open_slot_roles != 0`) — the role-compatibility half of the pending-slot guard (ADR 0027 line 258).
    pub sheddable_roles: u8,
    /// As a RECEIVER: the ROLE bitmask of this squad's OPEN (unfilled) pending spawn slots ([`role_bit`]).
    /// Non-zero ⇒ this row gets a `Merge { receiver_row }` column. Zero ⇒ no open slot ⇒ no Merge column
    /// (a full/non-forming squad is never a receiver — the receiver must be FORMING with a pending slot,
    /// ADR 0027 line 275).
    pub open_slot_roles: u8,
}

/// The single-bit role tag for the merge role-match bitmask ([`SquadRow::sheddable_roles`] /
/// [`SquadRow::open_slot_roles`]). Deterministic, fixed per [`SquadRole`] variant; the bot ORs these for
/// its sheddable members / open slots, the kernel ANDs to test role compatibility (ADR 0027 line 258 —
/// "a creep may transfer ONLY to fill that squad's PENDING SPAWN SLOT (compatible role)"). No `HashMap`.
pub fn role_bit(role: SquadRole) -> u8 {
    match role {
        SquadRole::Tank => 1 << 0,
        SquadRole::Healer => 1 << 1,
        SquadRole::RangedDPS => 1 << 2,
        SquadRole::MeleeDPS => 1 << 3,
        SquadRole::Dismantler => 1 << 4,
        SquadRole::Hauler => 1 << 5,
        SquadRole::Declaimer => 1 << 6,
    }
}

/// Sum two capability vectors (the receiver's surviving caps + the donor's sheddable caps) for the
/// marginal-lift P(win) (ADR 0032 v2 / ADR 0027 — `P(win | B.comp + S.sheddable_members)`). Saturating
/// (no overflow), field-wise; `tank_effective_hp` takes the MAX (the toughest tank, not a sum — a soak is
/// the single hardest body, mirroring [`SquadCapabilities`]'s own definition).
fn merged_caps(receiver: SquadCapabilities, donor_shed: SquadCapabilities) -> SquadCapabilities {
    SquadCapabilities {
        heal_per_tick: receiver.heal_per_tick.saturating_add(donor_shed.heal_per_tick),
        structure_dps: receiver.structure_dps.saturating_add(donor_shed.structure_dps),
        tank_effective_hp: receiver.tank_effective_hp.max(donor_shed.tank_effective_hp),
    }
}

/// One candidate objective COLUMN: the bot's projected facts the cell EV reads (exactly the
/// `objective_ev_q` inputs — `value_e` kind + intel, the `DefenseProfile`, the optional hostile
/// `EnemyForce`) + its stable id + class + per-row travel. The kernel computes the cell EV from these
/// with the SAME v1.1 helpers, so the global solve and the v1.1 per-squad score agree.
#[derive(Clone, Debug)]
pub struct ObjectiveCell {
    /// Stable objective id (the bot's `ObjectiveId.0`) — the column identity carried into the result.
    pub id: u32,
    /// The objective's capability class — matched against each row's `class` (the pre-filter).
    pub class: CapClass,
    /// `value_e` inputs (ADR 0032 §EV currency): the kind + the projected intel.
    pub value_kind: ObjectiveValueKind,
    pub intel: ObjectiveIntel,
    /// The defense profile the P(win) is judged against (towers/breach/safe-mode).
    pub defense: DefenseProfile,
    /// The hostile-creep force the P(win) attritions against (`None` ⇒ genuinely undefended).
    pub enemy: Option<EnemyForce>,
    /// Travel rooms HOME→objective per ROW (parallel to the matrix rows). A farther objective prices a
    /// linear penalty AND (via the caller-supplied per-row `onsite_window`) a shrinking on-site window.
    /// One entry per row; index `r` is row `r`'s travel to this objective.
    pub travel_rooms_per_row: Vec<u32>,
    /// Per-ROW feasibility override: `false` ⇒ this objective is infeasible for that row REGARDLESS of
    /// EV (claimed-by-another / backoff / a row already on it being excluded as a reassign target). The
    /// bot fills this from `is_claimed`/`is_unwinnable_now`/the no-ping-pong exclusion; index `r` = row
    /// `r`. Empty ⇒ feasible for all rows (the harness's simple case).
    pub feasible_per_row: Vec<bool>,
}

impl ObjectiveCell {
    fn travel_for(&self, row: usize) -> u32 {
        self.travel_rooms_per_row.get(row).copied().unwrap_or(0)
    }
    fn feasible_for(&self, row: usize) -> bool {
        self.feasible_per_row.get(row).copied().unwrap_or(true)
    }
}

/// The inputs the kernel needs to BUILD the matrix that are NOT per-cell: the on-site window (a
/// generous reach window — a farther objective gets fewer on-site ticks via the per-row travel, ADR
/// 0032 line 37) + the EV pairing tunables.
#[derive(Clone, Copy, Debug)]
pub struct MatrixParams {
    /// The on-site window (ticks) used for the P(win) deliverable. The bot passes its
    /// `MAX_TRAVEL_BUDGET` proxy; the helper shrinks reach automatically as travel grows.
    pub onsite_window: u32,
    /// The v1.1 EV pairing tunables (travel weight + dynamic margin).
    pub pairing: PairingParams,
    /// ADR 0032 v2 — the linear EV penalty per room the donor's shed member(s) must travel to reach the
    /// receiver's rally (`transfer_cost` in ADR 0032 line 104). Small (mirrors `pairing.w_travel`) so it
    /// only breaks near-EV ties between two candidate receivers.
    pub w_transfer: f32,
}

impl Default for MatrixParams {
    fn default() -> Self {
        MatrixParams { onsite_window: 1500, pairing: PairingParams::default(), w_transfer: 1.0 }
    }
}

/// The built EV matrix — `rows × cols` of INTEGER-quantized EV (`i64`), plus the column descriptors
/// (so the caller maps a chosen column back to an action). `ev[r][c]` is row `r`'s quantized EV in
/// column `c`; an infeasible cell is [`INFEASIBLE_EV`]. Pure data; consumed by [`solve_assignment`].
#[derive(Clone, Debug)]
pub struct EvMatrix {
    /// `rows` = the number of assignable squads (the `SquadRow` count).
    pub rows: usize,
    /// The column descriptors in `Vec` order: top-C objectives, then `StayPut`, then `Recycle`.
    pub columns: Vec<ColumnKind>,
    /// Row-major quantized EV: `ev[r * cols + c]`.
    ev: Vec<i64>,
}

impl EvMatrix {
    /// The number of columns.
    pub fn cols(&self) -> usize {
        self.columns.len()
    }
    /// The quantized EV at `(row, col)` (panics out of bounds — an internal invariant).
    pub fn at(&self, row: usize, col: usize) -> i64 {
        self.ev[row * self.columns.len() + col]
    }
}

/// THE cell EV (ADR 0032 §"EV of a (squad, objective) pairing"): `quantize_ev(pairing_ev(caps,
/// objective, enemy) − w_travel·travel)`, reusing the v1.1 helpers + `value_e` VERBATIM. Returns
/// [`INFEASIBLE_EV`] when the pairing is infeasible (capability-class mismatch — the cheap pre-filter
/// — or the per-row feasibility override). Pure + deterministic.
fn cell_ev(row: &SquadRow, row_idx: usize, obj: &ObjectiveCell, params: &MatrixParams) -> i64 {
    // Capability-class pre-filter (ADR 0032 §Integration) + the per-row feasibility override
    // (claimed-by-another / backoff / no-ping-pong) — an infeasible pairing is NEVER chosen.
    if row.class != obj.class || !obj.feasible_for(row_idx) {
        return INFEASIBLE_EV;
    }
    let val = value_e(obj.value_kind, &obj.intel);
    let ev = pairing_ev(
        row.caps,
        &obj.defense,
        obj.enemy,
        val,
        params.onsite_window,
        obj.travel_for(row_idx),
        &params.pairing,
    );
    quantize_ev(ev)
}

/// THE MERGE cell EV (ADR 0032 v2 §"Merge / attach", lines 101-107 + ADR 0027 lines 256-312): donor row
/// `donor_idx` merges its sheddable member(s) into the FORMING receiver `recv_idx`'s open pending slot.
/// The EV is the receiver's MARGINAL P(win) LIFT (NOT a fresh pairing, NOT the donor's own EV):
/// `[P(win | B.caps + S.sheddable) − P(win | B.caps)] · value_e(B.objective) − w_transfer · travel`.
///
/// Returns [`INFEASIBLE_EV`] unless ALL of the pending-slot Lanchester guard holds (ADR 0027 line 258):
/// - the donor is `merge_eligible` (terminal-with-survivors / over-rostered / forming-consolidate) AND
///   actually has sheddable caps + role(s);
/// - `donor_idx != recv_idx` (no self-merge — ADR 0027 line 274);
/// - the receiver has an OPEN pending slot whose role MATCHES a sheddable role
///   (`sheddable_roles & open_slot_roles != 0` — the role-compatibility half of the guard);
/// - the receiver has a CURRENT objective whose cell is found + class-matches the merged force (B is
///   forming FOR an objective; the merged force fights B's objective).
///
/// A NON-eligible donor, a self-merge, a role mismatch, or a receiver with no open slot is simply NEVER a
/// feasible cell — which is exactly why a DILUTIVE split (peeling into a new under-strength squad that does
/// not fill an existing slot) can NEVER be selected: it is not representable as a column at all (ADR 0027
/// lines 308-312, the whole guard). Pure + deterministic (no `HashMap`; integer-quantized).
#[allow(clippy::too_many_arguments)]
fn merge_cell_ev(
    donor: &SquadRow,
    donor_idx: usize,
    recv: &SquadRow,
    recv_idx: usize,
    objectives: &[ObjectiveCell],
    transfer_travel: u32,
    params: &MatrixParams,
) -> i64 {
    // No self-merge (ADR 0027 line 274) + the donor must be merge-eligible with something to shed.
    if donor_idx == recv_idx || !donor.merge_eligible {
        return INFEASIBLE_EV;
    }
    // Role-compatibility half of the pending-slot guard: a sheddable role must match an OPEN slot role.
    if donor.sheddable_roles & recv.open_slot_roles == 0 {
        return INFEASIBLE_EV;
    }
    // The receiver must be FORMING for an objective whose cell we can re-score the marginal lift against.
    let Some(cur) = recv.current_objective else { return INFEASIBLE_EV };
    let Some(obj_b) = objectives.iter().find(|o| o.id == cur) else { return INFEASIBLE_EV };
    // The merged force fights B's objective — the class must match (a cross-class merge is incoherent).
    if recv.class != obj_b.class {
        return INFEASIBLE_EV;
    }
    let val = value_e(obj_b.value_kind, &obj_b.intel);
    // The marginal LIFT is the pure P(win) DELTA the merged force buys for the RECEIVER's objective. Both
    // P(win)s use the SAME `onsite_window` (the receiver is the coordination unit and fights B's objective at
    // B's reach, ADR 0027 line 277 — the reach folds into both identically and cancels in the delta). The
    // merge changes only the FORCE, not where B fights; the transfer's own reach cost is the separate
    // `w_transfer · transfer_travel` penalty below (ADR 0032 line 104).
    let base_p = pairing_p_win(recv.caps, &obj_b.defense, obj_b.enemy, params.onsite_window, &params.pairing);
    let merged_p = pairing_p_win(
        merged_caps(recv.caps, donor.sheddable),
        &obj_b.defense,
        obj_b.enemy,
        params.onsite_window,
        &params.pairing,
    );
    let lift = (merged_p - base_p) * val;
    let ev = lift - params.w_transfer * transfer_travel as f32;
    quantize_ev(ev)
}

/// Build the `N×K` EV matrix (ADR 0032 §"The matching"): rows = the assignable squads (caller-ordered
/// by STABLE id), columns = the top-C objective cells (caller-pre-ranked) + a `StayPut` column +
/// a `Recycle` column. Every cell is the INTEGER-quantized EV ([`cell_ev`]); an infeasible cell is
/// [`INFEASIBLE_EV`]. Pure — the same inputs always yield the same matrix (byte-identical).
///
/// - `StayPut[r]` = `cell_ev(row r, row r's current objective)` if that objective is among `objectives`
///   (re-scored with the row's current survivors), else (`current_objective == None` / gone)
///   [`INFEASIBLE_EV`]. This is the gate's "beat the fight you're already in" alternative.
/// - `Recycle[r]` = the row's `recycle_ev` — the net-negative floor.
///
/// This is the v1.2 builder (NO merge columns) — preserved BYTE-IDENTICAL for the v1.2 kernel tests +
/// `run_auction_flow`. The v2 [`build_ev_matrix_with_merge`] adds the [`ColumnKind::Merge`] columns; this
/// delegates to it with an empty merge-travel matrix (no Merge column is feasible without receiver open
/// slots, so the result is identical whether or not the merge pass runs — but it short-circuits cleanly).
pub fn build_ev_matrix(squads: &[SquadRow], objectives: &[ObjectiveCell], params: &MatrixParams) -> EvMatrix {
    build_ev_matrix_with_merge(squads, objectives, &[], params)
}

/// ADR 0032 v2 — build the matrix WITH the [`ColumnKind::Merge`] column class (the pending-slot Lanchester
/// transfer). Identical to [`build_ev_matrix`] PLUS: for every receiver row whose `open_slot_roles != 0` (a
/// forming squad with an open pending slot), append one `Merge { receiver_row }` column; each donor row's
/// cell in that column is [`merge_cell_ev`] (the marginal P(win) lift, gated by the pending-slot guard).
///
/// `merge_travel_rooms` is an OPTIONAL row-major `rows × rows` matrix of donor→receiver-rally travel
/// (`merge_travel_rooms[donor * rows + receiver]`); empty ⇒ 0 travel for all (the harness's simple case).
/// The Merge columns are appended AFTER Objective/StayPut/Recycle, so the v1.2 column indices are unchanged
/// (StayPut/Recycle bases stay `objectives.len()` / `objectives.len() + rows`).
pub fn build_ev_matrix_with_merge(
    squads: &[SquadRow],
    objectives: &[ObjectiveCell],
    merge_travel_rooms: &[u32],
    params: &MatrixParams,
) -> EvMatrix {
    let rows = squads.len();
    let merge_travel = |donor: usize, receiver: usize| -> u32 {
        merge_travel_rooms.get(donor * rows + receiver).copied().unwrap_or(0)
    };
    // Receivers that get a Merge column: forming rows with an OPEN pending slot (role bitmask non-zero),
    // in stable row order (deterministic — no HashMap).
    let merge_receivers: Vec<usize> = (0..rows).filter(|&r| squads[r].open_slot_roles != 0).collect();

    // Columns: the C objective columns (shared/exclusive), then a PRIVATE StayPut + Recycle column per
    // row (so two squads can both stay/recycle without contending — see [`ColumnKind`]), then one
    // exclusive Merge column per forming receiver (ADR 0032 v2).
    let mut columns: Vec<ColumnKind> = Vec::with_capacity(objectives.len() + 2 * rows + merge_receivers.len());
    for obj in objectives {
        columns.push(ColumnKind::Objective { id: obj.id });
    }
    let stay_base = objectives.len();
    for r in 0..rows {
        columns.push(ColumnKind::StayPut { row: r });
    }
    let recycle_base = objectives.len() + rows;
    for r in 0..rows {
        columns.push(ColumnKind::Recycle { row: r });
    }
    let merge_base = columns.len();
    for &recv in &merge_receivers {
        columns.push(ColumnKind::Merge { receiver_row: recv });
    }
    let cols = columns.len();

    let mut ev = vec![INFEASIBLE_EV; rows * cols];
    for (r, row) in squads.iter().enumerate() {
        // Objective columns.
        for (c, obj) in objectives.iter().enumerate() {
            ev[r * cols + c] = cell_ev(row, r, obj, params);
        }
        // Merge columns: this row `r` is the DONOR; each Merge column targets a receiver row.
        for (k, &recv) in merge_receivers.iter().enumerate() {
            let recv_row = &squads[recv];
            ev[r * cols + merge_base + k] =
                merge_cell_ev(row, r, recv_row, recv, objectives, merge_travel(r, recv), params);
        }
        // This row's PRIVATE StayPut column (re-score the CURRENT objective with the row's survivors).
        // Infeasible (a gone objective) ⇒ INFEASIBLE_EV so any positive move beats it.
        let stay_col = stay_base + r;
        ev[r * cols + stay_col] = match row.current_objective {
            Some(cur) => objectives
                .iter()
                .find(|o| o.id == cur)
                .map(|o| {
                    // Re-score the CURRENT objective ignoring any per-row feasibility override (a squad may
                    // always keep its own fight even if the objective is "claimed" — by itself). Class still
                    // gates (a class change can't happen for the current objective by construction).
                    if row.class != o.class {
                        INFEASIBLE_EV
                    } else {
                        let val = value_e(o.value_kind, &o.intel);
                        let pev = pairing_ev(row.caps, &o.defense, o.enemy, val, params.onsite_window, o.travel_for(r), &params.pairing);
                        quantize_ev(pev)
                    }
                })
                .unwrap_or(INFEASIBLE_EV),
            None => INFEASIBLE_EV,
        };
        // This row's PRIVATE Recycle column.
        let recycle_col = recycle_base + r;
        ev[r * cols + recycle_col] = row.recycle_ev;

        // INVARIANT (ADR 0032 §EV-positive gate): every row MUST own a FEASIBLE zero-cost escape so the
        // perfect-matching solver can leave the row "unassigned" (recycle) instead of FORCING it onto a
        // net-negative objective. [`solve_assignment`] returns the optimum for the matrix it is given; it
        // does not invent escapes. This is exactly the per-row Recycle@`recycle_ev` cell just written.
        // Trips in test/debug if the escape is ever lost or made infeasible (compiled out of wasm release).
        debug_assert_ne!(
            ev[r * cols + recycle_col], INFEASIBLE_EV,
            "row {r} has no feasible Recycle escape — solve_assignment would force it onto a negative cell",
        );
    }

    EvMatrix { rows, columns, ev }
}

/// The result of a solve: `assignment[r] = Some(c)` ⇒ row `r` is matched to column `c`; `None` ⇒ the
/// row is unmatched — which happens when the row is matched to a dummy PADDING column (only possible
/// when `rows > cols`, i.e. the matrix has fewer columns than rows) or is FORCED onto an
/// [`INFEASIBLE_EV`] real cell (no admissible move). For production matrices ([`build_ev_matrix`],
/// `cols >= rows`) every row is matched to a real column — its own Recycle@0 escape in the worst case —
/// so `None` only ever means "forced onto an infeasible cell". The caller maps each `(r, c)` to an
/// action via `matrix.columns[c]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// `row -> Some(col)` (or `None` for an unmatched row — a padding match or a forced-infeasible cell).
    /// Length == `matrix.rows`.
    pub row_to_col: Vec<Option<usize>>,
    /// The total quantized EV of the assignment (Σ of the chosen feasible cells; infeasible/unmatched
    /// contribute 0 — see [`solve_assignment`]). The headline metric the greedy-suboptimal test asserts.
    pub total_ev: i64,
}

/// A DETERMINISTIC rectangular Hungarian / Kuhn–Munkres that returns the OPTIMAL (max-total-EV)
/// assignment over the `rows × cols` integer matrix (ADR 0032 §"The matching"). The matrix is padded
/// to a square `n×n` (`n = max(rows, cols)`) with zero-EV dummy cells and the standard
/// minimize-Hungarian is run on the NEGATED EV (max-EV ⇒ min-(−EV)); padding rows/cols are dropped
/// from the result. A real cell at [`INFEASIBLE_EV`] is given a large positive cost so the minimizer
/// avoids it unless forced; a row matched to such a forced cell is reported as `None` (it contributes
/// 0 EV — the caller keeps/recycles it).
///
/// **CONTRACT — the caller MUST supply a per-row zero-cost escape.** This solver returns the optimum
/// FOR THE GIVEN MATRIX. It does NOT decide on its own whether a row should be left unassigned: a
/// perfect matching is always sought, and because the sole production constructor builds `cols >= rows`
/// (one objective column per objective PLUS a per-row `StayPut` and a per-row `Recycle` column), there
/// are NO dummy padding columns, so every REAL row is matched to a REAL column. To make "leave this
/// row unassigned" admissible, the caller must give that row a FEASIBLE, zero-cost escape column —
/// exactly what [`build_ev_matrix`] does via the per-row `Recycle` column at `recycle_ev` (typically
/// 0). With that escape present, a row whose every objective/StayPut cell is negative is matched to its
/// own Recycle@0 instead of being forced onto a negative cell, which is precisely the optional-assignment
/// behaviour the EV-positive gate needs.
///
/// WITHOUT such a per-row escape the optimality guarantee for a ROW does NOT translate to "good for
/// that row": a perfect-matching solver will FORCE a row whose real cells are all negative onto the
/// least-negative one (the global Σ is still maximal, but the row took a net-loss commit). A future
/// caller that builds a matrix without per-row escapes will trip the `debug_assert` below; production is
/// safe because [`build_ev_matrix`] always appends the per-row Recycle@0 escape.
///
/// HARD determinism (ADR 0032 §Determinism / ADR 0020 §6):
/// - operates on the INTEGER-quantized EV ONLY (no float in the combinatorial branch);
/// - `Vec`-ordered rows + columns;
/// - the augmenting-path search scans columns in index order and the row/column potentials are
///   updated with a strict `<` (first index wins) so ties break on the smallest `(row, col)` — a
///   stable lexicographic tie-break;
/// - no `HashMap` (only `Vec`s + fixed scratch).
///
/// CPU: `O(n³)` for `n = max(rows, cols)` — with `n ≤ ~14` (≤6 squads × ≤12 cols + pad) this is a few
/// thousand int ops, once per scan, well within the combat `StageClass::Always` budget (ADR 0032 line
/// 73).
pub fn solve_assignment(matrix: &EvMatrix) -> Assignment {
    let rows = matrix.rows;
    let cols = matrix.cols();
    if rows == 0 || cols == 0 {
        return Assignment { row_to_col: vec![None; rows], total_ev: 0 };
    }

    // CONTRACT GUARD (see the doc comment): every row must own at least one FEASIBLE (non-INFEASIBLE)
    // cell — its zero-cost escape — or the perfect-matching solver will FORCE it onto a negative cell.
    // [`build_ev_matrix`] guarantees this via the per-row Recycle@0 column; this catches any other caller
    // that builds an escape-less matrix. debug_assert ⇒ compiled out of the wasm release.
    debug_assert!(
        (0..rows).all(|r| (0..cols).any(|c| matrix.at(r, c) != INFEASIBLE_EV)),
        "solve_assignment: a row has NO feasible cell — the caller must supply a per-row zero-cost escape \
         (e.g. build_ev_matrix's per-row Recycle column); without it a row with all-negative real cells \
         is forced onto a negative cell",
    );

    let n = rows.max(cols);

    // Square cost matrix to MINIMIZE = −EV (so minimizing cost maximizes EV). Padding cells (a dummy
    // row or column) cost 0 (EV 0). An INFEASIBLE_EV real cell becomes a huge POSITIVE cost so the
    // minimizer avoids it unless forced. We work in i64 throughout — no float.
    //
    // Guard the negation: −INFEASIBLE_EV is a large positive that must not overflow when added to row/
    // column potentials. INFEASIBLE_EV = i64::MIN/4, so −it ≈ i64::MAX/4 — safe headroom.
    let cost = |r: usize, c: usize| -> i64 {
        if r < rows && c < cols {
            let ev = matrix.at(r, c);
            if ev == INFEASIBLE_EV {
                // A forbidden assignment: a large positive cost (the minimizer avoids it). Distinct from
                // the padding 0 so a real-but-infeasible cell is never preferred to a free padding cell.
                -INFEASIBLE_EV
            } else {
                -ev
            }
        } else {
            0 // padding (dummy row/col) — zero EV.
        }
    };

    // ── Jonker-Volgenant-style O(n³) Hungarian over `cost` (1-indexed potentials, the classic
    //    deterministic shortest-augmenting-path form). `u`/`v` are the row/column potentials; `p[j]` is
    //    the row matched to column `j` (0 = none); `way[j]` is the augmenting back-pointer. All ties
    //    resolve to the SMALLEST index (strict `<`), so the result is the lexicographically stable
    //    optimum (ADR 0032 §Determinism). No HashMap. ──
    let inf = i64::MAX / 4;
    let mut u = vec![0i64; n + 1];
    let mut v = vec![0i64; n + 1];
    let mut p = vec![0usize; n + 1]; // p[col] = row assigned to col (1-indexed; 0 = unassigned)
    let mut way = vec![0usize; n + 1];

    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize; // current column (0 = the virtual start)
        let mut minv = vec![inf; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = inf;
            let mut j1 = 0usize;
            // Scan columns in ASCENDING index order; strict `<` ⇒ the first (smallest) index wins a tie.
            for j in 1..=n {
                if !used[j] {
                    let cur = cost(i0 - 1, j - 1) - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            // Update potentials along the visited set.
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        // Augment along the back-pointers.
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }

    // p[col] = row (1-indexed). Invert to row -> col, dropping padding + infeasible matches.
    let mut row_to_col = vec![None; rows];
    let mut total_ev = 0i64;
    for (col, &row) in p.iter().enumerate().skip(1) {
        if row == 0 {
            continue;
        }
        let (r, c) = (row - 1, col - 1);
        if r < rows && c < cols {
            let ev = matrix.at(r, c);
            if ev != INFEASIBLE_EV {
                row_to_col[r] = Some(c);
                total_ev += ev;
            }
            // An INFEASIBLE_EV real cell ⇒ no admissible move for this row (left None, contributes 0).
        }
        // A real row matched to a PADDING column ⇒ unmatched (left None).
    }

    Assignment { row_to_col, total_ev }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(structure_dps: u32, heal: u32) -> SquadCapabilities {
        SquadCapabilities { heal_per_tick: heal, structure_dps, tank_effective_hp: 2000 }
    }

    /// A simple undefended objective worth `denial` energy-equiv, with no travel for any of `n_rows`.
    fn undefended_obj(id: u32, denial: f32, n_rows: usize) -> ObjectiveCell {
        ObjectiveCell {
            id,
            class: CapClass::Offense,
            value_kind: ObjectiveValueKind::Denial,
            // Denial value_e = denial_value × DENIAL_DISCOUNT(0.5); pass 2× so value_e == `denial`.
            intel: ObjectiveIntel { denial_value: denial * 2.0, ..Default::default() },
            defense: DefenseProfile::default(),
            enemy: None,
            travel_rooms_per_row: vec![0; n_rows],
            feasible_per_row: vec![true; n_rows],
        }
    }

    fn offense_row(structure_dps: u32, current: Option<u32>) -> SquadRow {
        SquadRow { caps: caps(structure_dps, 50), class: CapClass::Offense, current_objective: current, recycle_ev: 0, ..Default::default() }
    }

    /// A faithful model of the OLD per-squad GREEDY behaviour (ADR 0032 §Problem #2): iterate rows in
    /// order; each row claims its own best-EV still-unclaimed objective column (StayPut/Recycle allowed),
    /// marking it covered so a later row cannot take it. Returns the total EV — the baseline the global
    /// solve must STRICTLY beat in the headline test.
    fn greedy_baseline(matrix: &EvMatrix) -> i64 {
        let mut covered = vec![false; matrix.cols()];
        let mut total = 0i64;
        for r in 0..matrix.rows {
            let mut best: Option<(usize, i64)> = None;
            for (c, col) in matrix.columns.iter().enumerate() {
                // StayPut/Recycle are per-row (never contended); objective columns are first-come covered.
                let is_shared = matches!(col, ColumnKind::Objective { .. });
                if is_shared && covered[c] {
                    continue;
                }
                let ev = matrix.at(r, c);
                if ev == INFEASIBLE_EV {
                    continue;
                }
                if best.map(|(_, b)| ev > b).unwrap_or(true) {
                    best = Some((c, ev));
                }
            }
            if let Some((c, ev)) = best {
                if matches!(matrix.columns[c], ColumnKind::Objective { .. }) {
                    covered[c] = true;
                }
                total += ev;
            }
        }
        total
    }

    /// THE HEADLINE greedy-suboptimal proof (ADR 0032 §Sim, the point of P-AUCTION): construct
    /// 2 squads × 2 objectives where the per-squad greedy baseline is STRICTLY worse than the global
    /// Hungarian on total EV. Squad A is a strong all-rounder; squad B is weak. Objective H is
    /// high-value, objective L is low. Greedy lets A (iterated first) grab H — leaving B, which can BARELY
    /// clear H but trivially clears L, stuck on L for a poor total. The GLOBAL solve puts A on H and B on
    /// L? No — the genuine case: A is BETTER at L's *kind* and B can only win H. We make EV(A,H) the
    /// single largest cell so greedy (A first) takes H, stranding B on its weak option; the optimum is
    /// A→L, B→H. Asserts the Hungarian total > greedy total — a real swap, not a tautology.
    #[test]
    fn hungarian_strictly_beats_greedy_on_total_ev() {
        // Objective H needs a lot of structure_dps to win in-window; objective L is undefended + cheap to win.
        // Squad A: huge dps (wins both, big P(win)); squad B: modest dps (wins L for sure; H only barely).
        let squad_a = offense_row(1000, None);
        let squad_b = offense_row(120, None);

        // H: undefended high value; L: undefended lower value. Both winnable by both (undefended → P=1).
        // The trick: make EV(A,H) the global max so greedy(A-first) grabs H, but the OPTIMAL pairing
        // routes A→L and B→H for a higher TOTAL because EV(B,H)+EV(A,L) > EV(A,H)+EV(B,L).
        //
        // With undefended objectives P(win)=1 for any dps>0, so value dominates: to make greedy wrong we
        // give H and L the SAME value but make B INFEASIBLE on L (so greedy A-first takes H, leaving B with
        // only Recycle=0), whereas the optimum is A→L (feasible, full value) + B→H (feasible, full value).
        let val = 50_000.0;
        let mut obj_h = undefended_obj(0, val, 2);
        let mut obj_l = undefended_obj(1, val, 2);
        // B (row 1) cannot take L (claimed-by-another / incompatible-tile) — only A can.
        obj_l.feasible_per_row = vec![true, false];
        // A and B are equally good on H (both undefended → P=1). Force greedy's mistake: A iterated first
        // picks H (ties to L but L feasible for A too — make H marginally higher for A so greedy grabs it).
        obj_h.intel.denial_value = val * 2.0 + 2.0; // value_e(H) = val + 1 (× discount 0.5) — H edges L for A.

        let squads = [squad_a, squad_b];
        let objs = [obj_h, obj_l];
        let m = build_ev_matrix(&squads, &objs, &MatrixParams::default());

        let greedy = greedy_baseline(&m);
        let sol = solve_assignment(&m);

        // Greedy: A grabs H (its max), B can only take H (covered) → Recycle/StayPut 0. Optimum: B→H, A→L.
        assert!(
            sol.total_ev > greedy,
            "global Hungarian must STRICTLY beat greedy: hungarian={} greedy={} cols={:?} assign={:?}",
            sol.total_ev,
            greedy,
            m.columns,
            sol.row_to_col
        );
        // Verify the optimal SHAPE: A (row 0) → L (col 1); B (row 1) → H (col 0).
        assert_eq!(sol.row_to_col[1], Some(0), "B must take H (its only winnable objective)");
        assert_eq!(sol.row_to_col[0], Some(1), "A yields H to take L — the global swap greedy misses");
    }

    /// EV-positive gate (ADR 0032 §EV-positive gate): a sub-threshold objective is NOT taken — the
    /// `StayPut`/`Recycle` column wins. A squad already on a high-value fight (`current_objective`) is
    /// offered only a tiny-value new objective; the optimum keeps it on `StayPut`.
    #[test]
    fn ev_positive_gate_prefers_stayput_over_a_subthreshold_objective() {
        // The squad's CURRENT objective is high-value (id 0); a new, low-value objective is id 1.
        let high = undefended_obj(0, 100_000.0, 1);
        let low = undefended_obj(1, 5.0, 1);
        let row = offense_row(1000, Some(0)); // currently on objective 0
        // Exclude the current objective from the OBJECTIVE columns (no-ping-pong) so the only ways to keep
        // it are StayPut; the only NEW objective column is the low one.
        let mut low = low;
        low.feasible_per_row = vec![true];
        let squads = [row];
        let objs = [high.clone(), low]; // include high so StayPut can re-score it; it's the current obj
        let m = build_ev_matrix(&squads, &objs, &MatrixParams::default());
        let sol = solve_assignment(&m);
        let c = sol.row_to_col[0].expect("assigned");
        // The chosen column must NOT be the low-value new objective — StayPut (or the high column, equal EV)
        // wins. We assert it is not the low objective id.
        match m.columns[c] {
            ColumnKind::Objective { id } => assert_ne!(id, 1, "must not take the sub-threshold low objective"),
            ColumnKind::StayPut { .. } | ColumnKind::Recycle { .. } | ColumnKind::Merge { .. } => {}
        }
        // And the total EV must equal the high/StayPut EV (the squad keeps its valuable fight). The high
        // objective is col 0; row 0's private StayPut is col (objectives.len() + 0).
        let stay_col = 2; // 2 objectives → StayPut for row 0 at index 2
        let stay_ev = m.at(0, 0).max(m.at(0, stay_col));
        assert_eq!(sol.total_ev, stay_ev, "optimum keeps the high-value fight, not the low objective");
    }

    /// Recycle wins when NOTHING is net-positive (ADR 0032 §EV-positive gate floor): a squad with a gone
    /// current objective (StayPut infeasible) and only a loss-making objective recycles (EV 0 > a loss).
    #[test]
    fn recycle_wins_when_no_objective_is_net_positive() {
        // A heavily-defended objective the weak squad cannot win (P(win) ~ 0 → EV negative after travel).
        let mut obj = undefended_obj(0, 1.0, 1);
        obj.defense = DefenseProfile { safe_mode: true, ..Default::default() }; // safe mode → P(win)=0 → EV = −travel
        obj.travel_rooms_per_row = vec![5];
        let row = SquadRow { caps: caps(120, 50), class: CapClass::Offense, current_objective: None, recycle_ev: 0, ..Default::default() };
        let squads = [row];
        let objs = [obj];
        let m = build_ev_matrix(&squads, &objs, &MatrixParams::default());
        let sol = solve_assignment(&m);
        let c = sol.row_to_col[0].expect("assigned");
        assert_eq!(m.columns[c], ColumnKind::Recycle { row: 0 }, "a no-win squad recycles (EV 0) rather than walk into a loss");
        assert_eq!(sol.total_ev, 0, "recycle EV is 0 — the net-negative floor");
    }

    /// Capability-class pre-filter (ADR 0032 §Integration): a Defense squad is NEVER matched to an
    /// Offense objective (the cell is INFEASIBLE_EV) — the column-feasibility that replaces the v1
    /// `compatible` predicate. The defender recycles/stays rather than take the wrong-class objective.
    #[test]
    fn capability_class_is_a_hard_prefilter() {
        let offense_obj = undefended_obj(0, 100_000.0, 1); // class Offense
        let defender = SquadRow { caps: caps(1000, 50), class: CapClass::Defense, current_objective: None, recycle_ev: 0, ..Default::default() };
        let squads = [defender];
        let objs = [offense_obj];
        let m = build_ev_matrix(&squads, &objs, &MatrixParams::default());
        assert_eq!(m.at(0, 0), INFEASIBLE_EV, "a Defense squad ↔ an Offense objective is infeasible");
        let sol = solve_assignment(&m);
        // The only feasible columns are StayPut (infeasible — no current obj) and Recycle (0).
        let c = sol.row_to_col[0].expect("assigned");
        assert_eq!(m.columns[c], ColumnKind::Recycle { row: 0 }, "the defender recycles rather than take the offense objective");
    }

    /// Determinism #1 (ADR 0032 §Determinism): solving the SAME matrix twice yields a byte-identical
    /// assignment.
    #[test]
    fn solve_is_deterministic_run_twice() {
        let squads = [offense_row(1000, None), offense_row(500, None), offense_row(250, None)];
        let objs = [undefended_obj(0, 90_000.0, 3), undefended_obj(1, 60_000.0, 3), undefended_obj(2, 30_000.0, 3)];
        let m = build_ev_matrix(&squads, &objs, &MatrixParams::default());
        let a = solve_assignment(&m);
        let b = solve_assignment(&m);
        assert_eq!(a, b, "same matrix → byte-identical assignment");
    }

    /// Determinism #2 (ADR 0032 §Determinism, the load-bearing case): PERMUTING the input rows/cols
    /// yields the SAME logical assignment (each squad keeps its objective regardless of input order) —
    /// the stable lexicographic tie-break makes order irrelevant to the chosen pairings.
    #[test]
    fn solve_is_invariant_to_input_permutation() {
        // 3 distinct-value undefended objectives, 3 strong squads (all can win all). The optimum pairs the
        // strongest-feasible by value; with all-winnable equal-dps squads the optimum is one-objective-each.
        let s0 = offense_row(1000, None);
        let s1 = offense_row(1000, None);
        let s2 = offense_row(1000, None);
        let o_hi = undefended_obj(10, 90_000.0, 3);
        let o_mid = undefended_obj(20, 60_000.0, 3);
        let o_lo = undefended_obj(30, 30_000.0, 3);

        // Original order.
        let m1 = build_ev_matrix(&[s0, s1, s2], &[o_hi.clone(), o_mid.clone(), o_lo.clone()], &MatrixParams::default());
        let sol1 = solve_assignment(&m1);
        // Map row→objective-id for order-independent comparison.
        let ids1: Vec<Option<u32>> = sol1
            .row_to_col
            .iter()
            .map(|c| c.and_then(|c| match m1.columns[c] {
                ColumnKind::Objective { id } => Some(id),
                _ => None,
            }))
            .collect();

        // Permuted columns (objectives reordered) — each squad must still get the SAME objective set total.
        let m2 = build_ev_matrix(&[s0, s1, s2], &[o_lo, o_hi, o_mid], &MatrixParams::default());
        let sol2 = solve_assignment(&m2);
        let ids2: Vec<Option<u32>> = sol2
            .row_to_col
            .iter()
            .map(|c| c.and_then(|c| match m2.columns[c] {
                ColumnKind::Objective { id } => Some(id),
                _ => None,
            }))
            .collect();

        // All-strong, all-winnable, distinct values → each objective taken exactly once; total EV equal.
        assert_eq!(sol1.total_ev, sol2.total_ev, "permuting columns leaves the optimal total unchanged");
        let mut set1: Vec<u32> = ids1.into_iter().flatten().collect();
        let mut set2: Vec<u32> = ids2.into_iter().flatten().collect();
        set1.sort_unstable();
        set2.sort_unstable();
        assert_eq!(set1, set2, "the SAME objective set is assigned regardless of input order");
        assert_eq!(set1, vec![10, 20, 30], "all three distinct objectives are taken once each");
    }

    /// Rectangular handling (ADR 0032 §"The matching" — `N != K`): more squads than objectives. The
    /// surplus squad is left unmatched-or-recycled, never forced onto an infeasible cell.
    #[test]
    fn rectangular_more_rows_than_objective_columns() {
        // 3 squads, 1 objective (+ StayPut + Recycle = 3 cols). One squad wins the objective; the others
        // recycle (EV 0). No panic, no double-assignment of the objective.
        let squads = [offense_row(1000, None), offense_row(1000, None), offense_row(1000, None)];
        let objs = [undefended_obj(0, 80_000.0, 3)];
        let m = build_ev_matrix(&squads, &objs, &MatrixParams::default());
        let sol = solve_assignment(&m);
        // Exactly one row holds the objective column (col 0); column-exclusivity (no double-claim).
        let on_obj = sol.row_to_col.iter().filter(|c| **c == Some(0)).count();
        assert_eq!(on_obj, 1, "Hungarian column-exclusivity: the objective is claimed by exactly one squad");
        // Every row is assigned to SOME column (objective or recycle) — no panic on the rectangular pad.
        assert!(sol.row_to_col.iter().all(|c| c.is_some()), "every row gets a column (objective or recycle)");
    }

    /// Empty inputs are handled (no squads, or no columns) without panic.
    #[test]
    fn empty_inputs_are_safe() {
        let m = build_ev_matrix(&[], &[], &MatrixParams::default());
        let sol = solve_assignment(&m);
        assert_eq!(sol.row_to_col.len(), 0);
        assert_eq!(sol.total_ev, 0);
    }

    // ── Brute-force cross-check harness (codifies the adversarial verifier's exhaustive check as a
    //    PERMANENT guard, ADR 0032 §Determinism / §"The matching"). ──

    /// A tiny DETERMINISTIC PRNG (a 64-bit LCG) — fixed-seed so the test is bit-stable, no external crate,
    /// no `HashMap`, no unseeded `thread_rng`. Returns the next `u64`.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            // Numerical Recipes LCG constants.
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0
        }
        /// A value in `[lo, hi]` inclusive (small ranges only — the modulo bias is irrelevant for a test).
        fn in_range(&mut self, lo: i64, hi: i64) -> i64 {
            let span = (hi - lo + 1) as u64;
            lo + (self.next_u64() % span) as i64
        }
    }

    /// Build a raw row-major `i64` matrix in the PRODUCTION SHAPE directly (bypassing `value_e`/`pairing_ev`
    /// so we can inject arbitrary negative + INFEASIBLE values): `rows × (cols_obj + rows StayPut + rows
    /// Recycle@0)`. Objective + StayPut cells are random in `[-range, range]` or INFEASIBLE (with prob ~1/4);
    /// each row's own StayPut/Recycle are on its private diagonal column (others INFEASIBLE), Recycle == 0.
    /// Returns `(matrix, total_cols, cols_obj)`.
    fn random_production_matrix(rng: &mut Lcg, rows: usize, cols_obj: usize, range: i64) -> (Vec<i64>, usize, usize) {
        let total = cols_obj + 2 * rows; // objectives + per-row StayPut + per-row Recycle
        let stay_base = cols_obj;
        let recycle_base = cols_obj + rows;
        let mut m = vec![INFEASIBLE_EV; rows * total];
        for r in 0..rows {
            // Objective columns: random value, or INFEASIBLE ~1/4 of the time.
            for c in 0..cols_obj {
                m[r * total + c] = if rng.next_u64().is_multiple_of(4) { INFEASIBLE_EV } else { rng.in_range(-range, range) };
            }
            // This row's PRIVATE StayPut column (random value, or INFEASIBLE ~1/4).
            m[r * total + stay_base + r] = if rng.next_u64().is_multiple_of(4) { INFEASIBLE_EV } else { rng.in_range(-range, range) };
            // This row's PRIVATE Recycle column at 0 — the always-feasible zero-cost escape.
            m[r * total + recycle_base + r] = 0;
        }
        (m, total, cols_obj)
    }

    /// The EXHAUSTIVE optimum of a row-major matrix: try every way to assign each row to a DISTINCT column
    /// (or to "skip" — left unmatched, contributing 0), maximizing Σ of the chosen feasible cells. INFEASIBLE
    /// cells are never chosen. `O((cols+1)^rows)` — fine for the tiny shapes here. Returns `max_total_ev`.
    fn exhaustive_optimum(m: &[i64], rows: usize, cols: usize) -> i64 {
        // Recurse over rows; `used[c]` marks a claimed column. A row may also SKIP (contribute 0).
        fn rec(m: &[i64], rows: usize, cols: usize, r: usize, used: &mut [bool], acc: i64, best: &mut i64) {
            if r == rows {
                if acc > *best {
                    *best = acc;
                }
                return;
            }
            // Option 1: skip this row (left unmatched — the +0 escape the production matrix always affords
            // via Recycle@0, but exhaustive enumeration must allow it independently to find the true max).
            rec(m, rows, cols, r + 1, used, acc, best);
            // Option 2: claim each still-free, feasible column.
            for c in 0..cols {
                if used[c] {
                    continue;
                }
                let ev = m[r * cols + c];
                if ev == INFEASIBLE_EV {
                    continue;
                }
                used[c] = true;
                rec(m, rows, cols, r + 1, used, acc + ev, best);
                used[c] = false;
            }
        }
        let mut used = vec![false; cols];
        let mut best = i64::MIN;
        rec(m, rows, cols, 0, &mut used, 0, &mut best);
        best
    }

    /// Wrap a raw row-major matrix in an [`EvMatrix`] so [`solve_assignment`] can run it. The column
    /// DESCRIPTORS only need the right COUNT + the StayPut/Recycle kinds in the right places for the
    /// debug_assert + result mapping; the objective ids are synthetic.
    fn ev_matrix_from_raw(m: Vec<i64>, rows: usize, cols_obj: usize, total: usize) -> EvMatrix {
        let mut columns = Vec::with_capacity(total);
        for c in 0..cols_obj {
            columns.push(ColumnKind::Objective { id: c as u32 });
        }
        for r in 0..rows {
            columns.push(ColumnKind::StayPut { row: r });
        }
        for r in 0..rows {
            columns.push(ColumnKind::Recycle { row: r });
        }
        debug_assert_eq!(columns.len(), total);
        EvMatrix { rows, columns, ev: m }
    }

    /// THE PERMANENT CROSS-CHECK (the verifier's brute-force, codified): over MANY production-shaped
    /// matrices (R rows × C objective columns + R StayPut + R Recycle@0, with negative AND INFEASIBLE
    /// values in the objective/StayPut cells), `solve_assignment`'s `total_ev` EQUALS the EXHAUSTIVE
    /// optimum, AND no objective column is double-claimed. Deterministic (fixed-seed LCG) and fast.
    #[test]
    fn solve_matches_exhaustive_optimum_on_production_shapes() {
        let mut rng = Lcg(0x0032_0001_0002_2026); // fixed seed (ADR 0032 v1.2)
        let mut mismatches = 0u32;
        let mut samples = 0u32;
        for rows in 1..=4usize {
            for cols_obj in 1..=4usize {
                // Many random matrices per shape — plenty to surface a forced-negative-cell regression.
                for _ in 0..400 {
                    let (raw, total, cobj) = random_production_matrix(&mut rng, rows, cols_obj, 1_000);
                    let opt = exhaustive_optimum(&raw, rows, total);
                    let m = ev_matrix_from_raw(raw, rows, cobj, total);
                    let sol = solve_assignment(&m);

                    if sol.total_ev != opt {
                        mismatches += 1;
                    }

                    // No OBJECTIVE column is claimed by two rows (Hungarian column-exclusivity).
                    let mut obj_claims = vec![0u32; cols_obj];
                    for c in sol.row_to_col.iter().flatten() {
                        if let ColumnKind::Objective { id } = m.columns[*c] {
                            obj_claims[id as usize] += 1;
                        }
                    }
                    assert!(
                        obj_claims.iter().all(|&n| n <= 1),
                        "an objective column was double-claimed: {obj_claims:?} assign={:?}",
                        sol.row_to_col
                    );
                    samples += 1;
                }
            }
        }
        assert_eq!(mismatches, 0, "{mismatches}/{samples} production-shaped matrices disagreed with the exhaustive optimum");
        assert!(samples >= 6_000, "expected >= 6000 samples, ran {samples}");
    }

    /// PINS WHY THE PER-ROW ESCAPE MATTERS (Finding 1): a perfect-matching solver on a matrix WITHOUT the
    /// per-row Recycle@0 escapes can FORCE a row onto a negative cell — the optimality-for-the-row guarantee
    /// does NOT hold without the escape. Here `cols == rows` (a square, escape-less matrix) and every cell is
    /// negative, so the perfect matching MUST pick negative cells (total < 0); the exhaustive "may-skip"
    /// optimum is 0 (skip every row). They DIFFER — which is exactly why `build_ev_matrix` appends the escape.
    #[test]
    fn without_per_row_escape_a_row_is_forced_onto_a_negative_cell() {
        // 2×2, all cells strictly negative, no StayPut/Recycle columns (cols_obj == rows, no escape).
        let rows = 2usize;
        let cols = 2usize;
        let raw = vec![-10, -20, -30, -40]; // row-major: row0=[-10,-20], row1=[-30,-40]
        // Build an EvMatrix with ONLY objective columns (escape-less). NOTE: this deliberately violates the
        // build_ev_matrix contract — we construct it by hand to demonstrate the consequence, and we do NOT
        // route it through solve_assignment's debug_assert path expecting None (every row IS feasible here,
        // just negative, so the guard does not trip — the point is the FORCED negative, not infeasibility).
        let columns = vec![ColumnKind::Objective { id: 0 }, ColumnKind::Objective { id: 1 }];
        let m = EvMatrix { rows, columns, ev: raw.clone() };
        let sol = solve_assignment(&m);

        // The perfect matching forces both rows onto a negative cell: the optimum perfect matching of a
        // 2×2 all-negative matrix is min(|−10|+|−40|, |−20|+|−30|) in magnitude ⇒ −(10+40) = −50.
        assert_eq!(sol.total_ev, -50, "escape-less perfect matching is forced negative");
        // Whereas the may-skip exhaustive optimum (what we WANT, and what the per-row escape buys) is 0.
        let want = exhaustive_optimum(&raw, rows, cols);
        assert_eq!(want, 0, "the may-skip optimum is to take NOTHING (0) — strictly better than −50");
        assert!(sol.total_ev < want, "WITHOUT a per-row escape the solver is worse than optimal-with-skip ({} < {})", sol.total_ev, want);
    }

    // ══ ADR 0032 v2 / ADR 0027 — the MERGE column class (the pending-slot Lanchester transfer) ══════════

    /// A DEFENDED objective worth `value` energy-equiv with `tower_dps` incoming + `required` hits to kill,
    /// no travel for `n_rows`. Defended ⇒ P(win) is a smooth function of caps (NOT the undefended binary), so
    /// adding a donor's sheddable caps produces a real MARGINAL LIFT.
    fn defended_obj(id: u32, value: f32, tower_range: u32, required: u32, n_rows: usize) -> ObjectiveCell {
        use crate::force_sizing::TowerThreat;
        ObjectiveCell {
            id,
            class: CapClass::Offense,
            value_kind: ObjectiveValueKind::Denial,
            intel: ObjectiveIntel { denial_value: value * 2.0, ..Default::default() },
            defense: DefenseProfile {
                objective_hits: required,
                // A fully-energized tower at `tower_range` (energy well above TOWER_ENERGY_COST so it fires).
                towers: vec![TowerThreat { range_to_assault: tower_range, energy: 1000 }],
                ..Default::default()
            },
            enemy: None,
            travel_rooms_per_row: vec![0; n_rows],
            feasible_per_row: vec![true; n_rows],
        }
    }

    /// THE HEADLINE MERGE PROOF (ADR 0032 v2 §Sim, ADR 0027 lines 256-312): the matrix picks `Merge→Bk`
    /// for a donor over a marginal SOLO reassign because the receiver's marginal P(win) LIFT outvalues the
    /// donor's own best solo objective — AND the DILUTIVE-split column is ABSENT (cannot be selected), which
    /// is the whole pending-slot guard.
    #[test]
    fn merge_is_picked_over_a_marginal_solo_reassign_and_the_dilutive_split_is_absent() {
        // Receiver B (row 0): FORMING for a high-value DEFENDED objective (id 0), under-DPS'd ALONE so its
        // P(win) is well below 1 — there is real lift to be had. It has an OPEN Dismantler pending slot.
        let mut receiver = offense_row(200, Some(0));
        receiver.open_slot_roles = role_bit(SquadRole::Dismantler); // an open dismantler pending slot
        // Donor S (row 1): a terminal-with-survivors squad whose sheddable member is a heavy Dismantler. Its
        // OWN best solo objective is a small-value one (id 1). Merge-eligible; sheds a Dismantler.
        let mut donor = offense_row(50, None);
        donor.merge_eligible = true;
        donor.sheddable = caps(800, 0); // a big structure-DPS dismantler — fills B's missing kill power
        donor.sheddable_roles = role_bit(SquadRole::Dismantler);

        // B's objective: HIGH value, defended (so caps matter). Donor's own objective: LOW value.
        let obj_b = defended_obj(0, 200_000.0, 20, 400_000, 2);
        let obj_low = undefended_obj(1, 50.0, 2); // donor's marginal solo option

        let squads = [receiver, donor];
        let objs = [obj_b, obj_low];
        let m = build_ev_matrix_with_merge(&squads, &objs, &[], &MatrixParams::default());

        // The Merge column for receiver_row 0 exists; NO Merge column for the donor (no open slot) — and
        // crucially there is NO column anywhere that represents a DILUTIVE split (peeling into a new under-
        // strength squad). The ONLY merge column is `Merge { receiver_row: 0 }`.
        let merge_cols: Vec<usize> = m
            .columns
            .iter()
            .enumerate()
            .filter_map(|(c, col)| matches!(col, ColumnKind::Merge { .. }).then_some(c))
            .collect();
        assert_eq!(merge_cols.len(), 1, "exactly ONE merge column (the one forming receiver with an open slot)");
        assert_eq!(m.columns[merge_cols[0]], ColumnKind::Merge { receiver_row: 0 }, "the merge column targets B");
        // The DILUTIVE split is never representable: the donor (row 1) is never a RECEIVER (open_slot_roles=0),
        // so no `Merge { receiver_row: 1 }` exists. (ADR 0027 lines 308-312 — the guard IS the absence.)
        assert!(
            !m.columns.iter().any(|c| matches!(c, ColumnKind::Merge { receiver_row: 1 })),
            "a dilutive split (donor as its own new under-strength receiver) is NOT a column"
        );

        // The donor's merge cell EV must be POSITIVE (a real lift) and beat its own best solo objective cell.
        let merge_ev = m.at(1, merge_cols[0]);
        let donor_solo_low = m.at(1, 1); // donor on the low objective
        assert!(merge_ev > 0, "the marginal-lift merge EV is positive, got {merge_ev}");
        assert!(merge_ev > donor_solo_low, "merge ({merge_ev}) must beat the donor's marginal solo reassign ({donor_solo_low})");

        // The SELF-merge cell (donor merging into itself) is INFEASIBLE — guarded explicitly.
        // (receiver row 0 in its own merge column.)
        assert_eq!(m.at(0, merge_cols[0]), INFEASIBLE_EV, "the receiver cannot self-merge (no self-merge guard)");

        // The global solve assigns the donor (row 1) to the merge column (its best feasible move).
        let sol = solve_assignment(&m);
        assert_eq!(sol.row_to_col[1], Some(merge_cols[0]), "the donor is matched to Merge→B, not the marginal solo objective");
    }

    /// FORMING-CONSOLIDATION (ADR 0027 lines 270-271 — "two squads stuck at 1/4 each can MERGE into one at
    /// 2/4"): two forming squads each at partial strength; the optimum consolidates the donor's member into
    /// the receiver's open slot (a merge) rather than both churning. Asserts the merge column is feasible +
    /// positive and chosen.
    #[test]
    fn two_forming_squads_consolidate_via_a_merge() {
        // Receiver (row 0): forming for a DEFENDED objective, has an open RangedDPS slot, under-DPS alone.
        let mut receiver = offense_row(150, Some(0));
        receiver.open_slot_roles = role_bit(SquadRole::RangedDPS);
        // Donor (row 1): ALSO forming (consolidating), merge-eligible, sheds a RangedDPS that lifts B.
        let mut donor = offense_row(150, Some(1));
        donor.merge_eligible = true;
        donor.sheddable = caps(500, 30);
        donor.sheddable_roles = role_bit(SquadRole::RangedDPS);

        let obj_b = defended_obj(0, 150_000.0, 20, 300_000, 2);
        // The donor's own objective is the SAME class but lower value + also defended (it is ALSO struggling).
        let obj_d = defended_obj(1, 40_000.0, 20, 300_000, 2);

        let squads = [receiver, donor];
        let objs = [obj_b, obj_d];
        let m = build_ev_matrix_with_merge(&squads, &objs, &[], &MatrixParams::default());

        let merge_col = m.columns.iter().position(|c| *c == ColumnKind::Merge { receiver_row: 0 }).expect("B has a merge column");
        let merge_ev = m.at(1, merge_col);
        let donor_stay = m.at(1, /*stay_base=*/ objs.len() + /*row*/ 1); // donor staying on its own struggling fight
        assert!(merge_ev > 0, "consolidation lift is positive, got {merge_ev}");
        let sol = solve_assignment(&m);
        assert_eq!(
            sol.row_to_col[1],
            Some(merge_col),
            "the donor consolidates into B (merge {merge_ev}) instead of churning on its own fight (stay {donor_stay})"
        );
    }

    /// THE PENDING-SLOT GUARD — a ROLE MISMATCH makes the merge INFEASIBLE (ADR 0027 line 258: a creep may
    /// transfer ONLY to fill a PENDING SPAWN SLOT of a COMPATIBLE ROLE). The donor sheds a Healer but B's
    /// only open slot is a Dismantler ⇒ the merge cell is INFEASIBLE_EV (never chosen).
    #[test]
    fn merge_is_infeasible_on_a_role_mismatch() {
        let mut receiver = offense_row(200, Some(0));
        receiver.open_slot_roles = role_bit(SquadRole::Dismantler); // open Dismantler slot
        let mut donor = offense_row(50, None);
        donor.merge_eligible = true;
        donor.sheddable = caps(0, 600);
        donor.sheddable_roles = role_bit(SquadRole::Healer); // sheds a HEALER — wrong role

        let obj_b = defended_obj(0, 200_000.0, 20, 400_000, 2);
        let squads = [receiver, donor];
        let objs = [obj_b];
        let m = build_ev_matrix_with_merge(&squads, &objs, &[], &MatrixParams::default());
        let merge_col = m.columns.iter().position(|c| *c == ColumnKind::Merge { receiver_row: 0 }).expect("merge col exists");
        assert_eq!(m.at(1, merge_col), INFEASIBLE_EV, "a role-mismatched merge is INFEASIBLE (the pending-slot role guard)");
    }

    /// A NON-merge-eligible donor (mid-fight — not terminal, not over-rostered, not forming-consolidate) is
    /// NEVER a merge donor: every merge cell with it as donor is INFEASIBLE (ADR 0027 line 273 — "donor sheds,
    /// never weakens mid-fight").
    #[test]
    fn ineligible_donor_never_merges() {
        let mut receiver = offense_row(200, Some(0));
        receiver.open_slot_roles = role_bit(SquadRole::Dismantler);
        let mut donor = offense_row(800, Some(1)); // a strong squad on its OWN fight — merge_eligible=false (default)
        donor.sheddable = caps(800, 0);
        donor.sheddable_roles = role_bit(SquadRole::Dismantler);
        // merge_eligible left false (Default) — the guard.

        let obj_b = defended_obj(0, 200_000.0, 20, 400_000, 2);
        let obj_d = undefended_obj(1, 90_000.0, 2);
        let squads = [receiver, donor];
        let objs = [obj_b, obj_d];
        let m = build_ev_matrix_with_merge(&squads, &objs, &[], &MatrixParams::default());
        let merge_col = m.columns.iter().position(|c| *c == ColumnKind::Merge { receiver_row: 0 }).expect("merge col exists");
        assert_eq!(m.at(1, merge_col), INFEASIBLE_EV, "an ineligible (mid-fight) donor never merges");
    }

    /// DETERMINISM (ADR 0032 §Determinism): the merge matrix + solve is byte-identical run twice, and
    /// PERMUTING the donor/receiver row order yields the SAME donor→receiver match (the role-matched,
    /// stable-order donor→slot match of ADR 0027 line 282).
    #[test]
    fn merge_is_deterministic_and_permutation_invariant() {
        let mut receiver = offense_row(200, Some(0));
        receiver.open_slot_roles = role_bit(SquadRole::Dismantler);
        let mut donor = offense_row(50, Some(1));
        donor.merge_eligible = true;
        donor.sheddable = caps(800, 0);
        donor.sheddable_roles = role_bit(SquadRole::Dismantler);
        let obj_b = defended_obj(0, 200_000.0, 20, 400_000, 2);
        let obj_d = undefended_obj(1, 30.0, 2);

        // Run twice — byte-identical.
        let m1 = build_ev_matrix_with_merge(&[receiver, donor], &[obj_b.clone(), obj_d.clone()], &[], &MatrixParams::default());
        let a = solve_assignment(&m1);
        let b = solve_assignment(&m1);
        assert_eq!(a, b, "the merge solve is deterministic run twice");

        // Permute the rows: donor first, receiver second. The donor must STILL merge into the receiver (now
        // row 1's merge column targets the receiver's NEW row index). We assert the donor (whichever row) is
        // matched to the receiver's merge column — order-independent.
        let m2 = build_ev_matrix_with_merge(&[donor, receiver], &[obj_b, obj_d], &[], &MatrixParams::default());
        let sol2 = solve_assignment(&m2);
        // In m2, the receiver is row 1 ⇒ its merge column is `Merge { receiver_row: 1 }`; the donor is row 0.
        let recv_merge_col = m2.columns.iter().position(|c| *c == ColumnKind::Merge { receiver_row: 1 }).expect("merge col");
        assert_eq!(sol2.row_to_col[0], Some(recv_merge_col), "permuting rows keeps the donor→receiver merge match");
    }

    /// The v2 builder PRESERVES the v1.2 matrix when no receiver has an open slot (no Merge column appended) —
    /// the empty-merge path is byte-identical to `build_ev_matrix` (the delegation invariant).
    #[test]
    fn no_open_slot_means_no_merge_column_v1_2_preserved() {
        let squads = [offense_row(1000, None), offense_row(500, None)];
        let objs = [undefended_obj(0, 90_000.0, 2), undefended_obj(1, 60_000.0, 2)];
        let v1_2 = build_ev_matrix(&squads, &objs, &MatrixParams::default());
        let v2 = build_ev_matrix_with_merge(&squads, &objs, &[], &MatrixParams::default());
        assert_eq!(v1_2.columns, v2.columns, "no open slots ⇒ no merge columns ⇒ identical column layout");
        assert!(!v2.columns.iter().any(|c| matches!(c, ColumnKind::Merge { .. })), "no merge column without an open slot");
        assert_eq!(solve_assignment(&v1_2), solve_assignment(&v2), "identical solve");
    }
}
