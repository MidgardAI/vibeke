//! Vibeke's own LLM assistance (spec 14), server-independent parts.
//!
//! Everything here produces **drafts**: text the user reads, edits and confirms through the
//! ordinary APIs. Nothing in this crate can reach a PTY, holder, interaction, check runner or
//! any other Vibeke mutation; it only formats context, talks to the configured provider and
//! validates the reply against a fixed per-operation schema (unknown fields are dropped).
//!
//! - [`config`]: `[assistant]` settings (off by default), connections, profiles, credential
//!   references (env var or a user-created 0600 file, never harness credential stores),
//!   endpoint rules (HTTPS unless loopback) and pricing.
//! - [`consent`]: per-workspace grants (user-level file), bound to the connection fingerprint.
//! - [`context`]: deterministic, redacted, bounded context packages with source IDs and the
//!   exact payload preview.
//! - [`ops`]: the named operations, their prompts and output validation.
//! - [`provider`]: Anthropic Messages, OpenAI-compatible chat completions and Ollama chat.
//! - [`budget`]: per-UTC-day request/token/cost ledger and a per-minute rate window.
//! - [`fake`]: a local fake HTTP server for tests (no real provider is ever contacted).

pub mod budget;
pub mod config;
pub mod consent;
pub mod context;
pub mod fake;
pub mod ops;
pub mod provider;

use serde::{Deserialize, Serialize};

/// Sanitized error categories (14 §8). Never carries provider bodies or prompt text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Disabled,
    NotConfigured,
    PermissionDenied,
    UnsupportedCapability,
    ContextTooLarge,
    QueueFull,
    BudgetExhausted,
    AuthenticationFailed,
    RateLimited,
    ProviderUnavailable,
    InvalidOutput,
    Timeout,
    Cancelled,
    Interrupted,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Disabled => "disabled",
            Category::NotConfigured => "not_configured",
            Category::PermissionDenied => "permission_denied",
            Category::UnsupportedCapability => "unsupported_capability",
            Category::ContextTooLarge => "context_too_large",
            Category::QueueFull => "queue_full",
            Category::BudgetExhausted => "budget_exhausted",
            Category::AuthenticationFailed => "authentication_failed",
            Category::RateLimited => "rate_limited",
            Category::ProviderUnavailable => "provider_unavailable",
            Category::InvalidOutput => "invalid_output",
            Category::Timeout => "timeout",
            Category::Cancelled => "cancelled",
            Category::Interrupted => "interrupted",
        }
    }
}

/// An error with a category and a short, content-free message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistError {
    pub category: Category,
    pub message: String,
}

impl AssistError {
    pub fn new(category: Category, message: impl Into<String>) -> Self {
        AssistError {
            category,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AssistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.category.as_str(), self.message)
    }
}

impl std::error::Error for AssistError {}

pub type Result<T> = std::result::Result<T, AssistError>;

/// Strip C0/C1 controls (except newline/tab) and bidi overrides from untrusted model text
/// before it is stored or rendered (14 §10, 09 §6).
pub fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            if c == '\n' || c == '\t' {
                return true;
            }
            if c.is_control() {
                return false;
            }
            !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
        })
        .collect()
}

/// Truncate to at most `max` bytes on a char boundary.
pub fn clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_and_bidi() {
        assert_eq!(sanitize("a\x1b[31mb\u{202E}c\nd"), "a[31mbc\nd");
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("æøå", 3), ("æ".to_string(), true));
        assert_eq!(clip("abc", 5), ("abc".to_string(), false));
    }
}
