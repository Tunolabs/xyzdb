//! Time-of-day workload schedule per the v0.3.3 bench design
//! §6.4 extension (v0.5 — multi-phase workload modulation).
//!
//! Real fintech ERP traffic is not uniform over 24h. There are quiet
//! overnight windows, a morning ramp-up, a midday peak, an afternoon
//! tail, an EOD batch ingest burst (quincena pattern), and a brief
//! evening quiet. The schedule expresses these phases as fractions of
//! the total Phase 3 duration, each phase carrying a global intensity
//! `multiplier` plus optional per-persona boosts.
//!
//! Resolution at runtime: given `(elapsed, total_duration)` the engine
//! returns the active phase + an effective `lambda_multiplier` for a
//! given persona. Drivers apply that multiplier when sampling MMPP
//! inter-arrivals via the persona-aware `ErraticaPicker`.
//!
//! Format (YAML):
//!
//! ```yaml
//! phases:
//!   - name: madrugada-quiet
//!     fraction: 0.10
//!     multiplier: 0.3
//!   - name: peak-midday
//!     fraction: 0.25
//!     multiplier: 1.5
//!   - name: eod-batch-burst
//!     fraction: 0.10
//!     multiplier: 2.5
//!     persona_boost:
//!       batch: 3.0
//! ```
//!
//! Phase fractions must sum to 1.0 within ±0.01. Multipliers are
//! applied to the persona's `lambda_busy_mult` (and inversely scaled
//! against `lambda_idle_mult` to keep total event count bounded).

use crate::personas::Persona;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// One phase of the daily schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulePhase {
    /// Human-readable name (logs / reports).
    pub name: String,
    /// Fraction of the total Phase 3 duration this phase occupies.
    /// Phases are concatenated in order; sum across phases must be
    /// 1.0 ± 0.01.
    pub fraction: f64,
    /// Global intensity multiplier applied to every persona's
    /// `lambda_busy_mult` during this phase. 1.0 = no change.
    pub multiplier: f64,
    /// Optional per-persona extra boost (multiplicative on top of
    /// `multiplier`). Map key uses the `Persona::as_str` id.
    #[serde(default)]
    pub persona_boost: HashMap<String, f64>,
    /// Probability per emitted event that the picker overrides the
    /// persona's mix and injects an "anomaly" query instead (Q4
    /// top-exposure, Q3 LIMIT-heavy, Q8 monthly close, Q5 overdue).
    /// Models the realistic pattern where heavy analytical queries
    /// fire unexpectedly during peak / EOD windows — a teller's
    /// dashboard manager pinging the system at the worst possible
    /// moment. Default 0.0 (no injection).
    #[serde(default)]
    pub anomaly_prob: f64,
}

/// Ordered list of phases that together describe one Phase 3 run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub phases: Vec<SchedulePhase>,
}

impl Schedule {
    /// Built-in `daily_erp` default: 6 phases covering 24h fintech ERP
    /// pattern. Scales proportionally for any `--duration`. Anomaly
    /// injection skewed toward peak / EOD phases (manager-pinging-
    /// at-the-worst-moment realism).
    pub fn daily_erp() -> Self {
        Self {
            phases: vec![
                SchedulePhase {
                    name: "madrugada-quiet".into(),
                    fraction: 0.10,
                    multiplier: 0.3,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.0,
                },
                SchedulePhase {
                    name: "ramp-morning".into(),
                    fraction: 0.15,
                    multiplier: 0.8,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.005, // 0.5 % — early dashboards opening
                },
                SchedulePhase {
                    name: "peak-midday".into(),
                    fraction: 0.25,
                    multiplier: 1.5,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.02, // 2 % — managers spamming reports
                },
                SchedulePhase {
                    name: "tail-afternoon".into(),
                    fraction: 0.30,
                    multiplier: 1.0,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.01,
                },
                SchedulePhase {
                    name: "eod-batch-burst".into(),
                    fraction: 0.10,
                    multiplier: 2.5,
                    persona_boost: HashMap::from([("batch".into(), 3.0)]),
                    anomaly_prob: 0.10, // 10 % — auditors + EOD reports collide
                },
                SchedulePhase {
                    name: "noche-quiet".into(),
                    fraction: 0.10,
                    multiplier: 0.4,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.0,
                },
            ],
        }
    }

    /// Single-phase uniform schedule with `multiplier = 1.0`. Used as
    /// a fallback when no schedule is configured (matches pre-v0.5
    /// behaviour).
    pub fn uniform() -> Self {
        Self {
            phases: vec![SchedulePhase {
                name: "uniform".into(),
                fraction: 1.0,
                multiplier: 1.0,
                persona_boost: HashMap::new(),
                anomaly_prob: 0.0,
            }],
        }
    }

    /// Parse from YAML source. Validates that phase fractions sum to
    /// 1.0 ± 0.01 and that all multipliers are positive.
    pub fn from_yaml(src: &str) -> Result<Self, String> {
        let s: Self = serde_yaml::from_str(src).map_err(|e| format!("YAML parse: {e}"))?;
        s.validate()?;
        Ok(s)
    }

    /// Read + parse a YAML file at `path`.
    pub fn from_yaml_file(path: &std::path::Path) -> Result<Self, String> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("read schedule {}: {e}", path.display()))?;
        Self::from_yaml(&src)
    }

    fn validate(&self) -> Result<(), String> {
        if self.phases.is_empty() {
            return Err("schedule has no phases".into());
        }
        let sum: f64 = self.phases.iter().map(|p| p.fraction).sum();
        if (sum - 1.0).abs() > 0.01 {
            return Err(format!(
                "phase fractions sum to {sum:.4}, expected 1.0 ± 0.01"
            ));
        }
        for p in &self.phases {
            if p.multiplier <= 0.0 {
                return Err(format!(
                    "phase '{}' has non-positive multiplier {}",
                    p.name, p.multiplier
                ));
            }
            if p.fraction <= 0.0 {
                return Err(format!(
                    "phase '{}' has non-positive fraction {}",
                    p.name, p.fraction
                ));
            }
            if !(0.0..=1.0).contains(&p.anomaly_prob) {
                return Err(format!(
                    "phase '{}' anomaly_prob {} out of range [0.0, 1.0]",
                    p.name, p.anomaly_prob
                ));
            }
            for (k, v) in &p.persona_boost {
                if Persona::from_str(k).is_none() {
                    return Err(format!(
                        "phase '{}' persona_boost references unknown persona '{k}'",
                        p.name
                    ));
                }
                if *v <= 0.0 {
                    return Err(format!(
                        "phase '{}' persona_boost {k}={v} must be positive",
                        p.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolve the phase active at `elapsed` into a total run of
    /// `total_duration`. Returns the phase index, its name, and the
    /// effective multiplier for `persona` (combining global
    /// `multiplier` × persona_boost if applicable).
    pub fn resolve(
        &self,
        elapsed: Duration,
        total_duration: Duration,
        persona: Option<Persona>,
    ) -> ResolvedPhase {
        if self.phases.is_empty() {
            return ResolvedPhase {
                index: 0,
                name: "(empty)".into(),
                multiplier: 1.0,
                anomaly_prob: 0.0,
            };
        }
        let total_secs = total_duration.as_secs_f64().max(1.0);
        let elapsed_frac = (elapsed.as_secs_f64() / total_secs).clamp(0.0, 1.0);
        let mut cumulative = 0.0;
        for (i, p) in self.phases.iter().enumerate() {
            cumulative += p.fraction;
            if elapsed_frac <= cumulative + 1e-9 {
                let boost = persona
                    .and_then(|p_id| p.persona_boost.get(p_id.as_str()))
                    .copied()
                    .unwrap_or(1.0);
                return ResolvedPhase {
                    index: i,
                    name: p.name.clone(),
                    multiplier: p.multiplier * boost,
                    anomaly_prob: p.anomaly_prob,
                };
            }
        }
        // Numeric edge case: elapsed_frac == 1.0 lands here.
        let last = self.phases.last().unwrap();
        let boost = persona
            .and_then(|p_id| last.persona_boost.get(p_id.as_str()))
            .copied()
            .unwrap_or(1.0);
        ResolvedPhase {
            index: self.phases.len() - 1,
            name: last.name.clone(),
            multiplier: last.multiplier * boost,
            anomaly_prob: last.anomaly_prob,
        }
    }
}

/// Resolved phase at a given elapsed time.
#[derive(Debug, Clone)]
pub struct ResolvedPhase {
    pub index: usize,
    pub name: String,
    /// Effective lambda multiplier for the queried persona (global ×
    /// persona_boost).
    pub multiplier: f64,
    /// Probability of anomaly-query injection during this phase.
    /// Forwarded verbatim from the active `SchedulePhase.anomaly_prob`.
    pub anomaly_prob: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_erp_default_validates() {
        let s = Schedule::daily_erp();
        s.validate().expect("daily_erp must validate");
        let frac_sum: f64 = s.phases.iter().map(|p| p.fraction).sum();
        assert!((frac_sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn uniform_schedule_validates() {
        Schedule::uniform().validate().unwrap();
    }

    #[test]
    fn yaml_roundtrip() {
        let src = r#"
phases:
  - name: a
    fraction: 0.5
    multiplier: 1.0
  - name: b
    fraction: 0.5
    multiplier: 2.0
    persona_boost:
      batch: 3.0
"#;
        let s = Schedule::from_yaml(src).unwrap();
        assert_eq!(s.phases.len(), 2);
        assert_eq!(s.phases[1].persona_boost.get("batch"), Some(&3.0));
    }

    #[test]
    fn yaml_rejects_unbalanced_phases() {
        // 0.5 + 0.3 = 0.8, not 1.0
        let src = r#"
phases:
  - name: a
    fraction: 0.5
    multiplier: 1.0
  - name: b
    fraction: 0.3
    multiplier: 1.0
"#;
        assert!(Schedule::from_yaml(src).is_err());
    }

    #[test]
    fn yaml_rejects_unknown_persona_boost() {
        let src = r#"
phases:
  - name: a
    fraction: 1.0
    multiplier: 1.0
    persona_boost:
      ceo: 5.0
"#;
        assert!(Schedule::from_yaml(src).is_err());
    }

    #[test]
    fn resolve_picks_correct_phase() {
        let s = Schedule {
            phases: vec![
                SchedulePhase {
                    name: "first".into(),
                    fraction: 0.3,
                    multiplier: 1.0,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.0,
                },
                SchedulePhase {
                    name: "middle".into(),
                    fraction: 0.4,
                    multiplier: 2.0,
                    persona_boost: HashMap::from([("batch".into(), 4.0)]),
                    anomaly_prob: 0.0,
                },
                SchedulePhase {
                    name: "last".into(),
                    fraction: 0.3,
                    multiplier: 0.5,
                    persona_boost: HashMap::new(),
                    anomaly_prob: 0.0,
                },
            ],
        };
        let total = Duration::from_secs(100);

        // 10s elapsed → first phase
        let r = s.resolve(Duration::from_secs(10), total, None);
        assert_eq!(r.name, "first");
        assert!((r.multiplier - 1.0).abs() < 1e-9);

        // 50s elapsed → middle phase (30%-70% window)
        let r = s.resolve(Duration::from_secs(50), total, None);
        assert_eq!(r.name, "middle");
        assert!((r.multiplier - 2.0).abs() < 1e-9);

        // 50s elapsed + batch persona → multiplier with boost 2.0×4.0
        let r = s.resolve(
            Duration::from_secs(50),
            total,
            Some(Persona::BatchProcessor),
        );
        assert!((r.multiplier - 8.0).abs() < 1e-9);

        // 90s elapsed → last phase
        let r = s.resolve(Duration::from_secs(90), total, None);
        assert_eq!(r.name, "last");
    }

    #[test]
    fn resolve_at_boundaries_lands_on_correct_phase() {
        let s = Schedule::daily_erp();
        let total = Duration::from_secs(3600);
        // Time 0 should land in first phase
        let r = s.resolve(Duration::ZERO, total, None);
        assert_eq!(r.index, 0);
        // Time = total should land in last phase
        let r = s.resolve(total, total, None);
        assert_eq!(r.index, s.phases.len() - 1);
    }

    #[test]
    fn daily_erp_anomaly_prob_is_peak_skewed() {
        // The maintainer-requested pattern: anomaly injection must be
        // concentrated in peak / EOD windows, near-zero in quiet
        // windows. Confirms the daily_erp profile honours the design.
        let s = Schedule::daily_erp();
        let total = Duration::from_secs(3600);
        // Madrugada (0-10 %): no anomalies.
        let madrugada = s.resolve(Duration::from_secs(180), total, None);
        assert_eq!(madrugada.name, "madrugada-quiet");
        assert!((madrugada.anomaly_prob - 0.0).abs() < 1e-9);
        // Peak midday: > 0.
        let peak = s.resolve(Duration::from_secs(1800), total, None);
        assert_eq!(peak.name, "peak-midday");
        assert!(peak.anomaly_prob > 0.01);
        // EOD batch burst: highest anomaly_prob in the schedule.
        let eod_frac = 0.10 + 0.15 + 0.25 + 0.30 + 0.05; // mid of eod-burst
        let eod = s.resolve(Duration::from_secs_f64(3600.0 * eod_frac), total, None);
        assert_eq!(eod.name, "eod-batch-burst");
        assert!(eod.anomaly_prob >= peak.anomaly_prob);
    }

    #[test]
    fn yaml_anomaly_prob_out_of_range_rejected() {
        let src = r#"
phases:
  - name: a
    fraction: 1.0
    multiplier: 1.0
    anomaly_prob: 1.5
"#;
        assert!(Schedule::from_yaml(src).is_err());
    }
}
