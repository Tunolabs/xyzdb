//! Persona profiles per the v0.3.3 bench design
//! extension (v0.5 — multi-persona heterogeneous workload).
//!
//! Each Phase 3 thread is assigned a single `Persona` at startup. The
//! persona dictates:
//!   - Read / Write query weights (which Q1-Q9 fire and how often).
//!   - Read fraction per MMPP state (`reads_idle`, `reads_busy`).
//!   - Per-persona overrides for the MMPP arrival rates and session
//!     lifecycle durations (intensity / cadence varies by user type).
//!
//! Captures the heterogeneity of real fintech ERP traffic: a teller's
//! UI bursts are nothing like a batch processor's nightly ingest, and
//! neither resembles an auditor's monthly heavy-history pull.
//!
//! The 4 personas committed in v0.5:
//!   * `FrontOffice`   — teller / cajero. High Q1+Q9, occasional Q7.
//!   * `BatchProcessor`— cron EOD payment ingest. Q7-dominated, sparse.
//!   * `AnalyticsDash` — dashboard manager. Q4+Q5+Q8 heavy, long sessions.
//!   * `Regulatorio`   — auditor / report generator. Q3 LIMIT-heavy, rare.
//!
//! Default thread assignment for a 9-thread Phase 3: 4 FrontOffice +
//! 2 BatchProcessor + 2 AnalyticsDash + 1 Regulatorio. Configurable via
//! the orchestrator `--personas` flag.

use crate::bench::BusinessQuery;
use serde::{Deserialize, Serialize};

/// One of four fintech ERP user types Phase 3 simulates.
///
/// Distinct from the MMPP state (`Idle`/`Busy`) and the session
/// lifecycle (`Connecting`/`Active`/...): persona is a **static** per-
/// thread attribute assigned at startup, while MMPP+session evolve
/// dynamically. The same `Persona` can be in any session/MMPP state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Persona {
    /// Teller / cajero. Front-of-store UI bursts: open client, check
    /// recent payments, capture transactional notes. Q1 lookup +
    /// Q9 customer context dominate; Q7 small batch on commit.
    FrontOffice,
    /// EOD payment-ingest cron. Few reads (sanity Q3), heavy Q7 batch
    /// ingest during the active window. Sparse outside the activity
    /// burst (mostly InterSessionPause).
    BatchProcessor,
    /// Dashboard / analytics manager. Q4 top-exposure, Q5 overdue
    /// summary, Q8 monthly close — heavy multi-step queries with long
    /// drill-down sessions.
    AnalyticsDash,
    /// Auditor / regulatory reporter. Q3 full-history pulls with high
    /// `LIMIT`, occasional Q5/Q8 cross-checks. Very sparse but each
    /// query is large.
    Regulatorio,
    /// Generic synthetic human user. Covers all Q1-Q9 (including Q4)
    /// with realistic fintech-ERP weights. Not specialised — captures
    /// "unpredictable human" baseline. Useful for cross-engine
    /// symmetric matrices where heterogeneity would bias the
    /// comparison, and for full-Q1-Q9 coverage in Phase 3 without
    /// pinning specific queries to specific persona slots.
    RandomHuman,
}

impl Persona {
    /// Stable string id for CLI parsing and log lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            Persona::FrontOffice => "front",
            Persona::BatchProcessor => "batch",
            Persona::AnalyticsDash => "analytics",
            Persona::Regulatorio => "regulatorio",
            Persona::RandomHuman => "humanrandom",
        }
    }

    /// Inverse of `as_str` for parsing the `--personas` flag.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "front" => Some(Persona::FrontOffice),
            "batch" => Some(Persona::BatchProcessor),
            "analytics" => Some(Persona::AnalyticsDash),
            "regulatorio" => Some(Persona::Regulatorio),
            "humanrandom" | "human" => Some(Persona::RandomHuman),
            _ => None,
        }
    }
}

/// Per-persona configuration overrides on top of `ErraticaConfig`
/// defaults. Each persona shapes its workload via weighted query mixes
/// + per-state read fractions + MMPP/session multipliers.
#[derive(Debug, Clone)]
pub struct PersonaConfig {
    pub persona: Persona,
    /// Weighted read query mix (sums to 1.0). Q4 is excluded from the Phase 3
    /// mix (still measured in Phase 2 cold). (Q10 was removed from the bench.)
    pub read_mix: &'static [(BusinessQuery, f64)],
    /// Weighted write query mix (sums to 1.0).
    pub write_mix: &'static [(BusinessQuery, f64)],
    /// P(read) when MMPP state is Idle. Overrides
    /// `ErraticaConfig::reads_idle` for this persona.
    pub reads_idle: f64,
    /// P(read) when MMPP state is Busy.
    pub reads_busy: f64,
    /// Multiplier on `lambda_idle`. 1.0 == no change.
    pub lambda_idle_mult: f64,
    /// Multiplier on `lambda_busy`. 1.0 == no change. Captures persona
    /// intensity: batch ~2x, regulatorio ~0.3x.
    pub lambda_busy_mult: f64,
    /// Mean Active session duration in seconds (LogNormal median).
    /// Overrides `session_active_mean_secs`.
    pub session_active_secs: f64,
    /// Mean InterSession pause in seconds. Overrides
    /// `inter_session_mu_ln_secs`.
    pub pause_secs: f64,
}

// ── Static persona profiles ─────────────────────────────────────────────────

/// Front-office teller. Q1 dominates (lookup), Q9 follows (context
/// pull on opening a client), Q6 for payment refresh, Q2 occasional
/// aggregate. Writes: Q7 small batch on commit.
const FRONT_READ_MIX: &[(BusinessQuery, f64)] = &[
    (BusinessQuery::Q1Point, 0.45),
    (BusinessQuery::Q9CustomerContext, 0.30),
    (BusinessQuery::Q6RecentPayments, 0.15),
    (BusinessQuery::Q2Aggregate, 0.10),
];
const FRONT_WRITE_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q7BatchIngest, 1.0)];

/// Batch processor cron. Rare reads (Q3 sanity), Q7 heavy when active.
const BATCH_READ_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q3FullHistory, 1.0)];
const BATCH_WRITE_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q7BatchIngest, 1.0)];

/// Analytics dashboard. Q4/Q5/Q8 weighted equally, Q2 occasional.
const ANALYTICS_READ_MIX: &[(BusinessQuery, f64)] = &[
    (BusinessQuery::Q4TopExposure, 0.30),
    (BusinessQuery::Q5OverdueByEmpresa, 0.30),
    (BusinessQuery::Q8MonthlyClose, 0.30),
    (BusinessQuery::Q2Aggregate, 0.10),
];
const ANALYTICS_WRITE_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q7BatchIngest, 1.0)];

/// Regulatorio auditor. Heavy Q3 (historiales completos), Q5/Q8 light.
const REG_READ_MIX: &[(BusinessQuery, f64)] = &[
    (BusinessQuery::Q3FullHistory, 0.70),
    (BusinessQuery::Q5OverdueByEmpresa, 0.20),
    (BusinessQuery::Q8MonthlyClose, 0.10),
];
const REG_WRITE_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q7BatchIngest, 1.0)];

/// Generic synthetic human. Full coverage of Q1-Q9 (Q10 removed from the
/// bench). Weights derive from the spec §6.4 canonical
/// distribution **plus** an explicit Q4 slot (the canonical mix had
/// Q4=0 because it was a Phase 2 cold-only query under the uniform
/// model; the persona model surfaces it under sustained load too).
/// Sum = 1.00.
const HUMAN_READ_MIX: &[(BusinessQuery, f64)] = &[
    (BusinessQuery::Q1Point, 0.20),
    (BusinessQuery::Q2Aggregate, 0.13),
    (BusinessQuery::Q3FullHistory, 0.10),
    (BusinessQuery::Q4TopExposure, 0.05),
    (BusinessQuery::Q5OverdueByEmpresa, 0.12),
    (BusinessQuery::Q6RecentPayments, 0.12),
    (BusinessQuery::Q8MonthlyClose, 0.08),
    (BusinessQuery::Q9CustomerContext, 0.20),
];
const HUMAN_WRITE_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q7BatchIngest, 1.0)];

impl PersonaConfig {
    /// Static default profile for the given persona. Captures the
    /// canonical fintech ERP characterisation. Tunable via env vars
    /// (future work — for v0.5 the defaults are committed values).
    pub fn for_persona(p: Persona) -> Self {
        match p {
            Persona::FrontOffice => Self {
                persona: p,
                read_mix: FRONT_READ_MIX,
                write_mix: FRONT_WRITE_MIX,
                // Teller bursts include simultaneous reads (context
                // pull) + occasional writes (commit). Slightly
                // higher write share than the global default in Busy.
                reads_idle: 0.95,
                reads_busy: 0.70,
                lambda_idle_mult: 1.0,
                lambda_busy_mult: 1.0,
                session_active_secs: 200.0,
                pause_secs: 600.0,
            },
            Persona::BatchProcessor => Self {
                persona: p,
                read_mix: BATCH_READ_MIX,
                write_mix: BATCH_WRITE_MIX,
                // Batch is write-dominated even in Idle (it wakes up
                // to ingest, then sleeps).
                reads_idle: 0.20,
                reads_busy: 0.10,
                // Idle is genuinely idle (sleeping cron), Busy is 2x
                // more intense than baseline.
                lambda_idle_mult: 0.2,
                lambda_busy_mult: 2.0,
                // Active window is long (full batch ingest takes
                // minutes), pause between runs is very long.
                session_active_secs: 600.0,
                pause_secs: 1800.0,
            },
            Persona::AnalyticsDash => Self {
                persona: p,
                read_mix: ANALYTICS_READ_MIX,
                write_mix: ANALYTICS_WRITE_MIX,
                // Almost pure read — analytics rarely writes, only
                // occasional Q7-like cache refresh stand-in.
                reads_idle: 0.98,
                reads_busy: 0.95,
                // Steady moderate intensity, fewer bursts.
                lambda_idle_mult: 0.6,
                lambda_busy_mult: 0.5,
                // Long drill-down sessions, moderate pause.
                session_active_secs: 900.0,
                pause_secs: 1200.0,
            },
            Persona::Regulatorio => Self {
                persona: p,
                read_mix: REG_READ_MIX,
                write_mix: REG_WRITE_MIX,
                // Read-only effectively.
                reads_idle: 0.99,
                reads_busy: 0.99,
                // Very rare activity but when active each query is
                // heavy.
                lambda_idle_mult: 0.1,
                lambda_busy_mult: 0.3,
                // Short sessions (audit pull and leave), very long
                // pause (rare events: monthly/quarterly).
                session_active_secs: 300.0,
                pause_secs: 3600.0,
            },
            Persona::RandomHuman => Self {
                persona: p,
                read_mix: HUMAN_READ_MIX,
                write_mix: HUMAN_WRITE_MIX,
                // Canonical fintech read/write ratio per spec §6.4.
                reads_idle: 0.95,
                reads_busy: 0.70,
                // Average intensity — neither sparse like batch nor
                // heavy like analytics.
                lambda_idle_mult: 1.0,
                lambda_busy_mult: 1.0,
                // Mid-range session lifecycle: 5min active, 10min
                // pause. Captures "generic ERP user" turnover.
                session_active_secs: 300.0,
                pause_secs: 600.0,
            },
        }
    }
}

/// Persona assignment for a thread pool, parsed from the orchestrator
/// `--personas front=N,batch=M,analytics=K,regulatorio=L` flag.
///
/// Default `4-2-2-1` sums to 9 threads, matches the v0.3.3 envelope.
/// Custom mixes that sum to less than `total_threads` pad with `Idle`
/// (no queries emitted from those slots); sums greater than
/// `total_threads` are rejected at parse time.
#[derive(Debug, Clone)]
pub struct PersonaAssignment {
    /// Per-thread persona assignment, indexed by `thread_id`. `None`
    /// means the slot is idle (padding).
    slots: Vec<Option<Persona>>,
}

impl PersonaAssignment {
    /// Default assignment for `total_threads`: 4 front + 2 batch +
    /// 2 analytics + 1 regulatorio when `total_threads == 9`. Other
    /// counts proportionally rescale. RandomHuman is NOT in the
    /// default — opt-in via the `--personas humanrandom=N` flag.
    pub fn default_for(total_threads: usize) -> Self {
        // For non-9 counts, scale roughly proportional to 4-2-2-1.
        // Round down; pad with FrontOffice if any remainder.
        let n_front = ((total_threads * 4 + 4) / 9).max(1);
        let n_batch = (total_threads * 2 / 9).max(0);
        let n_analytics = (total_threads * 2 / 9).max(0);
        let assigned = n_front + n_batch + n_analytics;
        let n_reg = if total_threads > assigned {
            total_threads - assigned
        } else {
            0
        };
        Self::from_counts(total_threads, n_front, n_batch, n_analytics, n_reg, 0)
    }

    /// Build from explicit counts per persona. Returns `Err` if the
    /// sum exceeds `total_threads`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_counts(
        total_threads: usize,
        front: usize,
        batch: usize,
        analytics: usize,
        regulatorio: usize,
        humanrandom: usize,
    ) -> Self {
        let sum = front + batch + analytics + regulatorio + humanrandom;
        let mut slots = Vec::with_capacity(total_threads);
        for _ in 0..front {
            slots.push(Some(Persona::FrontOffice));
        }
        for _ in 0..batch {
            slots.push(Some(Persona::BatchProcessor));
        }
        for _ in 0..analytics {
            slots.push(Some(Persona::AnalyticsDash));
        }
        for _ in 0..regulatorio {
            slots.push(Some(Persona::Regulatorio));
        }
        for _ in 0..humanrandom {
            slots.push(Some(Persona::RandomHuman));
        }
        // Pad idle if sum < total_threads.
        for _ in sum..total_threads {
            slots.push(None);
        }
        // Truncate if oversized (caller responsibility to validate).
        slots.truncate(total_threads);
        Self { slots }
    }

    /// Parse from a flag string, e.g.
    /// `"front=4,batch=2,analytics=2,regulatorio=1"` (heterogeneous
    /// fintech default) or `"humanrandom=9"` (symmetric baseline).
    /// Missing keys default to 0. Unknown keys are rejected.
    pub fn parse(input: &str, total_threads: usize) -> Result<Self, String> {
        let mut front = 0;
        let mut batch = 0;
        let mut analytics = 0;
        let mut regulatorio = 0;
        let mut humanrandom = 0;
        for pair in input.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, val) = pair
                .split_once('=')
                .ok_or_else(|| format!("invalid persona spec '{pair}', expected key=N"))?;
            let n: usize = val
                .trim()
                .parse()
                .map_err(|_| format!("invalid count '{val}' for persona '{key}'"))?;
            match key.trim() {
                "front" => front = n,
                "batch" => batch = n,
                "analytics" => analytics = n,
                "regulatorio" => regulatorio = n,
                "humanrandom" | "human" => humanrandom = n,
                _ => return Err(format!("unknown persona '{key}'")),
            }
        }
        let sum = front + batch + analytics + regulatorio + humanrandom;
        if sum > total_threads {
            return Err(format!(
                "persona counts sum to {sum} but only {total_threads} threads available"
            ));
        }
        Ok(Self::from_counts(
            total_threads,
            front,
            batch,
            analytics,
            regulatorio,
            humanrandom,
        ))
    }

    /// Persona for the given thread id, or `None` if the slot is idle.
    pub fn persona_for(&self, thread_id: usize) -> Option<Persona> {
        self.slots.get(thread_id).copied().flatten()
    }

    /// Total slot count, including idle padding.
    pub fn total_slots(&self) -> usize {
        self.slots.len()
    }

    /// Iterator over `(thread_id, Option<Persona>)`.
    pub fn iter(&self) -> impl Iterator<Item = (usize, Option<Persona>)> + '_ {
        self.slots.iter().enumerate().map(|(i, p)| (i, *p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_str_roundtrips() {
        for p in [
            Persona::FrontOffice,
            Persona::BatchProcessor,
            Persona::AnalyticsDash,
            Persona::Regulatorio,
            Persona::RandomHuman,
        ] {
            assert_eq!(Persona::from_str(p.as_str()), Some(p));
        }
        // Both spellings accepted for RandomHuman.
        assert_eq!(Persona::from_str("human"), Some(Persona::RandomHuman));
        assert_eq!(Persona::from_str("unknown"), None);
    }

    #[test]
    fn read_mix_weights_sum_to_one() {
        for p in [
            Persona::FrontOffice,
            Persona::BatchProcessor,
            Persona::AnalyticsDash,
            Persona::Regulatorio,
            Persona::RandomHuman,
        ] {
            let cfg = PersonaConfig::for_persona(p);
            let sum: f64 = cfg.read_mix.iter().map(|(_, w)| w).sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "{:?} read_mix weights sum to {sum}, expected 1.0",
                p
            );
            let wsum: f64 = cfg.write_mix.iter().map(|(_, w)| w).sum();
            assert!(
                (wsum - 1.0).abs() < 1e-9,
                "{:?} write_mix weights sum to {wsum}, expected 1.0",
                p
            );
        }
    }

    #[test]
    fn random_human_covers_q4() {
        // The RandomHuman persona must include Q4 in its read mix — that's
        // the whole reason it exists. Captures the maintainer-requested
        // "Q4 must appear under sustained load" coverage.
        let cfg = PersonaConfig::for_persona(Persona::RandomHuman);
        let has_q4 = cfg
            .read_mix
            .iter()
            .any(|(q, w)| matches!(q, BusinessQuery::Q4TopExposure) && *w > 0.0);
        assert!(has_q4, "RandomHuman persona must include Q4 in read_mix");
    }

    #[test]
    fn parse_humanrandom_full_pool() {
        let a = PersonaAssignment::parse("humanrandom=9", 9).unwrap();
        assert_eq!(a.total_slots(), 9);
        let human_count = (0..9)
            .filter(|i| a.persona_for(*i) == Some(Persona::RandomHuman))
            .count();
        assert_eq!(human_count, 9);
    }

    #[test]
    fn default_assignment_for_9_threads() {
        let a = PersonaAssignment::default_for(9);
        let counts = (0..9).fold([0usize; 4], |mut acc, i| {
            match a.persona_for(i) {
                Some(Persona::FrontOffice) => acc[0] += 1,
                Some(Persona::BatchProcessor) => acc[1] += 1,
                Some(Persona::AnalyticsDash) => acc[2] += 1,
                Some(Persona::Regulatorio) => acc[3] += 1,
                // RandomHuman is opt-in via the --personas flag, never
                // emitted by `default_for` — the assertion guarantees that.
                Some(Persona::RandomHuman) => {
                    panic!("default_for must not assign RandomHuman; got it at slot {i}")
                }
                None => {}
            }
            acc
        });
        assert_eq!(
            counts,
            [4, 2, 2, 1],
            "default 9-thread assignment must be 4-2-2-1"
        );
    }

    #[test]
    fn parse_full_spec() {
        let a = PersonaAssignment::parse("front=4,batch=2,analytics=2,regulatorio=1", 9).unwrap();
        assert_eq!(a.total_slots(), 9);
        let front_count = (0..9)
            .filter(|i| a.persona_for(*i) == Some(Persona::FrontOffice))
            .count();
        assert_eq!(front_count, 4);
    }

    #[test]
    fn parse_partial_pads_idle() {
        // Sum = 5, total = 9 → 4 idle slots padded.
        let a = PersonaAssignment::parse("front=3,batch=2", 9).unwrap();
        let idle_count = (0..9).filter(|i| a.persona_for(*i).is_none()).count();
        assert_eq!(idle_count, 4);
    }

    #[test]
    fn parse_oversized_errors() {
        // Sum = 10, total = 9 → reject.
        let r = PersonaAssignment::parse("front=4,batch=3,analytics=2,regulatorio=1", 9);
        assert!(r.is_err());
    }

    #[test]
    fn parse_unknown_persona_errors() {
        let r = PersonaAssignment::parse("front=4,nonsense=2", 9);
        assert!(r.is_err());
    }

    #[test]
    fn parse_invalid_count_errors() {
        let r = PersonaAssignment::parse("front=abc", 9);
        assert!(r.is_err());
    }
}
