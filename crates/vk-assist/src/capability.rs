//! Capability records (14 §5.2): what a model is believed to support, with provenance.
//!
//! Each feature is `supported | unsupported | unknown`, recorded with its **source** and a
//! verification date. Model existence/access and capabilities are separate questions: a
//! manually entered model starts with everything but plain text `unknown`, and nothing is
//! assumed from a model name. Sources, weakest to strongest:
//!
//! - `bundled`: what Vibeke ships. It never claims more than plain text and is never a
//!   verification (no adapter has been exercised against a live provider).
//! - `listing`: the provider's own model listing said so.
//! - `user`: the profile's `capabilities` table (`json_schema = "supported"`).
//! - `observed`: Vibeke saw it work (a stream finished, a native-schema reply validated) or
//!   fail (the provider rejected the request) on this connection and model.
//!
//! Native structured output and streaming are used only for `supported` records; everything
//! else takes the portable path (JSON text validated locally, one non-streamed response).

use crate::config::{Adapter, Resolved};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}

impl Support {
    pub fn as_str(self) -> &'static str {
        match self {
            Support::Supported => "supported",
            Support::Unsupported => "unsupported",
            Support::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Bundled,
    Listing,
    User,
    Observed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Text,
    Streaming,
    JsonSchema,
    Tools,
    Images,
}

pub const FEATURES: &[Feature] = &[
    Feature::Text,
    Feature::Streaming,
    Feature::JsonSchema,
    Feature::Tools,
    Feature::Images,
];

impl Feature {
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::Text => "text",
            Feature::Streaming => "streaming",
            Feature::JsonSchema => "json_schema",
            Feature::Tools => "tools",
            Feature::Images => "images",
        }
    }
    pub fn parse(s: &str) -> Option<Feature> {
        FEATURES.iter().copied().find(|f| f.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub support: Support,
    pub source: Source,
    /// `YYYY-MM-DD` (UTC) of the observation or declaration; `None` for bundled records.
    pub verified_on: Option<String>,
    pub note: Option<String>,
}

impl Capability {
    fn bundled(support: Support, note: &str) -> Capability {
        Capability {
            support,
            source: Source::Bundled,
            verified_on: None,
            note: Some(note.to_string()),
        }
    }
    /// True when the feature may be used on the wire.
    pub fn usable(&self) -> bool {
        self.support == Support::Supported
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub text: Capability,
    pub streaming: Capability,
    pub json_schema: Capability,
    pub tools: Capability,
    pub images: Capability,
}

const UNVERIFIED: &str = "not verified against a live provider";

impl Capabilities {
    /// The bundled starting point: plain text works for every adapter; the rest is `unknown`.
    pub fn bundled(_adapter: Adapter) -> Capabilities {
        let unknown = || Capability::bundled(Support::Unknown, UNVERIFIED);
        Capabilities {
            text: Capability::bundled(Support::Supported, "every adapter generates text"),
            streaming: unknown(),
            json_schema: unknown(),
            tools: unknown(),
            images: unknown(),
        }
    }

    pub fn get(&self, f: Feature) -> &Capability {
        match f {
            Feature::Text => &self.text,
            Feature::Streaming => &self.streaming,
            Feature::JsonSchema => &self.json_schema,
            Feature::Tools => &self.tools,
            Feature::Images => &self.images,
        }
    }

    fn slot(&mut self, f: Feature) -> &mut Capability {
        match f {
            Feature::Text => &mut self.text,
            Feature::Streaming => &mut self.streaming,
            Feature::JsonSchema => &mut self.json_schema,
            Feature::Tools => &mut self.tools,
            Feature::Images => &mut self.images,
        }
    }

    pub fn set(&mut self, f: Feature, c: Capability) {
        *self.slot(f) = c;
    }

    pub fn usable(&self, f: Feature) -> bool {
        self.get(f).usable()
    }

    /// Overlay another record set: every non-`unknown` feature of `over` wins.
    pub fn overlay(&mut self, over: &Capabilities) {
        for f in FEATURES {
            let c = over.get(*f);
            if c.support != Support::Unknown {
                self.set(*f, c.clone());
            }
        }
    }

    /// Apply the profile's declared `capabilities` table (source `user`). Unknown feature
    /// names or values are ignored here (config validation reports them).
    pub fn apply_user(&mut self, declared: &BTreeMap<String, Support>, now_ms: i64) {
        for (name, support) in declared {
            if let Some(f) = Feature::parse(name) {
                self.set(
                    f,
                    Capability {
                        support: *support,
                        source: Source::User,
                        verified_on: Some(utc_date(now_ms)),
                        note: Some("declared in the profile's capabilities".into()),
                    },
                );
            }
        }
    }
}

/// Records observed per connection and model. The key binds the adapter and endpoint
/// fingerprint, so an endpoint change starts from `unknown` again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Records {
    pub entries: BTreeMap<String, Capabilities>,
}

pub fn record_key(r: &Resolved) -> String {
    key_for(&r.connection_id, &r.fingerprint, &r.profile.model)
}

/// The record key for an arbitrary model on a resolved connection.
pub fn key_for(connection_id: &str, fingerprint: &str, model: &str) -> String {
    format!("{connection_id}|{fingerprint}|{model}")
}

impl Records {
    /// Record an observation for `r`'s connection and model.
    pub fn observe(&mut self, r: &Resolved, f: Feature, support: Support, note: &str, now_ms: i64) {
        let e = self
            .entries
            .entry(record_key(r))
            .or_insert_with(|| Capabilities::bundled(r.connection.adapter));
        e.set(
            f,
            Capability {
                support,
                source: Source::Observed,
                verified_on: Some(utc_date(now_ms)),
                note: Some(note.to_string()),
            },
        );
    }

    /// Record what the provider's model listing says about a model (source `listing`).
    pub fn from_listing(&mut self, r: &Resolved, f: Feature, support: Support, now_ms: i64) {
        let e = self
            .entries
            .entry(record_key(r))
            .or_insert_with(|| Capabilities::bundled(r.connection.adapter));
        // Never downgrade a stronger source.
        if matches!(e.get(f).source, Source::Observed | Source::User) {
            return;
        }
        e.set(
            f,
            Capability {
                support,
                source: Source::Listing,
                verified_on: Some(utc_date(now_ms)),
                note: Some("reported by the provider's model listing".into()),
            },
        );
    }

    /// Bundled, then observed/listed records, then the profile's declarations.
    pub fn effective(&self, r: &Resolved, now_ms: i64) -> Capabilities {
        let mut c = self.recorded(r, &r.profile.model);
        c.apply_user(&r.profile.capabilities, now_ms);
        c
    }

    /// Bundled plus observed/listed records for any model on `r`'s connection (no profile
    /// declarations: those belong to the profile's own model).
    pub fn recorded(&self, r: &Resolved, model: &str) -> Capabilities {
        let mut c = Capabilities::bundled(r.connection.adapter);
        if let Some(rec) = self
            .entries
            .get(&key_for(&r.connection_id, &r.fingerprint, model))
        {
            c.overlay(rec);
        }
        c
    }

    /// Like [`Records::from_listing`] for an arbitrary model on `r`'s connection.
    pub fn from_listing_for(
        &mut self,
        r: &Resolved,
        model: &str,
        f: Feature,
        support: Support,
        now_ms: i64,
    ) {
        let e = self
            .entries
            .entry(key_for(&r.connection_id, &r.fingerprint, model))
            .or_insert_with(|| Capabilities::bundled(r.connection.adapter));
        if matches!(e.get(f).source, Source::Observed | Source::User) {
            return;
        }
        e.set(
            f,
            Capability {
                support,
                source: Source::Listing,
                verified_on: Some(utc_date(now_ms)),
                note: Some("reported by the provider's model listing".into()),
            },
        );
    }

    pub fn forget_connection(&mut self, connection: &str) {
        self.entries
            .retain(|k, _| !k.starts_with(&format!("{connection}|")));
    }
}

/// `YYYY-MM-DD` (UTC) for a unix-millisecond timestamp.
pub fn utc_date(now_ms: i64) -> String {
    let days = now_ms.div_euclid(86_400_000);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AssistConfig;
    use serde_json::json;

    fn resolved(model: &str, caps: serde_json::Value) -> Resolved {
        AssistConfig::from_json(json!({
            "connections": {"c": {"adapter": "ollama", "endpoint": "http://127.0.0.1:9"}},
            "profiles": {"interactive": {"connection": "c", "model": model, "capabilities": caps}},
        }))
        .unwrap()
        .resolve(None)
        .unwrap()
    }

    #[test]
    fn dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_791_244_800_000), "2026-10-06");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(-86_400_000), "1969-12-31");
    }

    #[test]
    fn a_manual_model_starts_with_unknown_capabilities() {
        let r = resolved("my-local-model:7b", json!({}));
        let c = Records::default().effective(&r, 0);
        assert!(c.usable(Feature::Text));
        for f in [
            Feature::Streaming,
            Feature::JsonSchema,
            Feature::Tools,
            Feature::Images,
        ] {
            assert_eq!(c.get(f).support, Support::Unknown, "{f:?}");
            assert_eq!(c.get(f).source, Source::Bundled);
            assert!(c.get(f).verified_on.is_none());
            assert!(!c.usable(f));
        }
    }

    #[test]
    fn observations_and_declarations_override_in_order() {
        let r = resolved("m", json!({}));
        let mut rec = Records::default();
        rec.observe(
            &r,
            Feature::Streaming,
            Support::Supported,
            "stream finished",
            1_791_244_800_000,
        );
        let c = rec.effective(&r, 0);
        assert!(c.usable(Feature::Streaming));
        assert_eq!(c.streaming.source, Source::Observed);
        assert_eq!(c.streaming.verified_on.as_deref(), Some("2026-10-06"));

        // A listing never replaces an observation.
        rec.from_listing(&r, Feature::Streaming, Support::Unsupported, 0);
        assert!(rec.effective(&r, 0).usable(Feature::Streaming));

        // The user's declaration wins over everything.
        let r2 = resolved(
            "m",
            json!({"streaming": "unsupported", "json_schema": "supported"}),
        );
        let mut rec2 = Records::default();
        rec2.observe(&r2, Feature::Streaming, Support::Supported, "x", 0);
        let c2 = rec2.effective(&r2, 0);
        assert_eq!(c2.streaming.support, Support::Unsupported);
        assert_eq!(c2.streaming.source, Source::User);
        assert!(c2.usable(Feature::JsonSchema));
    }

    #[test]
    fn records_are_bound_to_connection_endpoint_and_model() {
        let r = resolved("m", json!({}));
        let mut rec = Records::default();
        rec.observe(&r, Feature::JsonSchema, Support::Supported, "ok", 0);
        assert!(rec.effective(&r, 0).usable(Feature::JsonSchema));
        let other_model = resolved("m2", json!({}));
        assert!(!rec.effective(&other_model, 0).usable(Feature::JsonSchema));
        let other_endpoint = AssistConfig::from_json(json!({
            "connections": {"c": {"adapter": "ollama", "endpoint": "http://127.0.0.1:10"}},
            "profiles": {"interactive": {"connection": "c", "model": "m"}},
        }))
        .unwrap()
        .resolve(None)
        .unwrap();
        assert!(
            !rec.effective(&other_endpoint, 0)
                .usable(Feature::JsonSchema)
        );
        rec.forget_connection("c");
        assert!(rec.entries.is_empty());
    }
}
