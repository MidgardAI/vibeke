//! Mention names and agent-to-agent messages (spec 19).
//!
//! A mention name (`@reviewer`) points at one pane of a workspace. Names are unique within
//! their workspace and survive agent restarts in the pane. Messages go from one pane (or the
//! human) to named panes; the server delivers them and routes replies back.

use serde::{Deserialize, Serialize};

/// Longest mention name, in bytes (the name rule is ASCII only).
pub const MAX_LEN: usize = 32;

/// Words a mention name may not take: target keywords (`@current`, `@focused`, 07 §1.2) and
/// broadcast-looking words.
pub const RESERVED: &[&str] = &[
    "current", "focused", "all", "everyone", "here", "me", "self", "human", "user", "vibeke",
];

/// Filler words left out of a name built from a label.
const FILLER: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "at", "by", "is",
];

/// A mention name bound to a pane (`SessionModel::mentions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionName {
    /// Without the `@`.
    pub name: String,
    pub workspace: String,
    /// Pane id (ULID).
    pub pane: String,
    /// `agent` or `terminal`. Phase 1 delivers to agents only.
    pub kind: MentionKind,
    /// Off: the name stays reserved, but nothing is delivered to the pane.
    pub delivery: bool,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    #[default]
    Agent,
    Terminal,
}

/// Whether `name` (without `@`) is a valid mention name: `[a-z][a-z0-9_-]{0,31}`, not reserved.
pub fn valid(name: &str) -> bool {
    name.len() <= MAX_LEN
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !RESERVED.contains(&name)
}

/// A name for a pane that has none yet, built from its sidebar label (spec 19 §2.2): the first
/// words of the label, lowercase ASCII, filler words dropped, cut at a word boundary near 20
/// bytes. `fallback` is used when the label gives nothing usable (the harness, or the cwd's
/// base name for a shell); `taken` reports names already used in the workspace, and a
/// `-2`, `-3`… suffix makes the result unique.
pub fn suggest(label: &str, fallback: &str, taken: impl Fn(&str) -> bool) -> String {
    let base = slug(label)
        .or_else(|| slug(fallback))
        .unwrap_or_else(|| "agent".to_string());
    if !taken(&base) {
        return base;
    }
    for n in 2.. {
        let suffix = format!("-{n}");
        let mut stem = base.clone();
        stem.truncate(MAX_LEN - suffix.len());
        let stem = stem.trim_end_matches(['-', '_']);
        let candidate = format!("{stem}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// `label` as a valid name, or `None` when nothing usable is left.
fn slug(label: &str) -> Option<String> {
    let words: Vec<String> = label
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let meaningful: Vec<&String> = words
        .iter()
        .filter(|w| !FILLER.contains(&w.as_str()))
        .collect();
    let words: Vec<&String> = if meaningful.is_empty() {
        words.iter().collect()
    } else {
        meaningful
    };
    let mut out = String::new();
    for w in words.iter().take(3) {
        let next = if out.is_empty() {
            w.to_string()
        } else {
            format!("{out}-{w}")
        };
        if next.len() > 20 && !out.is_empty() {
            break;
        }
        out = next;
    }
    let out = out.trim_start_matches(|c: char| !c.is_ascii_lowercase());
    let mut out = out.to_string();
    out.truncate(MAX_LEN);
    let out = out.trim_end_matches(['-', '_']).to_string();
    (!out.is_empty() && valid(&out)).then_some(out)
}

/// The mentions in a prompt (spec 19 §3.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mentions {
    /// `@name` tokens at the very start of the text (after leading whitespace), in order. A
    /// prompt that starts with mentions is a direct handoff to those names.
    pub leading: Vec<String>,
    /// Other `@name` tokens, in order of first appearance, without duplicates or leading ones.
    pub embedded: Vec<String>,
}

/// Finds `@name` tokens. A token counts when the `@` starts the text or follows whitespace or
/// one of `([{"'`, the name is valid, and it is not followed by `/`, `.`+alphanumeric, `@` or
/// `:`+alphanumeric (paths, emails, `@scope/pkg`, URLs). `\@name` stays literal. Whether a
/// token names a real pane is up to the caller: unknown names stay prose.
pub fn parse(text: &str) -> Mentions {
    let b = text.as_bytes();
    let mut out = Mentions::default();
    let mut leading = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // Commas may separate leading mentions (`@a, @b fix this`).
        if c.is_ascii_whitespace() || (leading && c == b',' && !out.leading.is_empty()) {
            i += 1;
            continue;
        }
        let boundary = i == 0
            || matches!(
                b[i - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b'{' | b'"' | b'\''
            );
        if c == b'@' && boundary {
            let start = i + 1;
            let mut end = start;
            while end < b.len()
                && (b[end].is_ascii_lowercase()
                    || b[end].is_ascii_digit()
                    || matches!(b[end], b'-' | b'_'))
            {
                end += 1;
            }
            // Trailing hyphens and underscores are punctuation, not part of the name.
            let mut name_end = end;
            while name_end > start && matches!(b[name_end - 1], b'-' | b'_') {
                name_end -= 1;
            }
            let name = &text[start..name_end];
            let next = b.get(end).copied();
            let after = b.get(end + 1).copied();
            let continues = match next {
                Some(b'/') | Some(b'@') => true,
                Some(b'.') | Some(b':') => after.is_some_and(|a| a.is_ascii_alphanumeric()),
                Some(n) => n.is_ascii_alphanumeric(),
                None => false,
            };
            if !name.is_empty() && !continues && valid(name) {
                let name = name.to_string();
                if leading {
                    if !out.leading.contains(&name) {
                        out.leading.push(name);
                    }
                } else if !out.leading.contains(&name) && !out.embedded.contains(&name) {
                    out.embedded.push(name);
                }
                i = end;
                continue;
            }
        }
        leading = false;
        i += 1;
    }
    out
}

/// Who wrote a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Author {
    /// The person, through a leading mention in an agent's prompt, the TUI or a web composer.
    Human { pane: Option<String> },
    /// The agent in a pane (through MCP, the CLI or the API with a pane token).
    Agent { pane: String, name: Option<String> },
}

/// One message (spec 19 §3.1). Bodies are redacted before they are stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    /// The first message of the conversation; replies share it.
    pub thread: String,
    pub workspace: String,
    pub from: Author,
    /// Recipient names as written (groups expanded in phase 2).
    pub to: Vec<String>,
    pub body: String,
    pub in_reply_to: Option<String>,
    /// How many messages led to this one; the server refuses past `MAX_HOPS`.
    pub hop: u32,
    pub sent_at_ms: i64,
}

/// Deepest reply chain the server accepts (loop guard).
pub const MAX_HOPS: u32 = 8;

/// One recipient's copy of a message (spec 19 §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub id: String,
    pub message: String,
    pub name: String,
    pub pane: String,
    pub state: DeliveryState,
    /// Why the delivery is held or failed.
    pub reason: Option<String>,
    /// Secret that the envelope typed into the pane carries; the hook checks it.
    #[serde(skip_serializing)]
    pub nonce: String,
    /// Whether the sender waits for a reply.
    pub expects_reply: bool,
    /// The reply message, once there is one.
    pub reply: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Waiting for the recipient to be idle.
    Queued,
    /// The envelope was typed into the recipient's prompt.
    Submitted,
    /// The recipient's hook took the envelope: the agent has the message.
    Delivered,
    Replied,
    /// The recipient finished without a reply (`message.complete`, or the turn ended twice).
    Completed,
    /// Not deliverable now (dialog open, delivery off, name moved); kept for `message.retry`.
    Held,
    Failed,
    Cancelled,
}

impl DeliveryState {
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Submitted | Self::Delivered | Self::Held
        )
    }
}

#[cfg(test)]
#[path = "mention_tests.rs"]
mod tests;
