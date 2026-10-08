//! Picker and menu detection on an agent CLI's screen: the generic key-hint menu, the
//! specific Claude grammars (model, effort, resume) and the unknown-dialog fallback.
//!
//! Every detector reads only the bottom of the visible screen and fails closed: anything it
//! does not fully recognise returns `None`, so a wrong guess never becomes a keystroke. The
//! keys a picker is driven with come from its key-hint footer (`↑/↓ to select · Enter to
//! confirm · Esc to cancel`), never from digits.

use super::Harness;
use regex::Regex;
use std::sync::LazyLock;

/// Keys that drive a picker, derived from its footer (named in the key grammar: `up`, `enter`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuKeys {
    pub up: Option<String>,
    pub down: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
    /// Commits the pointed row (single select) or the checked set (multi-select).
    pub confirm: Option<String>,
    /// Commits for this session only, where the footer offers it (`s to use this session only`).
    pub session: Option<String>,
    /// Toggles the pointed row in a multi-select.
    pub toggle: Option<String>,
    pub cancel: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Stable id (never a display digit), see [`assign_ids`]: the label's slug, plus a hash of
    /// the row's stable facts in a scrolling list.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// Checkbox state in a multi-select; `None` in a single-select list.
    pub checked: Option<bool>,
}

/// A left/right adjuster (an effort slider).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adjust {
    pub verb: String,
    pub values: Vec<String>,
    pub current: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// `model` | `effort` | `resume` | `permissions` | `confirm` | `menu` | `unknown`.
    pub name: String,
    pub title: String,
    pub body: Option<String>,
    pub rows: Vec<Row>,
    /// Index into `rows` of the row the harness points at.
    pub pointer: Option<usize>,
    pub multi: bool,
    /// The list shows only part of its rows (a scroll indicator is visible).
    pub scrolls: bool,
    pub adjust: Option<Adjust>,
    pub keys: MenuKeys,
    /// Hash of what the picker asks: name, title, body, footer, a search filter and, for lists
    /// that do not scroll, the row labels and descriptions. Only navigation state is left out
    /// (pointer, checkboxes, adjuster value, the scroll window): it changes while the picker is
    /// being answered. A changed signature is a different picker (a new interaction).
    pub signature: String,
}

impl Picker {
    pub fn is_unknown(&self) -> bool {
        self.name == "unknown"
    }
}

/// Claude's effort ladder (as its `/effort` slider shows it).
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Title of the unknown-dialog fallback.
pub const UNKNOWN_TITLE: &str = "The agent is showing a dialog";

const POINTERS: &[char] = &['❯', '›', '▶', '>'];
const SCROLL_GLYPHS: &[char] = &['↑', '↓'];
const BOX: &[char] = &['│', '┃', '║', '|'];

static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,2})[.)]\s+").unwrap());
static CHECKBOX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\[[ xX✔✓*]\]|☐|☒|☑|◯|◉)\s+").unwrap());
static MORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(…|\.\.\.)?\s*(\+\s*\d+\s+\w+|↓\s*\d+\s+more|\d+\s+more)\s*$").unwrap()
});
static EFFORT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<v>low|medium|high|xhigh|max)\s+effort\b.*←/→ to adjust").unwrap()
});

pub(crate) fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Lower-case slug of a label: the option id (`Opus 5.5` → `opus-5-5`).
pub fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let s = out.trim_end_matches('-').to_string();
    if s.is_empty() { "option".into() } else { s }
}

fn is_border_only(l: &str) -> bool {
    let t = l.trim();
    !t.is_empty() && t.chars().all(|c| "─━═╭╮╰╯┌┐└┘├┤┬┴┼│┃║▔▁ ".contains(c))
}

/// A modal's top edge (`▔▔▔`, possibly with status text drawn over it) or a horizontal rule.
fn is_edge(l: &str) -> bool {
    let t = l.trim_end();
    t.contains("▔▔▔") || t.ends_with('▔') || t.trim_start().starts_with('╭') || {
        let rule = t.chars().filter(|c| *c == '─' || *c == '━').count();
        rule >= 10 && rule * 2 >= t.trim().chars().count()
    }
}

fn strip_box(l: &str) -> &str {
    let t = l.trim();
    let t = t.strip_prefix(BOX).unwrap_or(t);
    let t = t.strip_suffix(BOX).unwrap_or(t);
    t.trim()
}

fn indent(l: &str) -> usize {
    l.chars().take_while(|c| c.is_whitespace()).count()
}

// ---- footer --------------------------------------------------------------------------------------

const CONFIRM_VERBS: &[&str] = &[
    "confirm", "select", "submit", "continue", "set", "use", "choose", "resume", "done", "apply",
    "save", "accept", "ok", "proceed", "pick",
];
const CANCEL_VERBS: &[&str] = &["cancel", "exit", "back", "close", "skip", "dismiss", "quit"];

/// The keys a footer segment like `Enter to confirm`, `esc back`, `↑/↓ to select` names.
fn apply_segment(seg: &str, k: &mut MenuKeys) -> bool {
    let seg = seg.trim();
    let mut parts = seg.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    let verb = rest.strip_prefix("to ").unwrap_or(rest).to_lowercase();
    let first = verb.split_whitespace().next().unwrap_or("");
    let kl = key.to_lowercase();
    if key.is_empty() || verb.is_empty() {
        return false;
    }
    let mut hit = false;
    if key.contains('↑') || key.contains('↓') || kl.contains("arrow") {
        k.up = Some("up".into());
        k.down = Some("down".into());
        hit = true;
    }
    if (key.contains('←') && key.contains('→')) && (first == "adjust" || first == "change") {
        k.left = Some("left".into());
        k.right = Some("right".into());
        hit = true;
    }
    let names: Vec<&str> = kl.split('/').collect();
    if names
        .iter()
        .any(|n| matches!(*n, "enter" | "return" | "⏎" | "↵"))
        && CONFIRM_VERBS.contains(&first)
    {
        k.confirm = Some("enter".into());
        hit = true;
    }
    if names.iter().any(|n| matches!(*n, "esc" | "escape")) && CANCEL_VERBS.contains(&first) {
        k.cancel = Some("escape".into());
        hit = true;
    }
    if names.contains(&"space") && matches!(first, "toggle" | "check" | "select" | "mark") {
        k.toggle = Some("space".into());
        hit = true;
    }
    if names.iter().any(|n| matches!(*n, "tab")) && matches!(first, "toggle") {
        // A toggle of a side setting (not the list), e.g. `Tab to toggle`: recognised, unused.
        hit = true;
    }
    if key.chars().count() == 1
        && key.chars().all(|c| c.is_ascii_lowercase())
        && verb.contains("session")
    {
        k.session = Some(key.to_string());
        hit = true;
    }
    hit
}

/// Split a footer line into hint segments (`·` separated, or columns of 3+ spaces).
fn segments(text: &str) -> Vec<String> {
    static GAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{3,}").unwrap());
    text.split('·')
        .flat_map(|s| GAP.split(s.trim()).map(str::to_string).collect::<Vec<_>>())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn looks_like_footer(text: &str) -> bool {
    let segs = segments(text);
    if segs.len() < 2 {
        return false;
    }
    let mut k = MenuKeys::default();
    let hits = segs.iter().filter(|s| apply_segment(s, &mut k)).count();
    hits >= 1 && (k.confirm.is_some() || k.cancel.is_some())
}

/// The key-hint footer at the bottom of `lines`: (index of its first line, keys, text).
/// A footer may wrap onto two lines; trailing box borders and blank lines are skipped.
pub fn footer(lines: &[&str]) -> Option<(usize, MenuKeys, String)> {
    let mut i = lines.len();
    while i > 0 && (lines[i - 1].trim().is_empty() || is_border_only(lines[i - 1])) {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let last = i - 1;
    let one = strip_box(lines[last]).to_string();
    let joined = (last > 0)
        .then(|| strip_box(lines[last - 1]))
        .filter(|a| {
            !a.is_empty()
                && !a.starts_with(POINTERS)
                && indent(lines[last - 1]) == indent(lines[last])
                && segments(a).len() >= 2
        })
        .map(|a| format!("{a} · {one}"));
    let usable = |t: &str| !t.to_lowercase().contains("interrupt") && looks_like_footer(t);
    // A footer may wrap: prefer the two-line reading when the line above continues the hints.
    let (start, text) = match joined {
        Some(j)
            if usable(&j)
                && (segments(&one).len() < 2 || !usable(&one) || {
                    let a = strip_box(lines[last - 1]);
                    a.contains(" · ") && a.contains(" to ")
                }) =>
        {
            (last - 1, j)
        }
        _ if usable(&one) => (last, one),
        _ => return None,
    };
    let mut k = MenuKeys::default();
    for s in segments(&text) {
        apply_segment(&s, &mut k);
    }
    Some((start, k, text))
}

// ---- list ------------------------------------------------------------------------------------------

struct ListScan {
    rows: Vec<Row>,
    pointer: usize,
    scrolls: bool,
    /// Line index range the list occupies.
    first: usize,
    last: usize,
}

/// Split `label  description` on a run of 2+ spaces.
fn split_label(rest: &str) -> (String, Option<String>) {
    static TWO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").unwrap());
    let mut it = TWO.splitn(rest.trim(), 2);
    let label = it.next().unwrap_or("").trim().to_string();
    let desc = it
        .next()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    (label, desc)
}

fn is_pointer_line(l: &str) -> bool {
    let t = l.trim_start_matches(|c: char| c.is_whitespace() || BOX.contains(&c));
    let mut ch = t.chars();
    matches!(ch.next(), Some(c) if POINTERS.contains(&c))
        && ch.next().is_some_and(char::is_whitespace)
        && ch.as_str().trim_start().chars().next().is_some()
}

/// Parse the pointer-marked list ending above line `end` (exclusive): the lowest pointer line
/// below any modal edge, extended over the contiguous (blank-free) rows around it.
fn scan_list(lines: &[&str], end: usize) -> Option<ListScan> {
    let mut pi = None;
    for i in (0..end).rev() {
        if is_pointer_line(lines[i]) {
            pi = Some(i);
            break;
        }
        if is_edge(lines[i]) && !lines[i].trim_start().starts_with(['│', '┃']) {
            break;
        }
    }
    let pi = pi?;
    let pline: Vec<char> = lines[pi].chars().collect();
    let pcol = pline.iter().position(|c| POINTERS.contains(c))?;
    let label_col = pcol
        + 1
        + pline[pcol + 1..]
            .iter()
            .take_while(|c| c.is_whitespace())
            .count();
    // A row: only blanks (and one pointer or scroll glyph) before `label_col`, text at it.
    let row_shaped = |l: &str| -> bool {
        let cs: Vec<char> = l.chars().collect();
        if cs.len() <= label_col || cs[label_col].is_whitespace() {
            return false;
        }
        let glyphs = cs[..label_col]
            .iter()
            .filter(|c| !c.is_whitespace() && !BOX.contains(c))
            .collect::<Vec<_>>();
        match glyphs.as_slice() {
            [] => true,
            [g] => POINTERS.contains(g) || SCROLL_GLYPHS.contains(g),
            _ => false,
        }
    };
    let deeper = |l: &str| !l.trim().is_empty() && indent(l) > label_col;
    let part_of = |l: &str| row_shaped(l) || deeper(l);
    let mut first = pi;
    while first > 0 && part_of(lines[first - 1]) {
        first -= 1;
    }
    let mut last = pi;
    while last + 1 < end && part_of(lines[last + 1]) {
        last += 1;
    }
    // Leading continuation lines (a deeper-indented paragraph above the rows) are not rows.
    while first < pi && !row_shaped(lines[first]) {
        first += 1;
    }
    if (first..=last)
        .filter(|&i| is_pointer_line(lines[i]))
        .count()
        != 1
    {
        return None;
    }
    let mut rows: Vec<(String, Option<String>, Option<bool>)> = Vec::new();
    let mut pointer = None;
    let mut scrolls = false;
    for (i, l) in lines.iter().enumerate().take(last + 1).skip(first) {
        if MORE.is_match(strip_box(l)) {
            scrolls = true;
            continue;
        }
        if row_shaped(l) {
            let cs: Vec<char> = l.chars().collect();
            if cs[..label_col].iter().any(|c| SCROLL_GLYPHS.contains(c)) {
                scrolls = true;
            }
            let rest: String = cs[label_col..].iter().collect();
            let rest = rest.trim_end().trim_end_matches(BOX).trim_end();
            let rest = NUMBERED.replace(rest, "");
            let (checked, rest) = match CHECKBOX.captures(&rest) {
                Some(c) => {
                    let b = &c[1];
                    (
                        Some(!matches!(b, "[ ]" | "☐" | "◯")),
                        rest[c.get(0).unwrap().end()..].to_string(),
                    )
                }
                None => (None, rest.to_string()),
            };
            let (mut label, desc) = split_label(&rest);
            let mut desc = desc;
            for mark in [" ✔", " ✓"] {
                if let Some(x) = label.strip_suffix(mark) {
                    label = x.trim().to_string();
                    desc = Some(match desc {
                        Some(d) => format!("Current. {d}"),
                        None => "Current.".into(),
                    });
                }
            }
            if label.is_empty() {
                return None;
            }
            if i == pi {
                pointer = Some(rows.len());
            }
            rows.push((label, desc, checked));
        } else {
            // A continuation (wrapped description) of the previous row.
            let prev = rows.last_mut()?;
            let more = strip_box(l).to_string();
            prev.1 = Some(match prev.1.take() {
                Some(d) => format!("{d} {more}"),
                None => more,
            });
        }
    }
    let pointer = pointer?;
    let mut rows: Vec<Row> = rows
        .into_iter()
        .map(|(label, description, checked)| Row {
            id: String::new(),
            label,
            description,
            checked,
        })
        .collect();
    assign_ids(&mut rows, scrolls, |r| r.description.clone());
    Some(ListScan {
        rows,
        pointer,
        scrolls,
        first,
        last,
    })
}

/// Give rows their option ids.
///
/// A fully visible list: the label's slug, numbered for repeated labels (`fix-2`); the whole
/// list is on screen and in the signature, so a position names the same row every time.
///
/// A scrolling list: the slug plus a hash of the row's stable facts (`facts`, e.g. its
/// description), so an id names the same row in every scroll window and filter. Rows that read
/// the same still get distinct (numbered) ids, but they cannot be told apart on screen: the
/// walker refuses to pick one (see [`twin`]).
fn assign_ids(rows: &mut [Row], scrolls: bool, facts: impl Fn(&Row) -> Option<String>) {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    for r in rows.iter_mut() {
        let base = match facts(r).filter(|f| scrolls && !f.is_empty()) {
            Some(f) => format!("{}~{:06x}", slug(&r.label), fnv(&f) & 0xff_ffff),
            None => slug(&r.label),
        };
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        r.id = if *n == 1 { base } else { format!("{base}-{n}") };
    }
}

/// Does row `i` read the same as another visible row (label and description)? Such rows
/// cannot be told apart, so answering one of them is refused rather than guessed.
pub fn twin(rows: &[Row], i: usize) -> bool {
    let r = &rows[i];
    rows.iter()
        .enumerate()
        .any(|(j, o)| j != i && o.label == r.label && o.description == r.description)
}

/// The title block above the list: the nearest paragraph (after a modal edge, if any).
/// Returns (title, body).
fn title_above(lines: &[&str], first: usize) -> Option<(String, Option<String>)> {
    let mut i = first;
    while i > 0 && lines[i - 1].trim().is_empty() {
        i -= 1;
    }
    let mut block = Vec::new();
    while i > 0 {
        let l = lines[i - 1];
        if l.trim().is_empty() || is_edge(l) || is_border_only(l) || block.len() >= 4 {
            break;
        }
        block.push(strip_box(l).to_string());
        i -= 1;
    }
    block.reverse();
    let title = block.first()?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let body = (block.len() > 1).then(|| block[1..].join(" "));
    Some((title, body))
}

fn classify(title: &str) -> &'static str {
    let t = title.to_lowercase();
    if t.trim_end().ends_with('?') {
        "confirm"
    } else if t.contains("permission") || t.contains("approval") {
        "permissions"
    } else if t.contains("resume") {
        "resume"
    } else if t.contains("model") {
        "model"
    } else if t.contains("effort") {
        "effort"
    } else {
        "menu"
    }
}

/// The picker's semantic content: what it asks (`title`, `body`, a search `filter`), what it
/// offers (`rows`: labels and descriptions, for a list that shows all of them) and how it is
/// answered (`footer`). Navigation state never goes in.
fn signature(
    name: &str,
    title: &str,
    body: Option<&str>,
    filter: Option<&str>,
    footer: &str,
    rows: Option<&[Row]>,
) -> String {
    let rows = rows
        .map(|r| {
            r.iter()
                .map(|x| {
                    format!(
                        "{}\u{1f}{}",
                        x.label,
                        x.description.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\u{1e}")
        })
        .unwrap_or_default();
    let (body, filter) = (body.unwrap_or(""), filter.unwrap_or(""));
    format!(
        "{:x}",
        fnv(&format!(
            "{name}\u{1d}{title}\u{1d}{body}\u{1d}{filter}\u{1d}{footer}\u{1d}{rows}"
        ))
    )
}

/// The generic menu: a key-hint footer at the bottom, a pointer-marked list above it and a
/// title above the list. Fails closed on anything it does not fully recognise.
pub fn generic_menu(lines: &[&str]) -> Option<Picker> {
    let (fstart, keys, ftext) = footer(lines)?;
    let list = scan_list(lines, fstart)?;
    if list.rows.len() < 2 || list.rows.len() > 20 || (list.rows.len() > 9 && !list.scrolls) {
        return None;
    }
    // Between the list and the footer only short context lines may sit (an adjuster, a note).
    if fstart.saturating_sub(list.last) > 8 {
        return None;
    }
    let (title, body) = title_above(lines, list.first)?;
    let multi = list.rows.iter().all(|r| r.checked.is_some());
    if !multi && list.rows.iter().any(|r| r.checked.is_some()) {
        return None;
    }
    let mut keys = keys;
    // A pointer-marked list is arrow-navigable; walking verifies every step on screen, so a
    // footer that leaves the arrows implicit (`enter select · esc back`) is still safe.
    keys.up.get_or_insert_with(|| "up".into());
    keys.down.get_or_insert_with(|| "down".into());
    if keys.confirm.is_none() && keys.cancel.is_none() {
        return None;
    }
    if multi && keys.toggle.is_none() {
        keys.toggle = Some("space".into());
    }
    let name = classify(&title).to_string();
    let rows_for_sig = (!list.scrolls).then_some(list.rows.as_slice());
    let signature = signature(&name, &title, body.as_deref(), None, &ftext, rows_for_sig);
    let mut rows = list.rows;
    if !multi {
        for r in &mut rows {
            r.checked = None;
        }
    }
    Some(Picker {
        name,
        title,
        body,
        rows,
        pointer: Some(list.pointer),
        multi,
        scrolls: list.scrolls,
        adjust: None,
        keys,
        signature,
    })
}

// ---- Claude grammars -------------------------------------------------------------------------------

/// Claude `/model`: the generic list plus the effort adjuster line under it
/// (`◐ Medium effort (default) ←/→ to adjust`).
fn claude_model(lines: &[&str]) -> Option<Picker> {
    let mut p = generic_menu(lines)?;
    if p.name != "model" {
        return None;
    }
    let (fstart, _, _) = footer(lines)?;
    let current = lines[..fstart]
        .iter()
        .rev()
        .take(8)
        .find_map(|l| EFFORT_LINE.captures(l).map(|c| c["v"].to_lowercase()));
    if let Some(cur) = current {
        p.adjust = Some(Adjust {
            verb: "Adjust effort".into(),
            values: CLAUDE_EFFORTS.iter().map(|s| s.to_string()).collect(),
            current: Some(cur),
        });
        p.keys.left = Some("left".into());
        p.keys.right = Some("right".into());
    }
    Some(p)
}

/// Claude `/effort`: a slider (`───▲───`) over the value labels, `←/→ to adjust` footer.
fn claude_effort(lines: &[&str]) -> Option<Picker> {
    let (fstart, keys, ftext) = footer(lines)?;
    keys.left.as_ref()?;
    let window = &lines[fstart.saturating_sub(12)..fstart];
    let si = window.iter().position(|l| {
        let t = l.trim();
        t.contains('▲') && t.chars().filter(|c| *c == '▲').count() == 1 && {
            let slider: String = t.split_whitespace().next().unwrap_or("").to_string();
            slider.chars().all(|c| c == '─' || c == '▲') && slider.chars().count() >= 10
        }
    })?;
    let slider_line: Vec<char> = window[si].chars().collect();
    let start = slider_line.iter().position(|c| *c == '─' || *c == '▲')?;
    let mut end = start;
    while end < slider_line.len() && (slider_line[end] == '─' || slider_line[end] == '▲') {
        end += 1;
    }
    let marker = slider_line.iter().position(|c| *c == '▲')?;
    let labels: Vec<char> = window.get(si + 1)?.chars().collect();
    // Words whose start lies within the slider span are the values.
    let mut values: Vec<(String, usize, usize)> = Vec::new();
    let mut i = start;
    while i < labels.len() && i < end + 2 {
        if labels[i].is_whitespace() {
            i += 1;
            continue;
        }
        let s = i;
        while i < labels.len() && !labels[i].is_whitespace() {
            i += 1;
        }
        values.push((labels[s..i].iter().collect(), s, i));
    }
    if values.len() < 2
        || !values
            .iter()
            .all(|(v, _, _)| CLAUDE_EFFORTS.contains(&v.to_lowercase().as_str()))
    {
        return None;
    }
    // The marker sits under the value whose centre is nearest.
    let current = values
        .iter()
        .min_by_key(|(_, s, e)| ((s + e) / 2).abs_diff(marker))
        .map(|(v, _, _)| v.to_lowercase())?;
    let title = window[..si]
        .iter()
        .rev()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.contains("Faster"))
        .find(|l| !is_edge(l))
        .unwrap_or("Effort")
        .to_string();
    if !title.to_lowercase().contains("effort") {
        return None;
    }
    let vals: Vec<String> = values.iter().map(|(v, _, _)| v.to_lowercase()).collect();
    let rows = vals
        .iter()
        .map(|v| Row {
            id: v.clone(),
            label: v.clone(),
            description: None,
            checked: None,
        })
        .collect();
    let pointer = vals.iter().position(|v| *v == current);
    let name = "effort".to_string();
    // The values are fixed by `CLAUDE_EFFORTS`; the marker position is navigation state.
    let signature = signature(&name, &title, None, None, &ftext, None);
    Some(Picker {
        name,
        title,
        body: None,
        rows,
        pointer,
        multi: false,
        scrolls: false,
        adjust: Some(Adjust {
            verb: "Adjust effort".into(),
            values: vals,
            current: Some(current),
        }),
        keys: MenuKeys {
            up: None,
            down: None,
            confirm: keys.confirm.clone().or(Some("enter".into())),
            ..keys
        },
        signature,
    })
}

/// Claude `/resume`: two-line rows (title, then `<age> · <branch> · <size>`) separated by
/// blank lines, a search box above them, and a footer without an explicit confirm key
/// (Enter resumes the pointed session).
fn claude_resume(lines: &[&str]) -> Option<Picker> {
    let (fstart, keys, ftext) = footer(lines)?;
    keys.cancel.as_ref()?;
    // The modal title right after its top edge.
    let edge = lines[..fstart]
        .iter()
        .rposition(|l| is_edge(l) && l.contains('▔'))?;
    let title_at = edge
        + 1
        + lines[edge + 1..fstart]
            .iter()
            .position(|l| !l.trim().is_empty())?;
    let title = lines[title_at].trim().to_string();
    if !title.starts_with("Resume session") {
        return None;
    }
    // Blocks of row-shaped lines after the search box.
    let boxed_end = lines[edge + 1..fstart]
        .iter()
        .rposition(|l| l.trim_start().starts_with('╰'))
        .map(|i| edge + 1 + i + 1)
        .unwrap_or(title_at + 1)
        .max(title_at + 1);
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for l in &lines[boxed_end..fstart] {
        if l.trim().is_empty() {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(l);
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    // Session rows are two lines; a one-line block is a group header (the project name).
    let sessions: Vec<&Vec<&str>> = blocks.iter().filter(|b| b.len() == 2).collect();
    if sessions.is_empty() || blocks.iter().any(|b| b.len() > 2) {
        return None;
    }
    // The search box's text: a different filter is a different list.
    let filter = lines[title_at + 1..boxed_end]
        .iter()
        .map(|l| strip_box(l))
        .filter(|l| !l.is_empty() && !is_border_only(l))
        .collect::<Vec<_>>()
        .join(" ");
    let mut rows = Vec::new();
    let mut pointer = None;
    for b in &sessions {
        let head = b[0].trim();
        let mut ch = head.chars();
        let g = ch.next()?;
        let (is_ptr, label) = if POINTERS.contains(&g) {
            (true, ch.as_str().trim())
        } else if SCROLL_GLYPHS.contains(&g) {
            (false, ch.as_str().trim())
        } else {
            (false, head)
        };
        if label.is_empty() || !b[1].contains(" ago") {
            return None;
        }
        if is_ptr {
            if pointer.is_some() {
                return None;
            }
            pointer = Some(rows.len());
        }
        rows.push(Row {
            id: String::new(),
            label: label.to_string(),
            description: Some(b[1].trim().to_string()),
            checked: None,
        });
    }
    let pointer = pointer?;
    // A session's identity: its title plus the facts after the age (branch, size, link). The
    // age (`1 second ago`) ticks while the list is open, so it stays out.
    assign_ids(&mut rows, true, |r| {
        r.description
            .as_deref()
            .map(|d| d.split_once(" · ").map_or("", |(_, rest)| rest).to_string())
    });
    let name = "resume".to_string();
    // The title's `(1 of 23)` is the pointer position: navigation state, left out.
    let signature = signature(&name, "Resume session", None, Some(&filter), &ftext, None);
    Some(Picker {
        name,
        title,
        body: None,
        rows,
        pointer: Some(pointer),
        multi: false,
        // A searchable list of sessions: always partial.
        scrolls: true,
        adjust: None,
        keys: MenuKeys {
            up: Some("up".into()),
            down: Some("down".into()),
            confirm: Some("enter".into()),
            ..keys
        },
        signature,
    })
}

// ---- unknown dialog --------------------------------------------------------------------------------

/// Claude's input box: a `❯` line between two horizontal rules.
fn claude_input_box(lines: &[&str]) -> bool {
    let n = lines.len();
    (1..n.saturating_sub(1)).any(|i| {
        lines[i].trim_start().starts_with('❯')
            && is_rule(lines[i - 1])
            && (i + 1..n.min(i + 6)).any(|j| is_rule(lines[j]))
    })
}

fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─' || c == '━')
}

/// A modal is on screen but no grammar recognised it: a cancel hint in a key-hint footer (or
/// Claude's modal edge) with the harness's input box hidden.
fn unknown_dialog(h: Harness, lines: &[&str]) -> Option<Picker> {
    let foot = footer(lines);
    let claude = h == Harness::Claude;
    if claude && claude_input_box(lines) {
        return None;
    }
    let edge = claude && lines.iter().any(|l| l.contains("▔▔▔"));
    let cancel = foot.as_ref().and_then(|(_, k, _)| k.cancel.clone());
    if cancel.is_none() && !edge {
        return None;
    }
    let ftext = foot.as_ref().map(|f| f.2.clone()).unwrap_or_default();
    // The dialog's own heading (after Claude's modal edge, else the first non-empty line of
    // the bottom block) gives the user context.
    let heading = if edge {
        let e = lines.iter().rposition(|l| l.contains("▔▔▔"))?;
        lines[e + 1..]
            .iter()
            .map(|l| strip_box(l))
            .find(|l| !l.is_empty() && !is_border_only(l))
            .map(str::to_string)
    } else {
        let end = foot.as_ref().map(|f| f.0).unwrap_or(lines.len());
        let mut i = end;
        while i > 0 && !lines[i - 1].trim().is_empty() {
            i -= 1;
        }
        lines[..end]
            .iter()
            .skip(i)
            .map(|l| strip_box(l))
            .find(|l| !l.is_empty() && !is_border_only(l))
            .map(str::to_string)
    };
    let name = "unknown".to_string();
    let signature = format!(
        "{:x}",
        fnv(&format!(
            "unknown|{}|{ftext}",
            heading.clone().unwrap_or_default()
        ))
    );
    Some(Picker {
        name,
        title: UNKNOWN_TITLE.into(),
        body: heading,
        rows: vec![],
        pointer: None,
        multi: false,
        scrolls: false,
        adjust: None,
        keys: MenuKeys {
            cancel: Some(cancel.unwrap_or_else(|| "escape".into())),
            ..Default::default()
        },
        signature,
    })
}

/// The bottom of the screen the detectors read: the last 40 lines down to the last non-blank.
fn bottom<'a>(screen_lines: &'a [&'a str]) -> &'a [&'a str] {
    let mut end = screen_lines.len();
    while end > 0 && screen_lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    &screen_lines[end.saturating_sub(40)..end]
}

/// Detect a picker on the bottom of the screen for harness `h` (already reduced to its base):
/// the harness's specific grammars first, then the generic menu.
pub fn detect(h: Harness, screen_lines: &[&str]) -> Option<Picker> {
    let lines = bottom(screen_lines);
    if h == Harness::Claude
        && let Some(p) = claude_effort(lines)
            .or_else(|| claude_resume(lines))
            .or_else(|| claude_model(lines))
    {
        return Some(p);
    }
    generic_menu(lines)
}

/// The unknown-dialog fallback for the code-backed harnesses (Claude, Codex, pi, omp).
pub fn detect_unknown(h: Harness, screen_lines: &[&str]) -> Option<Picker> {
    if !matches!(
        h,
        Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp
    ) {
        return None;
    }
    unknown_dialog(h, bottom(screen_lines))
}

// ---- answering: key planning -------------------------------------------------------------------

/// What the answer wants from the picker.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Goal {
    pub cancel: bool,
    /// Single select: the target row id.
    pub target: Option<String>,
    /// Multi-select: the full desired checked set.
    pub checked: Option<Vec<String>>,
    /// Adjuster value.
    pub adjust: Option<String>,
    /// Commit with the harness's persistent confirm (`default`) instead of a session-only key.
    pub persist: bool,
}

/// The next key to send, read off one verified screen read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Move or toggle and verify the screen changed before planning again.
    Key(String),
    /// Final key, bound to this read (Enter / session key / cancel key).
    Commit(String),
    Fail(String),
}

/// Plan one step. `order` is the row order recorded when the interaction opened (used to pick
/// a direction when a scrolled list no longer shows the target).
pub fn next_step(p: &Picker, goal: &Goal, order: &[String]) -> Step {
    if goal.cancel {
        return match &p.keys.cancel {
            Some(k) => Step::Commit(k.clone()),
            None => Step::Fail("this picker cannot be dismissed safely".into()),
        };
    }
    let walk_to = |want: &str| -> Option<Step> {
        let cur = p.pointer?;
        let pos = p.rows.iter().position(|r| r.id == want);
        // Rows that read the same cannot be told apart: never guess which one was meant.
        if pos.is_some_and(|t| twin(&p.rows, t)) {
            return Some(Step::Fail(format!(
                "picker_changed: option {want} reads the same as another row on screen"
            )));
        }
        let dir = match pos {
            Some(t) if t == cur => return None,
            Some(t) => t > cur,
            None => {
                let at = order.iter().position(|x| *x == p.rows[cur].id);
                let to = order.iter().position(|x| x == want);
                match (at, to) {
                    (Some(a), Some(b)) if a != b => b > a,
                    _ => return Some(Step::Fail(format!("option {want} is not on screen"))),
                }
            }
        };
        let k = if dir { &p.keys.down } else { &p.keys.up };
        Some(match k {
            Some(k) => Step::Key(k.clone()),
            None => Step::Fail("the picker offers no navigation keys".into()),
        })
    };
    if let Some(want) = &goal.checked {
        if !p.multi {
            return Step::Fail("not a multi-select picker".into());
        }
        if let Some(r) = (0..p.rows.len()).find(|&i| twin(&p.rows, i)) {
            return Step::Fail(format!(
                "picker_changed: option {} reads the same as another row on screen",
                p.rows[r].id
            ));
        }
        if let Some(r) = p
            .rows
            .iter()
            .find(|r| r.checked.unwrap_or(false) != want.contains(&r.id))
        {
            if let Some(s) = walk_to(&r.id) {
                return s;
            }
            return match &p.keys.toggle {
                Some(k) => Step::Key(k.clone()),
                None => Step::Fail("the picker offers no toggle key".into()),
            };
        }
        if want.iter().any(|w| !p.rows.iter().any(|r| &r.id == w)) {
            return Step::Fail("an option to check is not on screen".into());
        }
        return match &p.keys.confirm {
            Some(k) => Step::Commit(k.clone()),
            None => Step::Fail("the picker offers no confirm key".into()),
        };
    }
    let adjuster_only = p.name == "effort";
    if let Some(want) = &goal.target
        && !adjuster_only
    {
        if p.multi {
            return Step::Fail("a multi-select picker takes the full checked set".into());
        }
        if let Some(s) = walk_to(want) {
            return s;
        }
    }
    let adjust_to = goal
        .adjust
        .clone()
        .or_else(|| goal.target.clone().filter(|_| adjuster_only));
    if let Some(want) = adjust_to {
        let Some(a) = &p.adjust else {
            return Step::Fail("the picker has no adjuster".into());
        };
        let Some(t) = a.values.iter().position(|v| *v == want) else {
            return Step::Fail(format!("{want} is not an adjuster value"));
        };
        let Some(c) = a
            .current
            .as_ref()
            .and_then(|c| a.values.iter().position(|v| v == c))
        else {
            return Step::Fail("the adjuster's current value is not readable".into());
        };
        if t != c {
            let k = if t > c { &p.keys.right } else { &p.keys.left };
            return match k {
                Some(k) => Step::Key(k.clone()),
                None => Step::Fail("the picker offers no left/right keys".into()),
            };
        }
    }
    if goal.target.is_none() && goal.adjust.is_none() {
        return Step::Fail("nothing to choose".into());
    }
    // Commit: a session-only key when offered (never silently persist a new default).
    let key = if goal.persist {
        p.keys.confirm.clone()
    } else {
        p.keys.session.clone().or_else(|| p.keys.confirm.clone())
    };
    match key {
        Some(k) => Step::Commit(k),
        None => Step::Fail("the picker offers no confirm key".into()),
    }
}

/// The observable state a step must change (pointer, checks, adjuster).
pub fn state_of(p: &Picker) -> (Option<String>, Vec<bool>, Option<String>) {
    (
        p.pointer.and_then(|i| p.rows.get(i)).map(|r| r.id.clone()),
        p.rows.iter().map(|r| r.checked.unwrap_or(false)).collect(),
        p.adjust.as_ref().and_then(|a| a.current.clone()),
    )
}
