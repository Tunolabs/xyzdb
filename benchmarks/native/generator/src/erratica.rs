//! Workload errática per the v0.3.3 bench design.
//!
//! Replaces the v0.2.5 uniform 8R+1W loop with a parametric model that
//! exposes engine differences masked by uniform load:
//!   - MMPP 2-state per-thread arrivals (§6.1): bimodal Idle / Busy
//!   - LogNormal session lifecycle (§6.2): Connecting / Active /
//!     Disconnecting / InterSessionPause
//!   - Two-tier hot/cold 95/5 working-set drift with Markov walk (§6.3)
//!   - State-dependent R/W mix (§6.4): Idle 95 % R / Busy 70 % R
//!   - ChaCha20Rng with 6 domain-separated salts (§6.5) — deterministic
//!     event log per `(seed, scale, thread_id)` triple
//!   - 17 ERRATICA_* env vars (§6.7): defaults committed; alternative
//!     variants documented in design doc
//!
//! Reproducibility contract: same seed → byte-identical event log.
//! See `tests::reproducibility_seeded_event_log_byte_identical`.

use crate::bench::BusinessQuery;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

// ── Domain-separated PRNG salts (§6.5) ──────────────────────────────────────

const SALT_MMPP_TRANSITIONS: u64 = 0x10;
const SALT_MMPP_ARRIVALS: u64 = 0x20;
const SALT_SESSION_LIFECYCLE: u64 = 0x30;
const SALT_HOT_COLD_PICK: u64 = 0x40;
const SALT_DRIFT: u64 = 0x50;
const SALT_QUERY_MIX: u64 = 0x60;
const SALT_ANOMALY: u64 = 0x70;

// ── Defaults per §6.7 ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErraticaConfig {
    pub seed: u64,
    pub lambda_idle: f64,
    pub lambda_busy: f64,
    pub p_idle_busy: f64,
    pub p_busy_idle: f64,
    pub hot_ratio: f64,
    pub hot_query_prob: f64,
    pub drift_interval_secs: u64,
    pub drift_rate: f64,
    pub session_active_mean_secs: f64,
    pub session_active_sigma: f64,
    pub session_connect_mean_ms: f64,
    pub session_disconnect_mean_ms: f64,
    pub inter_session_mu_ln_secs: f64,
    pub inter_session_sigma: f64,
    pub reads_idle: f64,
    pub reads_busy: f64,
    pub phase3_duration_secs: u64,
    pub reader_threads: usize,
    pub writer_threads: usize,
}

impl Default for ErraticaConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            lambda_idle: 0.5,
            lambda_busy: 15.0,
            p_idle_busy: 0.003,
            p_busy_idle: 0.017,
            hot_ratio: 0.05,
            hot_query_prob: 0.95,
            drift_interval_secs: 60,
            drift_rate: 0.01,
            session_active_mean_secs: 120.0,
            session_active_sigma: 1.0,
            session_connect_mean_ms: 20.0,
            session_disconnect_mean_ms: 15.0,
            inter_session_mu_ln_secs: 600.0,
            inter_session_sigma: 1.5,
            reads_idle: 0.95,
            reads_busy: 0.70,
            phase3_duration_secs: 900,
            reader_threads: 8,
            writer_threads: 1,
        }
    }
}

impl ErraticaConfig {
    /// Load all `ERRATICA_*` env vars; missing or invalid values fall back
    /// to defaults (§6.7).
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("ERRATICA_SEED") {
            if let Ok(n) = v.parse::<u64>() {
                c.seed = n;
            }
        }
        macro_rules! load_f64 {
            ($var:literal, $field:ident) => {
                if let Ok(v) = std::env::var($var) {
                    if let Ok(n) = v.parse::<f64>() {
                        c.$field = n;
                    }
                }
            };
        }
        macro_rules! load_u64 {
            ($var:literal, $field:ident) => {
                if let Ok(v) = std::env::var($var) {
                    if let Ok(n) = v.parse::<u64>() {
                        c.$field = n;
                    }
                }
            };
        }
        macro_rules! load_usize {
            ($var:literal, $field:ident) => {
                if let Ok(v) = std::env::var($var) {
                    if let Ok(n) = v.parse::<usize>() {
                        c.$field = n;
                    }
                }
            };
        }
        load_f64!("ERRATICA_LAMBDA_IDLE", lambda_idle);
        load_f64!("ERRATICA_LAMBDA_BUSY", lambda_busy);
        load_f64!("ERRATICA_P_IDLE_BUSY", p_idle_busy);
        load_f64!("ERRATICA_P_BUSY_IDLE", p_busy_idle);
        load_f64!("ERRATICA_HOT_RATIO", hot_ratio);
        load_f64!("ERRATICA_HOT_QUERY_PROB", hot_query_prob);
        load_u64!("ERRATICA_DRIFT_INTERVAL_SEC", drift_interval_secs);
        load_f64!("ERRATICA_DRIFT_RATE", drift_rate);
        load_f64!("ERRATICA_SESSION_ACTIVE_MEAN_SEC", session_active_mean_secs);
        load_f64!("ERRATICA_SESSION_ACTIVE_SIGMA", session_active_sigma);
        load_f64!("ERRATICA_SESSION_CONNECT_MEAN_MS", session_connect_mean_ms);
        load_f64!(
            "ERRATICA_SESSION_DISCONNECT_MEAN_MS",
            session_disconnect_mean_ms
        );
        load_f64!(
            "ERRATICA_INTER_SESSION_PAUSE_LOGNORMAL_MU_LN_SEC",
            inter_session_mu_ln_secs
        );
        load_f64!(
            "ERRATICA_INTER_SESSION_PAUSE_LOGNORMAL_SIGMA",
            inter_session_sigma
        );
        load_f64!("ERRATICA_READS_IDLE", reads_idle);
        load_f64!("ERRATICA_READS_BUSY", reads_busy);
        load_u64!("ERRATICA_PHASE3_DURATION_SEC", phase3_duration_secs);
        load_usize!("ERRATICA_READER_THREADS", reader_threads);
        load_usize!("ERRATICA_WRITER_THREADS", writer_threads);
        c
    }
}

// ── ChaCha20Rng factory (domain-separated) ──────────────────────────────────

fn chacha(seed: u64, salt: u64, thread_id: u64) -> ChaCha20Rng {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&seed.to_le_bytes());
    bytes[8..16].copy_from_slice(&salt.to_le_bytes());
    bytes[16..24].copy_from_slice(&thread_id.to_le_bytes());
    ChaCha20Rng::from_seed(bytes)
}

/// Box-Muller standard-normal sample.
fn standard_normal(rng: &mut ChaCha20Rng) -> f64 {
    let u1: f64 = rng.random_range(1e-12..1.0);
    let u2: f64 = rng.random::<f64>();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// LogNormal sample from `(mu_ln, sigma)` parameterisation. `mu_ln` is the
/// log of the median (i.e. `ln(median)`); `sigma` is the shape parameter.
fn log_normal(rng: &mut ChaCha20Rng, mu_ln: f64, sigma: f64) -> f64 {
    (mu_ln + sigma * standard_normal(rng)).exp()
}

/// Exponential inter-arrival sample with rate `lambda` (events/sec).
fn exponential_inter_arrival(rng: &mut ChaCha20Rng, lambda: f64) -> f64 {
    let u: f64 = rng.random_range(1e-12..1.0);
    -u.ln() / lambda
}

// ── MMPP 2-state per-thread (§6.1) ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmppState {
    Idle,
    Busy,
}

pub struct MmppSampler {
    state: MmppState,
    rng_transitions: ChaCha20Rng,
    rng_arrivals: ChaCha20Rng,
    config: ErraticaConfig,
}

impl MmppSampler {
    pub fn new(thread_id: u64, config: &ErraticaConfig) -> Self {
        Self {
            state: MmppState::Idle,
            rng_transitions: chacha(config.seed, SALT_MMPP_TRANSITIONS, thread_id),
            rng_arrivals: chacha(config.seed, SALT_MMPP_ARRIVALS, thread_id),
            config: config.clone(),
        }
    }

    pub fn state(&self) -> MmppState {
        self.state
    }

    /// Sample next event arrival delay (exponential at the current state's
    /// lambda). Also rolls a state transition with `p_*` probability over
    /// the inter-arrival window.
    pub fn next_arrival(&mut self) -> Duration {
        let lambda = match self.state {
            MmppState::Idle => self.config.lambda_idle,
            MmppState::Busy => self.config.lambda_busy,
        };
        let dt_secs = exponential_inter_arrival(&mut self.rng_arrivals, lambda);

        // Probability of state transition over this window: p_rate * dt.
        let p_rate = match self.state {
            MmppState::Idle => self.config.p_idle_busy,
            MmppState::Busy => self.config.p_busy_idle,
        };
        let p_window = (p_rate * dt_secs).min(1.0);
        if self.rng_transitions.random::<f64>() < p_window {
            self.state = match self.state {
                MmppState::Idle => MmppState::Busy,
                MmppState::Busy => MmppState::Idle,
            };
        }
        Duration::from_secs_f64(dt_secs)
    }
}

// ── LogNormal session lifecycle (§6.2) ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionState {
    Connecting,
    Active,
    Disconnecting,
    InterSessionPause,
}

pub struct SessionLifecycle {
    state: SessionState,
    state_until: Instant,
    rng: ChaCha20Rng,
    config: ErraticaConfig,
}

impl SessionLifecycle {
    pub fn new(thread_id: u64, config: &ErraticaConfig, now: Instant) -> Self {
        let mut sl = Self {
            state: SessionState::Connecting,
            state_until: now,
            rng: chacha(config.seed, SALT_SESSION_LIFECYCLE, thread_id),
            config: config.clone(),
        };
        sl.transition_to(SessionState::Connecting, now);
        sl
    }

    fn transition_to(&mut self, new: SessionState, now: Instant) {
        let dur_ms = match new {
            SessionState::Connecting => {
                log_normal(&mut self.rng, self.config.session_connect_mean_ms.ln(), 0.5)
            }
            SessionState::Active => {
                log_normal(
                    &mut self.rng,
                    self.config.session_active_mean_secs.ln(),
                    self.config.session_active_sigma,
                ) * 1000.0
            }
            SessionState::Disconnecting => log_normal(
                &mut self.rng,
                self.config.session_disconnect_mean_ms.ln(),
                0.5,
            ),
            SessionState::InterSessionPause => {
                log_normal(
                    &mut self.rng,
                    self.config.inter_session_mu_ln_secs.ln(),
                    self.config.inter_session_sigma,
                ) * 1000.0
            }
        };
        self.state = new;
        self.state_until = now + Duration::from_millis(dur_ms.max(1.0) as u64);
    }

    /// Tick: advance the lifecycle if the current state's deadline elapsed.
    /// Returns the post-tick state (cycle Connecting → Active → Disconnecting
    /// → InterSessionPause → Connecting → ...).
    pub fn tick(&mut self, now: Instant) -> SessionState {
        if now >= self.state_until {
            let next = match self.state {
                SessionState::Connecting => SessionState::Active,
                SessionState::Active => SessionState::Disconnecting,
                SessionState::Disconnecting => SessionState::InterSessionPause,
                SessionState::InterSessionPause => SessionState::Connecting,
            };
            self.transition_to(next, now);
        }
        self.state
    }

    pub fn is_active(&self) -> bool {
        self.state == SessionState::Active
    }
}

// ── Two-tier hot/cold working set + Markov drift (§6.3) ─────────────────────

pub struct WorkingSet {
    pool: Vec<String>,
    hot_tier: Vec<usize>,
    cold_tier: Vec<usize>,
    rng_drift: ChaCha20Rng,
    rng_pick: ChaCha20Rng,
    drift_at: Instant,
    config: ErraticaConfig,
}

impl WorkingSet {
    pub fn new(pool: Vec<String>, config: &ErraticaConfig, now: Instant) -> Self {
        let n = pool.len();
        let hot_n = ((n as f64) * config.hot_ratio).ceil() as usize;
        let hot_n = hot_n.max(1).min(n.saturating_sub(1).max(1));
        let hot_tier: Vec<usize> = (0..hot_n).collect();
        let cold_tier: Vec<usize> = (hot_n..n).collect();
        Self {
            pool,
            hot_tier,
            cold_tier,
            rng_drift: chacha(config.seed, SALT_DRIFT, 0),
            rng_pick: chacha(config.seed, SALT_HOT_COLD_PICK, 0),
            drift_at: now + Duration::from_secs(config.drift_interval_secs),
            config: config.clone(),
        }
    }

    /// Tick: advance Markov drift (swap fraction of hot↔cold) if interval
    /// elapsed. Idempotent on multiple calls within the same window.
    pub fn tick(&mut self, now: Instant) {
        while now >= self.drift_at {
            let n_swap = ((self.hot_tier.len() as f64) * self.config.drift_rate).ceil() as usize;
            for _ in 0..n_swap {
                if self.hot_tier.is_empty() || self.cold_tier.is_empty() {
                    break;
                }
                let h_idx = self.rng_drift.random_range(0..self.hot_tier.len());
                let c_idx = self.rng_drift.random_range(0..self.cold_tier.len());
                self.hot_tier.swap(h_idx, h_idx);
                let h_val = self.hot_tier[h_idx];
                let c_val = self.cold_tier[c_idx];
                self.hot_tier[h_idx] = c_val;
                self.cold_tier[c_idx] = h_val;
            }
            self.drift_at += Duration::from_secs(self.config.drift_interval_secs);
        }
    }

    /// Pick an RFC: hot tier with probability `hot_query_prob`, cold tier
    /// otherwise. Returns a borrow into the pool.
    pub fn pick_rfc(&mut self) -> &str {
        let pick_hot = self.rng_pick.random::<f64>() < self.config.hot_query_prob;
        let tier = if pick_hot && !self.hot_tier.is_empty() {
            &self.hot_tier
        } else if !self.cold_tier.is_empty() {
            &self.cold_tier
        } else {
            &self.hot_tier
        };
        let idx = tier[self.rng_pick.random_range(0..tier.len())];
        &self.pool[idx]
    }
}

// ── State-dependent R/W mix + query mix (§6.4) ──────────────────────────────

/// Read-mix Phase 3 weights per design §6.4 (sum = 1.00).
const READ_MIX: &[(BusinessQuery, f64)] = &[
    (BusinessQuery::Q1Point, 0.25),
    (BusinessQuery::Q2Aggregate, 0.15),
    (BusinessQuery::Q3FullHistory, 0.10),
    // Q4 weight 0.00 — excluded from Phase 3 mix (still in Phase 2 cold).
    (BusinessQuery::Q5OverdueByEmpresa, 0.10),
    (BusinessQuery::Q6RecentPayments, 0.10),
    (BusinessQuery::Q8MonthlyClose, 0.10),
    (BusinessQuery::Q9CustomerContext, 0.20),
];

/// Write-mix Phase 3 weights per design §6.4 (sum = 1.00). Q10 (transactional
/// cascade) removed — deferred on xyzDB/Mongo, so Q7 is the sole write query.
const WRITE_MIX: &[(BusinessQuery, f64)] = &[(BusinessQuery::Q7BatchIngest, 1.00)];

/// v0.5 anomaly pool — heavy analytical queries injected into the
/// dispatch stream with `Schedule.anomaly_prob` probability during
/// peak / EOD windows. Captures the realistic "manager fires off a
/// top-exposures dashboard while the EOD batch is running" pattern.
/// Weights skew toward Q4 (the spec §6.4 ghost-pre-agg path that was
/// excluded from the canonical mix) and Q3 LIMIT-heavy historiales.
const ANOMALY_QUERIES: &[(BusinessQuery, f64)] = &[
    (BusinessQuery::Q4TopExposure, 0.40),
    (BusinessQuery::Q3FullHistory, 0.25),
    (BusinessQuery::Q8MonthlyClose, 0.20),
    (BusinessQuery::Q5OverdueByEmpresa, 0.15),
];

pub struct QueryMixer {
    rng: ChaCha20Rng,
    /// Active read mix. Defaults to the canonical Phase 3 `READ_MIX`;
    /// persona-aware construction overrides with the persona's own mix.
    read_mix: &'static [(BusinessQuery, f64)],
    /// Active write mix. Same semantics as `read_mix`.
    write_mix: &'static [(BusinessQuery, f64)],
}

impl QueryMixer {
    pub fn new(thread_id: u64, config: &ErraticaConfig) -> Self {
        Self {
            rng: chacha(config.seed, SALT_QUERY_MIX, thread_id),
            read_mix: READ_MIX,
            write_mix: WRITE_MIX,
        }
    }

    /// Construct with persona-specific read/write mixes (v0.5 multi-persona).
    pub fn with_mixes(
        thread_id: u64,
        config: &ErraticaConfig,
        read_mix: &'static [(BusinessQuery, f64)],
        write_mix: &'static [(BusinessQuery, f64)],
    ) -> Self {
        Self {
            rng: chacha(config.seed, SALT_QUERY_MIX, thread_id),
            read_mix,
            write_mix,
        }
    }

    /// Pick a query for the given MMPP state. Read/write decision uses
    /// `reads_idle` (Idle) or `reads_busy` (Busy); within the chosen branch,
    /// the query is sampled per the active mix weights.
    pub fn pick(&mut self, state: MmppState, config: &ErraticaConfig) -> BusinessQuery {
        let reads_p = match state {
            MmppState::Idle => config.reads_idle,
            MmppState::Busy => config.reads_busy,
        };
        let is_read = self.rng.random::<f64>() < reads_p;
        if is_read {
            sample_weighted(&mut self.rng, self.read_mix)
        } else {
            sample_weighted(&mut self.rng, self.write_mix)
        }
    }
}

fn sample_weighted(rng: &mut ChaCha20Rng, weights: &[(BusinessQuery, f64)]) -> BusinessQuery {
    let total: f64 = weights.iter().map(|(_, w)| *w).sum();
    let r = rng.random::<f64>() * total;
    let mut acc = 0.0;
    for (q, w) in weights {
        acc += *w;
        if r < acc {
            return *q;
        }
    }
    weights[weights.len() - 1].0
}

// ── Per-thread errática picker (combined harness) ───────────────────────────

/// Single-thread workload event — what the driver should do next.
///
/// Per spec §6.1 the MMPP samples an inter-arrival `dt` and then **emits the
/// event at `t + dt`**. The pre-fix loop returned `Sleep(dt)` and discarded
/// the prepared query, so the driver slept and then re-sampled — emitting a
/// query only on the rare `dt < 1ms` draw. That collapsed throughput to
/// ~0.03 ev/s. The fix bundles `(sleep, query)` so the driver sleeps the
/// inter-arrival gap and **then** executes the prepared query.
#[derive(Debug, Clone)]
pub enum ErraticaEvent {
    /// Sleep for the given duration with no query attached — used when the
    /// session is **not** in `Active` state (Connecting / Disconnecting /
    /// InterSessionPause) so the driver simply idles until the next state.
    Sleep(Duration),
    /// Sleep `sleep` (the MMPP inter-arrival gap) then issue `query`
    /// against `rfc`. Driver contract: `thread::sleep(sleep)` then execute.
    SleepThenQuery {
        sleep: Duration,
        query: BusinessQuery,
        rfc: String,
    },
    /// Issue the query immediately (MMPP sampled `dt <= 1ms`, the spec's
    /// sub-millisecond fast path).
    Query { query: BusinessQuery, rfc: String },
}

pub struct ErraticaPicker {
    pub config: ErraticaConfig,
    pub mmpp: MmppSampler,
    pub session: SessionLifecycle,
    pub working_set: WorkingSet,
    pub mixer: QueryMixer,
    /// v0.5 multi-persona — optional persona id used to look up the
    /// phase's `persona_boost` in the schedule.
    pub persona: Option<crate::personas::Persona>,
    /// v0.5 schedule — optional time-of-day intensity multiplier
    /// applied to MMPP inter-arrival samples.
    pub schedule: Option<crate::schedule::Schedule>,
    /// Anchor instant for `elapsed = now - run_start` schedule lookup.
    pub run_start: Instant,
    /// Total Phase 3 duration; schedule fractions resolve against this.
    pub total_duration: Duration,
    /// Domain-separated RNG stream for anomaly-injection rolls. Kept
    /// alongside the other streams so reproducibility holds when
    /// `Schedule.anomaly_prob > 0`.
    rng_anomaly: ChaCha20Rng,
}

impl ErraticaPicker {
    pub fn new(thread_id: u64, pool: Vec<String>, config: ErraticaConfig, now: Instant) -> Self {
        let rng_anomaly = chacha(config.seed, SALT_ANOMALY, thread_id);
        Self {
            mmpp: MmppSampler::new(thread_id, &config),
            session: SessionLifecycle::new(thread_id, &config, now),
            working_set: WorkingSet::new(pool, &config, now),
            mixer: QueryMixer::new(thread_id, &config),
            config,
            persona: None,
            schedule: None,
            run_start: now,
            total_duration: Duration::from_secs(1),
            rng_anomaly,
        }
    }

    /// v0.5 constructor — bakes the persona overrides into the
    /// `ErraticaConfig` (lambda multipliers, reads_idle/busy,
    /// session/pause durations), swaps in the persona-specific
    /// read/write mixes, and attaches the schedule for runtime
    /// intensity modulation.
    ///
    /// Backwards-compatible: callers that don't have a persona or
    /// schedule can keep using `new(...)`.
    #[allow(clippy::too_many_arguments)]
    pub fn with_persona(
        thread_id: u64,
        pool: Vec<String>,
        mut config: ErraticaConfig,
        now: Instant,
        persona: crate::personas::PersonaConfig,
        schedule: Option<crate::schedule::Schedule>,
        run_start: Instant,
        total_duration: Duration,
    ) -> Self {
        // Apply persona overrides to the config so MMPP / Session
        // samplers honour them via their existing &ErraticaConfig path.
        config.lambda_idle *= persona.lambda_idle_mult;
        config.lambda_busy *= persona.lambda_busy_mult;
        config.reads_idle = persona.reads_idle;
        config.reads_busy = persona.reads_busy;
        config.session_active_mean_secs = persona.session_active_secs;
        config.inter_session_mu_ln_secs = persona.pause_secs;
        let mixer = QueryMixer::with_mixes(thread_id, &config, persona.read_mix, persona.write_mix);
        let rng_anomaly = chacha(config.seed, SALT_ANOMALY, thread_id);
        Self {
            mmpp: MmppSampler::new(thread_id, &config),
            session: SessionLifecycle::new(thread_id, &config, now),
            working_set: WorkingSet::new(pool, &config, now),
            mixer,
            config,
            persona: Some(persona.persona),
            schedule,
            run_start,
            total_duration,
            rng_anomaly,
        }
    }

    /// Drive the workload: returns the next event. `Sleep(d)` means the
    /// driver thread should `thread::sleep(d)` before calling `next_event`
    /// again. `Query{..}` means the driver should execute that query now.
    pub fn next_event(&mut self, now: Instant) -> ErraticaEvent {
        // Drive session lifecycle and drift.
        let session_state = self.session.tick(now);
        self.working_set.tick(now);

        // Outside Active state: skip events (sleep until session_until).
        if session_state != SessionState::Active {
            let until = self.session.state_until;
            let dt = until.saturating_duration_since(now);
            return ErraticaEvent::Sleep(dt.max(Duration::from_millis(1)));
        }

        // Active session: sample MMPP arrival gap and *always* emit a query.
        // The driver sleeps `dt` (if > 1 ms) then executes — this is the
        // contract per spec §6.1 "emit event at t+dt". The pre-fix branch
        // returned `Sleep(dt)` and discarded the prepared `(query, rfc)`,
        // which is the cfbb7d5 bug that collapsed Phase 3 to ~0.03 ev/s.
        let dt_base = self.mmpp.next_arrival();
        // v0.5 schedule modulation: a higher phase multiplier means
        // more intense traffic, equivalent to a higher effective
        // lambda. Since `dt ~ Exp(lambda)`, `dt / multiplier ~
        // Exp(lambda * multiplier)`. We scale the sampled inter-arrival
        // directly rather than refactoring the MMPP sampler to take an
        // override.
        let (dt, anomaly_prob) = if let Some(sched) = &self.schedule {
            let elapsed = now.saturating_duration_since(self.run_start);
            let r = sched.resolve(elapsed, self.total_duration, self.persona);
            let scaled = if r.multiplier > 0.0 {
                Duration::from_secs_f64(dt_base.as_secs_f64() / r.multiplier)
            } else {
                dt_base
            };
            (scaled, r.anomaly_prob)
        } else {
            (dt_base, 0.0)
        };
        let mmpp_state = self.mmpp.state();
        // v0.5 anomaly injection: with probability `anomaly_prob` from
        // the active schedule phase, override the persona's mix and
        // emit a heavy analytical query (Q4 / Q3 LIMIT-heavy / Q8 /
        // Q5). Captures the realistic "manager fires off a dashboard
        // at the worst possible moment" pattern. RNG stream is
        // domain-separated so reproducibility holds with the same seed.
        let query = if anomaly_prob > 0.0 && self.rng_anomaly.random::<f64>() < anomaly_prob {
            sample_weighted(&mut self.rng_anomaly, ANOMALY_QUERIES)
        } else {
            self.mixer.pick(mmpp_state, &self.config)
        };
        let rfc = self.working_set.pick_rfc().to_string();
        if dt > Duration::from_millis(1) {
            ErraticaEvent::SleepThenQuery {
                sleep: dt,
                query,
                rfc,
            }
        } else {
            ErraticaEvent::Query { query, rfc }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pool(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("RFC{:07}", i)).collect()
    }

    /// Reproducibility CI gate (§6.5 + §11.4 acceptance criterion):
    /// same `seed` → byte-identical event log across two independent
    /// `ErraticaPicker` instances.
    #[test]
    fn reproducibility_seeded_event_log_byte_identical() {
        let cfg = ErraticaConfig::default();
        let pool = make_pool(10_000);
        let now = Instant::now();
        let mut p1 = ErraticaPicker::new(7, pool.clone(), cfg.clone(), now);
        let mut p2 = ErraticaPicker::new(7, pool, cfg, now);
        // 200 events span at least one MMPP transition + one session
        // lifecycle cycle in expectation; sufficient to detect drift.
        for i in 0..200 {
            let e1 = p1.next_event(now + Duration::from_secs(i));
            let e2 = p2.next_event(now + Duration::from_secs(i));
            match (&e1, &e2) {
                (ErraticaEvent::Sleep(d1), ErraticaEvent::Sleep(d2)) => {
                    assert_eq!(d1, d2, "Sleep durations diverge at step {i}");
                }
                (
                    ErraticaEvent::SleepThenQuery {
                        sleep: s1,
                        query: q1,
                        rfc: r1,
                    },
                    ErraticaEvent::SleepThenQuery {
                        sleep: s2,
                        query: q2,
                        rfc: r2,
                    },
                ) => {
                    assert_eq!(s1, s2, "SleepThenQuery sleep diverges at step {i}");
                    assert_eq!(q1, q2, "SleepThenQuery query diverges at step {i}");
                    assert_eq!(r1, r2, "SleepThenQuery rfc diverges at step {i}");
                }
                (
                    ErraticaEvent::Query { query: q1, rfc: r1 },
                    ErraticaEvent::Query { query: q2, rfc: r2 },
                ) => {
                    assert_eq!(q1, q2, "Query divergence at step {i}");
                    assert_eq!(r1, r2, "RFC divergence at step {i}");
                }
                _ => panic!("Event-kind divergence at step {i}: {e1:?} vs {e2:?}"),
            }
        }
    }

    #[test]
    fn mmpp_state_starts_idle_and_can_transition() {
        let cfg = ErraticaConfig::default();
        let mut s = MmppSampler::new(0, &cfg);
        assert_eq!(s.state(), MmppState::Idle);
        // Drive enough samples to observe at least one transition.
        let mut saw_busy = false;
        for _ in 0..10_000 {
            let _ = s.next_arrival();
            if s.state() == MmppState::Busy {
                saw_busy = true;
                break;
            }
        }
        assert!(
            saw_busy,
            "MMPP should transition Idle→Busy within 10k samples"
        );
    }

    #[test]
    fn session_lifecycle_cycles_through_four_states() {
        let cfg = ErraticaConfig {
            // Shrink state durations so a tight loop hits all four states.
            session_connect_mean_ms: 1.0,
            session_active_mean_secs: 0.001,
            session_disconnect_mean_ms: 1.0,
            inter_session_mu_ln_secs: 0.001,
            inter_session_sigma: 0.1,
            ..ErraticaConfig::default()
        };
        let now = Instant::now();
        let mut sl = SessionLifecycle::new(0, &cfg, now);
        let mut seen = std::collections::HashSet::new();
        for ms in 0..2000 {
            let s = sl.tick(now + Duration::from_millis(ms));
            seen.insert(s);
        }
        assert!(seen.contains(&SessionState::Connecting));
        assert!(seen.contains(&SessionState::Active));
        assert!(seen.contains(&SessionState::Disconnecting));
        assert!(seen.contains(&SessionState::InterSessionPause));
    }

    #[test]
    fn working_set_pick_skews_toward_hot_tier() {
        let cfg = ErraticaConfig::default();
        let pool = make_pool(1000);
        let now = Instant::now();
        let mut ws = WorkingSet::new(pool, &cfg, now);
        // Hot tier = first 50 (5 % of 1000). Sample 10k picks; hot share
        // should be ≥ 90 % (allowing for cold-pick events).
        let mut hot_hits = 0;
        for _ in 0..10_000 {
            let rfc = ws.pick_rfc();
            let idx: usize = rfc.trim_start_matches("RFC").parse().unwrap();
            if idx < 50 {
                hot_hits += 1;
            }
        }
        assert!(
            hot_hits >= 9_000,
            "Hot-pick share should be ≥ 90 % with hot_query_prob=0.95; got {hot_hits}/10000"
        );
    }

    #[test]
    fn query_mixer_busy_state_produces_more_writes_than_idle() {
        let cfg = ErraticaConfig::default();
        let mut idle_writes = 0;
        let mut busy_writes = 0;
        let mut m_idle = QueryMixer::new(0, &cfg);
        let mut m_busy = QueryMixer::new(1, &cfg);
        for _ in 0..10_000 {
            if matches!(
                m_idle.pick(MmppState::Idle, &cfg),
                BusinessQuery::Q7BatchIngest
            ) {
                idle_writes += 1;
            }
            if matches!(
                m_busy.pick(MmppState::Busy, &cfg),
                BusinessQuery::Q7BatchIngest
            ) {
                busy_writes += 1;
            }
        }
        assert!(
            busy_writes > idle_writes,
            "Busy state should produce more writes (got idle={idle_writes} busy={busy_writes})"
        );
    }

    /// Regression guard for the cfbb7d5 dispatch-loop bug (closed
    /// 2026-05-14). Pre-fix, `next_event` returned `Sleep(dt)` and
    /// dropped the prepared `(query, rfc)` whenever the MMPP inter-arrival
    /// landed above 1 ms — i.e. 98-99.5 % of the time. The fix bundles
    /// them into `SleepThenQuery` so the driver sleeps the gap and then
    /// executes the prepared query.
    ///
    /// Acceptance: across 500 events while a thread is in `Active`
    /// session state, the count of `Sleep`-without-query (still legal
    /// for non-Active states) must NOT dominate the trace. With a
    /// short-pause config the thread is in Active most of the time, so
    /// the vast majority of events must be query-bearing.
    #[test]
    fn dispatch_loop_emits_query_after_sleep_in_active_state() {
        // Shrink session lifecycle so threads land in Active almost
        // immediately and stay there for the duration of the trace.
        let cfg = ErraticaConfig {
            session_connect_mean_ms: 1.0,
            session_active_mean_secs: 600.0,
            session_disconnect_mean_ms: 1.0,
            inter_session_mu_ln_secs: 0.001,
            inter_session_sigma: 0.1,
            ..ErraticaConfig::default()
        };
        let pool = make_pool(10_000);
        let now = Instant::now();
        let mut p = ErraticaPicker::new(13, pool, cfg, now);
        let mut sleep_only = 0;
        let mut sleep_then_query = 0;
        let mut query_only = 0;
        for i in 0..500 {
            let t = now + Duration::from_millis(i * 10);
            match p.next_event(t) {
                ErraticaEvent::Sleep(_) => sleep_only += 1,
                ErraticaEvent::SleepThenQuery { .. } => sleep_then_query += 1,
                ErraticaEvent::Query { .. } => query_only += 1,
            }
        }
        let query_bearing = sleep_then_query + query_only;
        // Pre-fix: query_bearing would be ~0-3 (only sub-ms draws); the
        // bug produced almost-all `Sleep`. Post-fix: most events must
        // carry a query payload.
        assert!(
            query_bearing >= 400,
            "Expected ≥400/500 query-bearing events in Active state; got \
             sleep_only={sleep_only} sleep_then_query={sleep_then_query} \
             query_only={query_only}"
        );
    }

    /// Order-of-magnitude rate check against the MMPP λ_idle parameter.
    /// Sampling `next_arrival` directly on a sampler stuck in Idle
    /// state, the average inter-arrival should converge near
    /// `1 / λ_idle` seconds (= 2 s under default 0.5 ev/s). This
    /// guarantees the bug fix did not break the underlying MMPP
    /// arrival distribution.
    #[test]
    fn mmpp_idle_inter_arrival_matches_lambda() {
        let cfg = ErraticaConfig {
            // Disable transitions so the sampler stays in Idle for the
            // whole trace; we are testing λ_idle, not the modulator.
            p_idle_busy: 0.0,
            p_busy_idle: 0.0,
            ..ErraticaConfig::default()
        };
        let mut s = MmppSampler::new(0, &cfg);
        let n = 1000;
        let mut total_secs = 0.0f64;
        for _ in 0..n {
            total_secs += s.next_arrival().as_secs_f64();
        }
        let mean = total_secs / n as f64;
        let expected = 1.0 / cfg.lambda_idle; // 2.0 s
        let lo = expected * 0.85;
        let hi = expected * 1.15;
        assert!(
            mean >= lo && mean <= hi,
            "λ_idle = {} ⇒ mean dt expected ≈ {expected:.3} s, got {mean:.3} s",
            cfg.lambda_idle
        );
    }
}
