//! The StateArbiter (04 §2.4–§2.5) and drift telemetry (04 §12.3).
//!
//! The rules live here as pure functions so they can be unit-tested without a server:
//!
//! 1. process death overrides at once (handled by `end_run`, mirrored in [`judge`]);
//! 2. open interactions are authoritative until resolved (`resolve` never closes one on silence);
//! 3. silence is not staleness: a lower-precedence transport may only **add information**
//!    ([`Verdict::Keep`]), never overwrite a structured state that is still alive;
//! 4. structured loss is explicit: [`structured_lost`] marks the adapter `disconnected`, then
//!    recomputes execution from the next available transport (reconcile → self-report → screen →
//!    process, [`fallback`]) and marks it `inferred`;
//! 5. precedence among simultaneous fresh signals: structured > self-report > screen > process;
//! 6. unvalidated versions run rules 1–5 with `confidence ≤ 0.8`.
//!
//! Drift telemetry (local only) counts, per harness version, how often another transport
//! disagreed with the structured one, how often an interaction was resolved by a path nobody
//! reported and how often an answer failed. A spike raises a one-time notification
//! ("Claude 2.2.0 behaves differently than Vibeke expects…") and `agent.drift_detected`.

use super::*;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Precedence among simultaneous fresh signals (rule 5): higher wins.
pub fn precedence(s: StateSource) -> u8 {
    match s {
        StateSource::User | StateSource::Structured => 4,
        StateSource::SelfReport => 3,
        StateSource::Screen => 2,
        StateSource::Process => 1,
    }
}

/// What the arbiter knows about the run's transports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ctx {
    /// The transport behind the current state is alive (rule 3 / 4).
    pub higher_alive: bool,
    /// Adapter health `unvalidated_version` (rule 6).
    pub unvalidated: bool,
}

pub fn ctx_of(run: &AgentRun) -> Ctx {
    Ctx {
        higher_alive: run.health != AdapterHealth::Disconnected
            && run.execution.source == StateSource::Structured,
        unvalidated: run.health == AdapterHealth::UnvalidatedVersion,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Apply the proposal. `inferred`: the winning transport is not the structured one because
    /// that channel is gone (rule 4); the UI marks the state as inferred.
    Apply { confidence: f32, inferred: bool },
    /// Keep the current state; the lower-precedence signal only adds information.
    /// `disagrees`: it names a different state (counts toward drift telemetry).
    Keep { disagrees: bool },
}

/// Decide whether `proposed` from `src` replaces the run's current execution facet.
pub fn judge(
    cur: &Facet<Execution>,
    proposed: &Execution,
    src: StateSource,
    conf: f32,
    ctx: Ctx,
) -> Verdict {
    // Rule 1: the process is the one thing that is always authoritative.
    if *proposed == Execution::Exited && src == StateSource::Process {
        return Verdict::Apply {
            confidence: 1.0,
            inferred: false,
        };
    }
    // An exited run stays exited.
    if cur.value == Execution::Exited {
        return Verdict::Keep { disagrees: false };
    }
    let cap = |c: f32| if ctx.unvalidated { c.min(0.8) } else { c };
    if precedence(src) >= precedence(cur.source) {
        return Verdict::Apply {
            confidence: cap(conf),
            inferred: false,
        };
    }
    // Lower precedence than the current state's source.
    if ctx.higher_alive {
        // Rule 3: silence is not staleness; add information only.
        return Verdict::Keep {
            disagrees: cur.value != *proposed,
        };
    }
    // Rule 4: the higher transport is gone; the next available one drives, marked inferred
    // (only a lost *structured* channel makes the state inferred: a self-report that a screen
    // match replaces is just the next-best evidence).
    Verdict::Apply {
        confidence: cap(conf.min(0.7)),
        inferred: cur.source == StateSource::Structured,
    }
}

/// Next available transport after structured loss (rule 4): self-report → screen → process.
/// `process_alive` without any other evidence yields `Unknown` at low confidence.
pub fn fallback(
    self_report: Option<(Execution, f32)>,
    screen: Option<(Execution, f32)>,
    process_alive: bool,
) -> (Execution, StateSource, f32) {
    if let Some((e, c)) = self_report {
        return (e, StateSource::SelfReport, c.min(0.7));
    }
    if let Some((e, c)) = screen {
        return (e, StateSource::Screen, c.min(0.7));
    }
    if process_alive {
        (Execution::Unknown, StateSource::Process, 0.3)
    } else {
        (Execution::Exited, StateSource::Process, 1.0)
    }
}

/// `detail` marker of a state recomputed after structured loss.
pub const INFERRED: &str = "inferred";

/// Does `detail` mark an inferred state?
pub fn is_inferred(detail: Option<&str>) -> bool {
    detail.is_some_and(|d| d == INFERRED || d.starts_with("inferred:"))
}

/// Rule 4: a structured channel is known to be gone (extension socket closed, headless protocol
/// EOF, adapter health `disconnected`). Marks the adapter disconnected, attempts `reconcile` where
/// the run has it (transcript read for hook runs) and recomputes the execution state from the
/// next transport, marked `inferred`.
pub fn structured_lost(server: &Arc<Server>, run_id: &str, reason: &str) {
    let Some(run) = server.with_core(|c| c.run(run_id).cloned()) else {
        return;
    };
    if run.health == AdapterHealth::Disconnected || run.ended_at_ms.is_some() {
        return;
    }
    update_run(server, run_id, |r, tx| {
        let from = format!("{:?}", r.health).to_lowercase();
        r.health = AdapterHealth::Disconnected;
        tx.event(
            "adapter.health_changed",
            json!({"run": r.id}),
            json!({"to": "disconnected", "from": from, "transport": r.integration, "reason": reason}),
        );
    });
    // Reconcile (transcript) first, then the screen, then the process.
    let reconciled = super::tailer::reconcile_state(server, &run);
    let screen_state = screen_state_of(server, &run);
    let alive = server
        .pane_rt(&run.pane)
        .is_some_and(|rt| rt.status.lock().unwrap().as_ref().is_none_or(|s| !s.exited));
    let (to, src, conf) = match reconciled {
        Some(e) => (e, StateSource::Structured, 0.7),
        None => fallback(None, screen_state, alive),
    };
    if to != run.execution.value || src != run.execution.source {
        set_execution(
            server,
            run_id,
            to,
            src,
            conf,
            Some(match src {
                StateSource::Structured => "inferred:transcript".to_string(),
                _ => INFERRED.to_string(),
            }),
        );
    } else {
        update_run(server, run_id, |r, _| {
            r.execution.detail = Some(INFERRED.into());
            r.execution.confidence = r.execution.confidence.min(0.7);
        });
    }
}

/// Signals that are the arbiter's own (`TransportLost`: the extension or protocol channel closed
/// while the harness lives, 04 §2.5 rule 4). Returns `true` when the event was consumed.
pub fn handle_signal(server: &Arc<Server>, pane: &str, event: &str, p: &Value) -> bool {
    if event != "TransportLost" {
        return false;
    }
    if let Some(run) = server.with_core(|c| c.run_for_pane(pane).cloned()) {
        let reason = p
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("transport_lost");
        structured_lost(server, &run.id, reason);
    }
    true
}

/// The structured transport spoke again: health recovers (`bound_run` flips it) and the inferred
/// marker goes away with the next structured state.
pub fn structured_restored(server: &Server, run_id: &str) {
    update_run(server, run_id, |r, _| {
        if is_inferred(r.execution.detail.as_deref()) {
            r.execution.detail = None;
        }
    });
}

fn screen_state_of(server: &Server, run: &AgentRun) -> Option<(Execution, f32)> {
    let h = Harness::from_id(&run.harness)?;
    let rt = server.pane_rt(&run.pane)?;
    let text = rt.screen.lock().unwrap().engine.screen_text();
    screen::evaluate(h, &text).state
}

// ---- drift telemetry ----------------------------------------------------------------------------

/// Minimum observations before a version's rate is judged.
pub const MIN_OBSERVATIONS: u64 = 20;
/// A version whose bad-signal rate reaches this is drifting on its own.
pub const ABSOLUTE_RATE: f64 = 0.25;
/// …or whose rate is this many times the best other version's, above [`RELATIVE_FLOOR`].
pub const RELATIVE_FACTOR: f64 = 3.0;
pub const RELATIVE_FLOOR: f64 = 0.10;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Counts {
    /// Execution transitions seen (the denominator).
    pub observations: u64,
    /// Another transport named a different state than the structured one.
    pub disagreements: u64,
    /// Interactions resolved by a path nobody reported.
    pub unknown_resolutions: u64,
    /// Answers whose delivery failed or ended unknown.
    pub answer_failures: u64,
    /// The drift notification already fired for this version.
    pub notified: bool,
}

impl Counts {
    pub fn bad(&self) -> u64 {
        self.disagreements + self.unknown_resolutions + self.answer_failures
    }

    pub fn rate(&self) -> f64 {
        self.bad() as f64 / self.observations.max(1) as f64
    }
}

/// Pure spike test: `baseline` is the best rate among the harness's other versions with enough
/// observations.
pub fn is_spike(c: &Counts, baseline: Option<f64>) -> bool {
    if c.notified || c.observations < MIN_OBSERVATIONS {
        return false;
    }
    let r = c.rate();
    r >= ABSOLUTE_RATE || baseline.is_some_and(|b| r >= RELATIVE_FLOOR && r >= b * RELATIVE_FACTOR)
}

/// Resolution reasons that come from a native signal or a known Vibeke path.
pub fn resolution_known(reason: &str) -> bool {
    matches!(
        reason,
        "PostToolUse"
            | "PostToolUseFailure"
            | "PermissionDenied"
            | "turn ended"
            | "tool ran"
            | "dialog closed"
            | "self-report moved on"
            | "process_exited"
            | "gate_timeout"
    ) || reason.starts_with("rpc:")
        || reason.starts_with("native:")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Observation,
    Disagreement,
    UnknownResolution,
    AnswerFailure,
}

static COUNTS: LazyLock<Mutex<BTreeMap<String, Counts>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
/// run → (last reported disagreement, when).
static LAST_DISAGREEMENT: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn key(harness: &str, version: &str) -> String {
    format!("{harness}@{version}")
}

const KV_SCOPE: &str = "drift";

/// Record one signal for the run's harness version and raise the one-time notification when it
/// tips the version into a spike.
pub fn note(server: &Server, run: &AgentRun, kind: Kind) {
    let version = run
        .harness_version
        .clone()
        .unwrap_or_else(|| "unknown".into());
    let k = key(&run.harness, &version);
    let (spike, persist): (Option<Counts>, Option<String>) = {
        let mut g = COUNTS.lock().unwrap();
        if !g.contains_key(&k)
            && let Some(Some(c)) = Some(
                server
                    .with_core(|c| c.store.kv_get(KV_SCOPE, &k).ok().flatten())
                    .and_then(|s| serde_json::from_str::<Counts>(&s).ok()),
            )
        {
            g.insert(k.clone(), c);
        }
        let e = g.entry(k.clone()).or_default();
        match kind {
            Kind::Observation => e.observations += 1,
            Kind::Disagreement => e.disagreements += 1,
            Kind::UnknownResolution => e.unknown_resolutions += 1,
            Kind::AnswerFailure => e.answer_failures += 1,
        }
        let baseline = baseline_rate(&g, &run.harness, &version);
        let e = g.get_mut(&k).expect("just inserted");
        let spike = is_spike(e, baseline);
        if spike {
            e.notified = true;
        }
        let persist = (spike || e.observations.is_multiple_of(10) || kind != Kind::Observation)
            .then(|| serde_json::to_string(e).unwrap_or_default());
        (spike.then(|| e.clone()), persist)
    };
    if let Some(json) = persist {
        server.with_core(|c| {
            let mut tx = Tx::new();
            tx.m.kv(KV_SCOPE, &k, Some(json));
            let _ = c.commit(tx);
        });
    }
    if let Some(c) = spike {
        raise(server, run, &version, &c);
    }
}

fn baseline_rate(g: &BTreeMap<String, Counts>, harness: &str, version: &str) -> Option<f64> {
    let me = key(harness, version);
    g.iter()
        .filter(|(k, c)| {
            k.starts_with(&format!("{harness}@")) && **k != me && c.observations >= MIN_OBSERVATIONS
        })
        .map(|(_, c)| c.rate())
        .min_by(|a, b| a.total_cmp(b))
}

fn raise(server: &Server, run: &AgentRun, version: &str, c: &Counts) {
    let name = super::harness::Harness::from_id(&run.harness)
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| run.harness.clone());
    let title = format!("{name} {version} behaves differently than Vibeke expects");
    let body = format!(
        "exact-state may be degraded. `vibeke integration doctor {}`",
        run.harness
    );
    update_run(server, &run.id, |r, tx| {
        tx.event(
            "agent.drift_detected",
            json!({"run": r.id, "pane": r.pane}),
            json!({"harness": run.harness, "version": version, "observations": c.observations, "disagreements": c.disagreements, "unknown_resolutions": c.unknown_resolutions, "answer_failures": c.answer_failures, "rate": c.rate()}),
        );
    });
    server.notify("system", Some(&run.pane), &title, &body, "normal");
}

/// A lower-precedence transport named a different state than the structured one: an event for
/// the log and a count for drift detection.
pub fn disagreement(
    server: &Server,
    run: &AgentRun,
    facet: &str,
    structured: &str,
    other: &str,
    source: &str,
) {
    // The screen is evaluated up to 10 times a second: one episode (the same pair) is one
    // event and one count, re-reported at most once a minute while it persists.
    {
        let key = format!("{facet}|{structured}|{other}|{source}");
        let mut g = LAST_DISAGREEMENT.lock().unwrap();
        match g.get(&run.id) {
            Some((k, at)) if *k == key && at.elapsed() < Duration::from_secs(60) => return,
            _ => {
                g.insert(run.id.clone(), (key, Instant::now()));
            }
        }
    }
    update_run(server, &run.id, |r, tx| {
        tx.event(
            "adapter.disagreement",
            json!({"run": r.id}),
            json!({"facet": facet, "structured": structured, "other": other, "source": source}),
        );
    });
    note(server, run, Kind::Disagreement);
}

/// Counters for `agent.drift`: every harness version seen, newest activity unordered.
pub fn snapshot() -> Vec<Value> {
    let g = COUNTS.lock().unwrap();
    g.iter()
        .map(|(k, c)| {
            let (h, v) = k.split_once('@').unwrap_or((k, ""));
            json!({
                "harness": h, "version": v,
                "observations": c.observations, "disagreements": c.disagreements,
                "unknown_resolutions": c.unknown_resolutions, "answer_failures": c.answer_failures,
                "rate": c.rate(), "drifting": c.notified,
            })
        })
        .collect()
}

/// Load persisted counters for `agent.drift` after a restart.
pub fn load_persisted(server: &Server) {
    let versions: Vec<(String, String)> = server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter_map(|r| r.harness_version.clone().map(|v| (r.harness.clone(), v)))
            .collect()
    });
    let mut g = COUNTS.lock().unwrap();
    for (h, v) in versions {
        let k = key(&h, &v);
        if g.contains_key(&k) {
            continue;
        }
        if let Some(c) = server
            .with_core(|c| c.store.kv_get(KV_SCOPE, &k).ok().flatten())
            .and_then(|s| serde_json::from_str::<Counts>(&s).ok())
        {
            g.insert(k, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facet(v: Execution, src: StateSource) -> Facet<Execution> {
        Facet {
            value: v,
            since_ms: 0,
            source: src,
            confidence: 1.0,
            detail: None,
        }
    }

    const LIVE: Ctx = Ctx {
        higher_alive: true,
        unvalidated: false,
    };

    #[test]
    fn process_death_overrides_everything_at_once() {
        let cur = facet(Execution::Working, StateSource::Structured);
        assert_eq!(
            judge(&cur, &Execution::Exited, StateSource::Process, 0.2, LIVE),
            Verdict::Apply {
                confidence: 1.0,
                inferred: false
            }
        );
        // …and an exited run never comes back from a screen or self-report signal.
        let dead = facet(Execution::Exited, StateSource::Process);
        assert_eq!(
            judge(&dead, &Execution::Working, StateSource::Screen, 0.9, LIVE),
            Verdict::Keep { disagrees: false }
        );
    }

    #[test]
    fn silence_is_not_staleness_lower_sources_only_add_information() {
        let cur = facet(Execution::Working, StateSource::Structured);
        // A screen `idle` while structured says working changes nothing, but is counted.
        assert_eq!(
            judge(&cur, &Execution::Idle, StateSource::Screen, 0.9, LIVE),
            Verdict::Keep { disagrees: true }
        );
        // Agreement is not a disagreement.
        assert_eq!(
            judge(&cur, &Execution::Working, StateSource::Screen, 0.9, LIVE),
            Verdict::Keep { disagrees: false }
        );
        assert_eq!(
            judge(&cur, &Execution::Idle, StateSource::SelfReport, 0.9, LIVE),
            Verdict::Keep { disagrees: true }
        );
    }

    #[test]
    fn precedence_picks_structured_over_self_report_over_screen_over_process() {
        assert!(precedence(StateSource::Structured) > precedence(StateSource::SelfReport));
        assert!(precedence(StateSource::SelfReport) > precedence(StateSource::Screen));
        assert!(precedence(StateSource::Screen) > precedence(StateSource::Process));
        let cur = facet(Execution::Idle, StateSource::Screen);
        let r = judge(
            &cur,
            &Execution::Working,
            StateSource::Structured,
            1.0,
            Ctx {
                higher_alive: false,
                unvalidated: false,
            },
        );
        assert_eq!(
            r,
            Verdict::Apply {
                confidence: 1.0,
                inferred: false
            }
        );
    }

    #[test]
    fn structured_loss_downgrades_to_the_next_transport_marked_inferred() {
        let cur = facet(Execution::Working, StateSource::Structured);
        let gone = Ctx {
            higher_alive: false,
            unvalidated: false,
        };
        match judge(&cur, &Execution::Idle, StateSource::Screen, 0.9, gone) {
            Verdict::Apply {
                confidence,
                inferred,
            } => {
                assert!(inferred);
                assert!(confidence <= 0.7);
            }
            v => panic!("{v:?}"),
        }
        // fallback order: self-report, then screen, then process.
        assert_eq!(
            fallback(
                Some((Execution::Working, 0.9)),
                Some((Execution::Idle, 0.9)),
                true
            )
            .1,
            StateSource::SelfReport
        );
        assert_eq!(
            fallback(None, Some((Execution::Idle, 0.9)), true),
            (Execution::Idle, StateSource::Screen, 0.7)
        );
        assert_eq!(
            fallback(None, None, true),
            (Execution::Unknown, StateSource::Process, 0.3)
        );
        assert_eq!(fallback(None, None, false).0, Execution::Exited);
        assert!(is_inferred(Some("inferred")) && is_inferred(Some("inferred:transcript")));
        assert!(!is_inferred(Some("compacting")) && !is_inferred(None));
    }

    #[test]
    fn unvalidated_versions_cap_confidence_at_point_eight() {
        let cur = facet(Execution::Idle, StateSource::Structured);
        let ctx = Ctx {
            higher_alive: true,
            unvalidated: true,
        };
        assert_eq!(
            judge(&cur, &Execution::Working, StateSource::Structured, 1.0, ctx),
            Verdict::Apply {
                confidence: 0.8,
                inferred: false
            }
        );
        let gone = Ctx {
            higher_alive: false,
            unvalidated: true,
        };
        match judge(&cur, &Execution::Working, StateSource::Screen, 0.99, gone) {
            Verdict::Apply { confidence, .. } => assert!(confidence <= 0.7),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn ctx_reflects_health_and_source() {
        let run = AgentRun {
            execution: facet(Execution::Working, StateSource::Structured),
            health: AdapterHealth::Disconnected,
            ..super::super::harness_tests_run()
        };
        assert!(!ctx_of(&run).higher_alive);
        let run = AgentRun {
            health: AdapterHealth::UnvalidatedVersion,
            ..run
        };
        let c = ctx_of(&run);
        assert!(c.higher_alive && c.unvalidated);
    }

    #[test]
    fn spike_needs_enough_observations_and_fires_once() {
        let mut c = Counts {
            observations: 10,
            disagreements: 9,
            ..Default::default()
        };
        assert!(!is_spike(&c, None), "too few observations");
        c.observations = 40;
        c.disagreements = 12;
        assert!(is_spike(&c, None), "30% is above the absolute threshold");
        c.notified = true;
        assert!(!is_spike(&c, None), "one-time");
        // Relative: a quiet baseline makes a modest rate suspicious.
        let c = Counts {
            observations: 50,
            disagreements: 6,
            ..Default::default()
        };
        assert!(!is_spike(&c, None));
        assert!(is_spike(&c, Some(0.02)));
        assert!(!is_spike(&c, Some(0.10)));
        let calm = Counts {
            observations: 100,
            disagreements: 2,
            ..Default::default()
        };
        assert!(!is_spike(&calm, Some(0.0)), "below the relative floor");
        assert!(resolution_known("turn ended") && !resolution_known("mystery"));
    }
}
