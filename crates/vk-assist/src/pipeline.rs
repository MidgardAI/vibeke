//! One request's provider work (14 §8): the call (optionally streamed, optionally with native
//! structured output), local validation and **at most one bounded repair attempt**.
//!
//! - Native schema mode and streaming are used only when the caller passes them (it does so for
//!   capabilities recorded `supported`); every reply is validated by Vibeke either way.
//! - A reply the provider flagged truncated or refused never reaches validation and is never
//!   repaired: it is `invalid_output` immediately.
//! - A reply that fails validation gets exactly one repair attempt: the original request plus
//!   the rejected reply and the content-free reason. The repair is a provider attempt like any
//!   other: it passes the caller's gate (budget, rate window, consent) and its usage is summed.
//!   It is never streamed and never retried; a second invalid reply fails the request.
//! - A provider rejection of native structured output (HTTP 400/422 with a schema on the wire)
//!   is reported as `unsupported_capability` and flagged, so the caller can record the
//!   capability as unsupported; nothing is silently re-sent in another mode.

use crate::config::Resolved;
use crate::context::Payload;
use crate::ops::{self, Operation};
use crate::provider::{self, Mode, Usage};
use crate::{AssistError, Category};
use serde_json::Value;
use std::time::Instant;

/// Which provider attempt a gate call is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    /// 1-based over the whole request (first call, its retry, the repair).
    pub n: u32,
    pub repair: bool,
}

pub struct Job<'a> {
    pub resolved: &'a Resolved,
    pub key: Option<&'a str>,
    pub payload: &'a Payload,
    pub deadline: Instant,
    pub op: Operation,
    pub sources: &'a [String],
    pub targets: &'a [String],
    pub mode: Mode,
    /// Allow the one repair attempt (default true).
    pub repair: bool,
}

#[derive(Debug)]
pub struct Out {
    pub result: Result<Value, AssistError>,
    pub usage: Usage,
    pub attempts: u32,
    pub finish_reason: Option<String>,
    /// The reply came over a completed stream.
    pub streamed: bool,
    /// A native schema was on the wire and the reply validated without repair.
    pub native_ok: bool,
    /// The provider rejected the request while a native schema was on the wire.
    pub native_rejected: bool,
    pub repaired: bool,
}

pub async fn run(
    job: Job<'_>,
    mut gate: impl FnMut(Attempt) -> Result<(), AssistError>,
    sink: Option<&mut (dyn FnMut(&str) + Send)>,
) -> Out {
    let native = job.mode.native_schema.is_some();
    let o1 = provider::generate_with(
        job.resolved,
        job.key,
        job.payload,
        job.deadline,
        &job.mode,
        |n| gate(Attempt { n, repair: false }),
        sink,
    )
    .await;
    let mut out = Out {
        result: Err(AssistError::new(
            Category::ProviderUnavailable,
            "not attempted",
        )),
        usage: o1.usage,
        attempts: o1.attempts,
        finish_reason: o1.finish_reason.clone(),
        streamed: o1.streamed,
        native_ok: false,
        native_rejected: false,
        repaired: false,
    };
    let text = match o1.result {
        Ok(t) => t,
        Err(e) => {
            if native && matches!(o1.status, Some(400 | 422)) {
                out.native_rejected = true;
                out.result = Err(AssistError::new(
                    Category::UnsupportedCapability,
                    "the provider rejected native structured output; the capability is now recorded as unsupported, retry to use JSON text",
                ));
            } else {
                out.result = Err(e);
            }
            return out;
        }
    };
    let first = ops::validate(job.op, &text, job.sources, job.targets);
    let problem = match first {
        Ok(v) => {
            out.native_ok = native;
            out.result = Ok(v);
            return out;
        }
        Err(e) if e.category == Category::InvalidOutput && job.repair => e,
        Err(e) => {
            out.result = Err(e);
            return out;
        }
    };
    // One bounded repair attempt, counted against the limits through the gate.
    let base = out.attempts;
    let mut repair = job.payload.clone();
    repair.user = ops::repair_user_message(&job.payload.user, &text, &problem.message);
    let rmode = Mode {
        native_schema: job.mode.native_schema.clone(),
        stream: false,
        no_retry: true,
    };
    let o2 = provider::generate_with(
        job.resolved,
        job.key,
        &repair,
        job.deadline,
        &rmode,
        |n| {
            gate(Attempt {
                n: base + n,
                repair: true,
            })
        },
        None,
    )
    .await;
    out.repaired = o2.attempts > 0;
    out.attempts += o2.attempts;
    out.usage.add(o2.usage);
    out.finish_reason = o2.finish_reason.clone().or(out.finish_reason);
    out.result = match o2.result {
        Ok(t2) => ops::validate(job.op, &t2, job.sources, job.targets),
        // The repair's own failure (budget refusal, timeout, provider error) is the result;
        // the first reply stays rejected.
        Err(e) => Err(e),
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AssistConfig;
    use crate::fake::{FakeServer, Reply};
    use serde_json::json;
    use std::time::Duration;

    fn resolved(adapter: &str, endpoint: &str) -> Resolved {
        AssistConfig::from_json(json!({
            "connections": {"c": {"adapter": adapter, "endpoint": endpoint, "credential": {"env": "X"}}},
            "profiles": {"interactive": {"connection": "c", "model": "m-1"}},
        }))
        .unwrap()
        .resolve(None)
        .unwrap()
    }

    fn payload() -> Payload {
        Payload {
            adapter: "x".into(),
            model: "m-1".into(),
            max_output_tokens: 100,
            system: "sys".into(),
            user: "USER-REQUEST".into(),
        }
    }

    fn job<'a>(r: &'a Resolved, p: &'a Payload, mode: Mode) -> Job<'a> {
        Job {
            resolved: r,
            key: Some("k"),
            payload: p,
            deadline: Instant::now() + Duration::from_secs(10),
            op: Operation::PaneTitle,
            sources: &[],
            targets: &[],
            mode,
            repair: true,
        }
    }

    const GOOD: &str = r#"{"title":"Fix login"}"#;
    const BAD: &str = r#"{"name":"no title field"}"#;

    #[tokio::test]
    async fn invalid_output_gets_exactly_one_repair_attempt() {
        let f = FakeServer::start(vec![
            Reply::anthropic(BAD, 10, 4),
            Reply::anthropic(GOOD, 15, 5),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let p = payload();
        let mut gates = vec![];
        let o = run(
            job(&r, &p, Mode::default()),
            |a| {
                gates.push(a);
                Ok(())
            },
            None,
        )
        .await;
        assert_eq!(o.result.unwrap()["title"], "Fix login");
        assert!(o.repaired);
        assert_eq!(o.attempts, 2);
        assert_eq!(o.usage.input_tokens, Some(25));
        assert_eq!(o.usage.output_tokens, Some(9));
        assert_eq!(
            gates,
            vec![
                Attempt {
                    n: 1,
                    repair: false
                },
                Attempt { n: 2, repair: true }
            ]
        );
        let second = f.requests()[1].json();
        let user = second["messages"][0]["content"].as_str().unwrap();
        assert!(user.starts_with("USER-REQUEST"));
        assert!(user.contains("<previous_reply>") && user.contains("no title field"));
        assert!(user.contains("missing or not a string"));
        assert_eq!(f.count(), 2);
    }

    #[tokio::test]
    async fn a_second_invalid_reply_fails_and_nothing_is_attempted_a_third_time() {
        let f = FakeServer::start(vec![
            Reply::anthropic(BAD, 10, 4),
            Reply::anthropic(BAD, 10, 4),
            Reply::anthropic(GOOD, 10, 4),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let p = payload();
        let o = run(job(&r, &p, Mode::default()), |_| Ok(()), None).await;
        assert_eq!(o.result.unwrap_err().category, Category::InvalidOutput);
        assert_eq!(o.attempts, 2);
        assert_eq!(f.count(), 2, "the repair is bounded to one attempt");
    }

    #[tokio::test]
    async fn truncated_and_refused_replies_are_not_repaired() {
        for stop in ["max_tokens", "refusal"] {
            let f = FakeServer::start(vec![
                Reply::anthropic_stop("{\"title\":\"x", stop),
                Reply::anthropic(GOOD, 1, 1),
            ])
            .await;
            let r = resolved("anthropic", &f.url());
            let p = payload();
            let o = run(job(&r, &p, Mode::default()), |_| Ok(()), None).await;
            assert_eq!(o.result.unwrap_err().category, Category::InvalidOutput);
            assert!(!o.repaired);
            assert_eq!(f.count(), 1, "{stop}");
        }
    }

    #[tokio::test]
    async fn the_repair_passes_the_gate_and_a_refusal_stops_it() {
        let f = FakeServer::start(vec![
            Reply::anthropic(BAD, 10, 4),
            Reply::anthropic(GOOD, 10, 4),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let p = payload();
        let o = run(
            job(&r, &p, Mode::default()),
            |a| {
                if a.repair {
                    Err(AssistError::new(
                        Category::BudgetExhausted,
                        "the request limit is used up",
                    ))
                } else {
                    Ok(())
                }
            },
            None,
        )
        .await;
        assert_eq!(o.result.unwrap_err().category, Category::BudgetExhausted);
        assert_eq!(o.attempts, 1);
        assert!(!o.repaired);
        assert_eq!(f.count(), 1, "a refused repair is never sent");
    }

    #[tokio::test]
    async fn repair_can_be_switched_off_and_is_never_retried_on_429() {
        let f = FakeServer::start(vec![Reply::anthropic(BAD, 1, 1)]).await;
        let r = resolved("anthropic", &f.url());
        let p = payload();
        let mut j = job(&r, &p, Mode::default());
        j.repair = false;
        let o = run(j, |_| Ok(()), None).await;
        assert_eq!(o.result.unwrap_err().category, Category::InvalidOutput);
        assert_eq!(f.count(), 1);
        let f = FakeServer::start(vec![
            Reply::anthropic(BAD, 1, 1),
            Reply::status(429, "").with_header("retry-after", "0"),
            Reply::anthropic(GOOD, 1, 1),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let o = run(job(&r, &p, Mode::default()), |_| Ok(()), None).await;
        assert_eq!(o.result.unwrap_err().category, Category::RateLimited);
        assert_eq!(f.count(), 2, "the repair attempt itself is not retried");
    }

    #[tokio::test]
    async fn native_schema_replies_validate_and_are_flagged() {
        let f = FakeServer::start(vec![Reply::anthropic_tool(
            json!({"title": "Native"}),
            5,
            5,
        )])
        .await;
        let r = resolved("anthropic", &f.url());
        let p = payload();
        let o = run(
            job(
                &r,
                &p,
                Mode {
                    native_schema: Some(Operation::PaneTitle.json_schema()),
                    ..Default::default()
                },
            ),
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.native_ok);
        assert_eq!(o.result.unwrap()["title"], "Native");
        // Even a native reply is validated: an invented field value fails like text would.
        let f = FakeServer::start(vec![
            Reply::anthropic_tool(json!({"nothing": 1}), 5, 5),
            Reply::anthropic_tool(json!({"title": "Second"}), 5, 5),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let o = run(
            job(
                &r,
                &p,
                Mode {
                    native_schema: Some(Operation::PaneTitle.json_schema()),
                    ..Default::default()
                },
            ),
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.repaired && !o.native_ok);
        assert_eq!(o.result.unwrap()["title"], "Second");
    }

    #[tokio::test]
    async fn a_provider_rejecting_the_schema_is_flagged_not_silently_resent() {
        let f = FakeServer::start(vec![
            Reply::status(400, "{\"error\":\"schema unsupported\"}"),
            Reply::anthropic(GOOD, 1, 1),
        ])
        .await;
        let r = resolved("openai_compatible", &f.url());
        let p = payload();
        let o = run(
            job(
                &r,
                &p,
                Mode {
                    native_schema: Some(Operation::PaneTitle.json_schema()),
                    ..Default::default()
                },
            ),
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.native_rejected);
        assert_eq!(
            o.result.unwrap_err().category,
            Category::UnsupportedCapability
        );
        assert_eq!(f.count(), 1, "no automatic re-send in another mode");
        // A 400 without a schema on the wire stays an ordinary rejection.
        let f = FakeServer::start(vec![Reply::status(400, "x")]).await;
        let r = resolved("openai_compatible", &f.url());
        let o = run(job(&r, &p, Mode::default()), |_| Ok(()), None).await;
        assert!(!o.native_rejected);
        assert_eq!(o.result.unwrap_err().category, Category::NotConfigured);
    }

    #[tokio::test]
    async fn streamed_replies_report_deltas_and_validate() {
        let f = FakeServer::start(vec![Reply::openai_stream(
            &["{\"title\":", "\"Streamed\"}"],
            8,
            6,
        )])
        .await;
        let r = resolved("openai_compatible", &f.url());
        let p = payload();
        let mut deltas = vec![];
        let mut sink = |t: &str| deltas.push(t.to_string());
        let o = run(
            job(
                &r,
                &p,
                Mode {
                    stream: true,
                    ..Default::default()
                },
            ),
            |_| Ok(()),
            Some(&mut sink),
        )
        .await;
        assert!(o.streamed);
        assert_eq!(deltas.concat(), "{\"title\":\"Streamed\"}");
        assert_eq!(o.result.unwrap()["title"], "Streamed");
        assert_eq!(o.usage.total(), 14);
    }

    #[tokio::test]
    async fn repair_after_a_streamed_reply_is_not_streamed() {
        let f = FakeServer::start(vec![
            Reply::openai_stream(&["{\"nope\":1}"], 8, 6),
            Reply::openai("{\"title\":\"Fixed\"}", 9, 3),
        ])
        .await;
        let r = resolved("openai_compatible", &f.url());
        let p = payload();
        let mut deltas = vec![];
        let mut sink = |t: &str| deltas.push(t.to_string());
        let o = run(
            job(
                &r,
                &p,
                Mode {
                    stream: true,
                    ..Default::default()
                },
            ),
            |_| Ok(()),
            Some(&mut sink),
        )
        .await;
        assert_eq!(o.result.unwrap()["title"], "Fixed");
        assert_eq!(
            deltas.concat(),
            "{\"nope\":1}",
            "only the first reply streams"
        );
        assert_eq!(f.requests()[1].json().get("stream"), None);
    }
}
