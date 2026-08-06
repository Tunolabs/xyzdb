// SPDX-License-Identifier: BUSL-1.1
use crate::lid::LID;
use crate::record::Record;
use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Result of a query execution. Shared between engine, server, and clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueryResult {
    /// Single mutation succeeded (PUT/SET/DELETE/LINK/ANCHOR/LOBE).
    Ok { lid: Option<LID>, message: String },
    /// Batch mutation succeeded.
    BatchOk {
        count: usize,
        first_lid: LID,
        last_lid: LID,
    },
    /// FIND/PULL/SCAN: records found.
    Records(Vec<Record>),
    /// AGGREGATE: computed values.
    Aggregation(BTreeMap<String, Value>),
    /// SHOW: metadata info lines.
    Info(Vec<String>),
    /// V4: GROUP BY + AGGREGATE: each entry has group key fields + aggregate results.
    GroupedAggregation(Vec<BTreeMap<String, Value>>),
    /// v0.2.5.1: paginated SCAN result. Returned when the caller passed
    /// `CURSOR "<token>"` OR when the engine's default LIMIT capped a
    /// result set and more records remain. Plain `Records` continues to
    /// be returned when a SCAN fits completely under the active LIMIT —
    /// existing clients that never paginate see no shape change.
    ///
    /// Bincode wire form: variant index follows the existing six variants.
    /// Old clients decoding a `PaginatedRecords` frame fail explicitly;
    /// `Records` frames remain byte-identical.
    PaginatedRecords {
        records: Vec<Record>,
        /// Opaque token for the next page. `None` when `has_more` is false.
        cursor: Option<String>,
        /// `true` when the engine detected at least one record beyond the
        /// returned page.
        has_more: bool,
        /// M2.3 budget-stop counters — `Some` ONLY on the NEAREST hydration path
        /// that hit the `--nearest-budget-ms` airbag (`has_more = true` for that
        /// reason); `None` on every other `PaginatedRecords` (cursor pages, SCAN
        /// caps), so those frames stay byte-identical and existing clients that
        /// key off `has_more` (e.g. devva `clean_page`) are untouched. See
        /// [`BudgetStop`]. Grouped-optional BY DESIGN — do not flatten to three
        /// top-level fields.
        budget_stop: Option<BudgetStop>,
    },
}

/// Turns the truncation *inference* (`has_more`) into a *fact*: "examined 238k
/// of 246k candidates, found 6" is almost certainty; "there may be more" is not.
/// Emitted ONLY when a NEAREST's bounded hydration tail was cut by the latency
/// airbag — a rare event, rarer still once sub-gravity lands.
///
/// GROUPED into one optional field ON PURPOSE, not three top-level fields on
/// `PaginatedRecords`. The signal fires almost never, so it must not levy the
/// wire-and-maintenance cost of three fields threaded through every one of the
/// four `PaginatedRecords` construction sites. The form was chosen from that
/// asymmetry (rare firing vs wire cost), not from completeness. If a future
/// change proposes "flatten to three first-level fields, it's cleaner": the
/// grouped `Option` is deliberate — flattening pays the cost everywhere for a
/// signal that appears almost nowhere.
///
/// Read the three counts as **"scored `candidates`, checked the residual filter
/// on `examined` of them, `found` passed"**. `candidates` is the SCORED set, not
/// filter matches — in the selective case almost none of them pass, and that is
/// the whole reason the airbag fires. Two derived quantities make the trio
/// useful: `examined / candidates` is the fraction of the scored universe
/// actually checked, and `found / examined` is the filter's observed pass rate
/// over that checked portion — so a client seeing 6 passers in 238k checked can
/// estimate a fraction of a row in the 8k unchecked and turn "there may be more"
/// into "almost certainly not", without the engine asserting anything it did not
/// measure.
///
/// It describes the CUT, not the set. `examined` is how much was checked, not how
/// much exists; there is no cursor (the scoring pass is not resumable), so
/// `candidates - examined` is NOT "the rest, ask for it" — it is unchecked, not
/// pending. The only actions are raise `--nearest-budget-ms`, narrow the scope,
/// or accept the partial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetStop {
    /// Candidates whose residual filter was checked (hydrated) before the cut —
    /// the numerator of the fraction-checked. Counts passers AND non-passers.
    pub examined: usize,
    /// The whole SCORED set in score order (the bucket), before the residual.
    /// NOT the number of filter matches.
    pub candidates: usize,
    /// Passers returned. Redundant with `records.len()` BY CONSTRUCTION (same
    /// value today); kept because it is explicit and cheap. If a later cap ever
    /// trims `records` after this is set, `records.len()` is authoritative.
    pub found: usize,
    /// Which iteration order produced this partial — and therefore WHAT KIND of
    /// partial it is. `"score_order"`: candidates were walked best-first, so the
    /// rows are a true PREFIX of the answer and nothing unexamined scores better.
    /// `"key_order"`: candidates were walked in key order for sequential I/O, so
    /// the rows are the best of a contiguous KEY REGION and the unwalked part may
    /// hold BETTER ones, not merely more.
    ///
    /// It names the FACT, not the implementation: the consumer needs to know
    /// whether it holds a prefix or a sample, and that stays the right question
    /// even if a third traversal appears — so it is not `"A"`/`"B"`.
    ///
    /// This field earns its place on a struct chosen to be minimal (see the type
    /// doc) for ONE reason: without it, a derived reading this engine already
    /// documents — extrapolating the observed pass rate to the unexamined tail to
    /// conclude "almost certainly no more" — is FALSE under `key_order`, where the
    /// unwalked region can hold better rows. Not completeness; keeping an existing
    /// claim true.
    pub strategy: ScanStrategy,
}

/// How the candidates were walked to produce a truncated partial — i.e. what KIND
/// of partial the caller is holding.
///
/// Serialises as `"score_order"` / `"key_order"`. An enum rather than a bare
/// string so an invalid value cannot be constructed, while the wire form stays the
/// plain word a consumer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStrategy {
    /// Walked best-first. The rows returned are a true PREFIX of the answer:
    /// nothing left unexamined scores better than what came back.
    ScoreOrder,
    /// Walked in key order, for sequential I/O. The rows are the best of a
    /// contiguous KEY REGION, so the unwalked part may hold BETTER rows — not
    /// merely more of them.
    KeyOrder,
}

impl QueryResult {
    /// Extract records from a Records (or PaginatedRecords) result, or
    /// empty vec for other variants. Pagination metadata is dropped.
    pub fn into_records(self) -> Vec<Record> {
        match self {
            QueryResult::Records(recs) => recs,
            QueryResult::PaginatedRecords { records, .. } => records,
            _ => vec![],
        }
    }
}
