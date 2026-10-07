//! A reusable path input with a live directory list (`new_workspace`, and the handoff accept
//! flow later).
//!
//! The input is text with a cursor (`←/→`, `home/end`, `ctrl+a/e/u/w`, `backspace`, `delete`).
//! Below it, the child directories of the folder being typed, filtered with [`nav::fuzzy`] by the
//! partial name after the last `/`; git repositories carry a `git` note. `tab` completes the
//! longest common prefix of the matching names (a single match completes to `name/`); a second
//! `tab` that can't complete further walks the list. `↑/↓` choose a row; `→` or `enter` on a
//! chosen row goes into it, `←` (with a row chosen) or `ctrl+backspace` goes to the parent.
//! `enter` with no row chosen submits the typed path; `esc` drops the choice, then cancels.
//!
//! The TUI runs on the host it shows, so it reads the local disk directly through
//! [`DirSource`]; tests inject a fake one. For a remote machine the list stays empty
//! ([`NoDirs`]) and the picker is a plain path input.
//!
//! [`nav::fuzzy`]: crate::nav::fuzzy

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::render::{Style, attr};

use crate::app::{App, Mode, Popup, Prompt, PromptKind};
use crate::nav::{fuzzy, highlight, list_frame, list_row};
use crate::screen::{Grid, Rect as SRect};

/// One child directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    /// Holds a `.git` (directory, or the file of a linked worktree).
    pub git_repo: bool,
}

/// Where directory listings come from.
pub trait DirSource: Send + Sync {
    /// The child directories of `dir`, sorted case-insensitively; dot-directories only when
    /// `dot` is set. `None` when `dir` can't be read.
    fn list(&self, dir: &Path, dot: bool) -> Option<Vec<DirEntry>>;
    /// The home folder `~` stands for.
    fn home(&self) -> Option<PathBuf>;
}

/// Most directories read from one folder.
pub const MAX_ENTRIES: usize = 2000;

/// The local disk.
pub struct LocalDirs;

impl DirSource for LocalDirs {
    fn list(&self, dir: &Path, dot: bool) -> Option<Vec<DirEntry>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            // Dot-directories only when asked for (the others stay; the filter narrows them).
            if name.starts_with('.') && !dot {
                continue;
            }
            let path = e.path();
            // `is_dir` follows symlinks: a link to a folder is a folder here.
            if !path.is_dir() {
                continue;
            }
            let git_repo = std::fs::symlink_metadata(path.join(".git")).is_ok();
            out.push(DirEntry { name, git_repo });
            if out.len() >= MAX_ENTRIES {
                break;
            }
        }
        sort(&mut out);
        Some(out)
    }

    fn home(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// No listing (a remote machine's disk is not ours to read).
pub struct NoDirs;

impl DirSource for NoDirs {
    fn list(&self, _: &Path, _: bool) -> Option<Vec<DirEntry>> {
        None
    }
    fn home(&self) -> Option<PathBuf> {
        None
    }
}

fn sort(v: &mut [DirEntry]) {
    v.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// What a key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Cancel,
    /// Enter with no row chosen: the typed path (`~` not expanded; see [`PathPicker::resolved`]).
    Submit(String),
}

#[derive(Clone)]
pub struct PathPicker {
    pub input: String,
    /// Byte offset into `input` (always on a char boundary).
    pub cursor: usize,
    /// Chosen row of [`PathPicker::matches`].
    pub sel: Option<usize>,
    /// The last key was a `tab` that completed nothing.
    tab_stuck: bool,
    /// (folder text, dot) the entries were read for.
    listed: Option<(String, bool)>,
    entries: Vec<DirEntry>,
    fs: Arc<dyn DirSource>,
}

impl fmt::Debug for PathPicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PathPicker")
            .field("input", &self.input)
            .field("cursor", &self.cursor)
            .field("sel", &self.sel)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl PathPicker {
    pub fn new(initial: impl Into<String>, fs: Arc<dyn DirSource>) -> Self {
        let input = initial.into();
        let mut p = PathPicker {
            cursor: input.len(),
            input,
            sel: None,
            tab_stuck: false,
            listed: None,
            entries: Vec::new(),
            fs,
        };
        p.refresh();
        p
    }

    /// The folder part of the input (through the last `/`) and the partial name after it.
    pub fn split(&self) -> (&str, &str) {
        match self.input.rfind('/') {
            Some(i) => (&self.input[..=i], &self.input[i + 1..]),
            None => ("", &self.input),
        }
    }

    /// `~` and `~/…` expanded against the source's home folder.
    pub fn expand(&self, s: &str) -> String {
        let home = self.fs.home().map(|h| h.to_string_lossy().into_owned());
        match (home, s) {
            (Some(h), "~") => h,
            (Some(h), _) if s.starts_with("~/") => {
                format!("{}/{}", h.trim_end_matches('/'), &s[2..])
            }
            _ => s.to_string(),
        }
    }

    /// The typed path with `~` expanded (what a command should get).
    pub fn resolved(&self) -> String {
        self.expand(self.input.trim())
    }

    /// Re-read the folder when the folder part (or the dot-directory need) changed.
    fn refresh(&mut self) {
        let (dir, name) = self.split();
        let key = (dir.to_string(), name.starts_with('.'));
        if self.listed.as_ref() == Some(&key) {
            return;
        }
        self.entries = if key.0.is_empty() {
            Vec::new()
        } else {
            let path = PathBuf::from(self.expand(&key.0));
            if path.is_absolute() {
                self.fs.list(&path, key.1).unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        self.listed = Some(key);
    }

    /// The listed folder's directories matching the partial name, best first: (entry index,
    /// matched char positions). Dot-directories only when the name starts with `.`.
    pub fn matches(&self) -> Vec<(usize, Vec<usize>)> {
        let (_, name) = self.split();
        let dot = name.starts_with('.');
        let mut v: Vec<(usize, i32, Vec<usize>)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| dot || !e.name.starts_with('.'))
            .filter_map(|(i, e)| {
                if name.is_empty() {
                    return Some((i, 0, Vec::new()));
                }
                fuzzy(name, &e.name).map(|m| (i, m.score, m.positions))
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().map(|(i, _, p)| (i, p)).collect()
    }

    /// The entry behind row `i` of [`PathPicker::matches`].
    pub fn entry(&self, row: usize) -> Option<&DirEntry> {
        self.matches().get(row).map(|(i, _)| &self.entries[*i])
    }

    fn set_input(&mut self, s: String) {
        self.cursor = s.len();
        self.input = s;
        self.sel = None;
        self.refresh();
    }

    fn edited(&mut self) {
        self.sel = None;
        self.refresh();
    }

    /// Into `name` in the listed folder.
    pub fn descend(&mut self, name: &str) {
        let dir = self.split().0.to_string();
        self.set_input(format!("{dir}{name}/"));
    }

    /// To the listed folder's parent (`~/code/x/` → `~/code/`, `~/` → the home folder's parent).
    pub fn ascend(&mut self) {
        let mut dir = self.split().0.trim_end_matches('/').to_string();
        if dir == "~" {
            dir = self.expand("~").trim_end_matches('/').to_string();
        }
        let next = match dir.rfind('/') {
            Some(i) => dir[..=i].to_string(),
            None if self.input.starts_with('/') => "/".into(),
            None => return,
        };
        self.set_input(next);
    }

    /// Tab completion: the longest common prefix (case-insensitive) of the names starting
    /// with the partial name; a single match completes to `name/`. False when nothing changed.
    pub fn complete(&mut self) -> bool {
        let (dir, name) = self.split();
        let (dir, low) = (dir.to_string(), name.to_lowercase());
        let dot = name.starts_with('.');
        let hits: Vec<&DirEntry> = self
            .entries
            .iter()
            .filter(|e| {
                (dot || !e.name.starts_with('.')) && e.name.to_lowercase().starts_with(&low)
            })
            .collect();
        let next = match hits.as_slice() {
            [] => return false,
            [one] => format!("{dir}{}/", one.name),
            many => {
                let common = common_prefix(many.iter().map(|e| e.name.as_str()));
                if common.chars().count() <= low.chars().count() {
                    return false;
                }
                format!("{dir}{common}")
            }
        };
        if next == self.input {
            return false;
        }
        self.set_input(next);
        true
    }

    fn move_sel(&mut self, down: bool, wrap: bool) {
        let n = self.matches().len();
        if n == 0 {
            self.sel = None;
            return;
        }
        self.sel = match (self.sel, down) {
            (None, true) => Some(0),
            (Some(i), true) if i + 1 < n => Some(i + 1),
            (Some(_), true) => Some(if wrap { 0 } else { n - 1 }),
            (Some(0), false) | (None, false) => None,
            (Some(i), false) => Some(i - 1),
        };
    }

    fn prev_boundary(&self) -> usize {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.input[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    fn insert(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.edited();
    }

    /// Paste: newlines become spaces (a path is one line).
    pub fn paste(&mut self, text: &str) {
        self.insert(&text.replace(['\n', '\r'], " "));
    }

    /// `ctrl+w`: back over trailing `/`, then to just after the previous `/`.
    fn delete_segment(&mut self) {
        let before = &self.input[..self.cursor];
        let trimmed = before.trim_end_matches('/');
        let start = trimmed.rfind('/').map_or(0, |i| i + 1);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.edited();
    }

    pub fn key(&mut self, ev: &KeyEvent) -> Outcome {
        if ev.kind == KeyKind::Release {
            return Outcome::Stay;
        }
        let ctrl = ev.mods.ctrl();
        let was_stuck = std::mem::take(&mut self.tab_stuck);
        match &ev.key {
            Key::Named(NamedKey::Escape) => {
                if self.sel.take().is_none() {
                    return Outcome::Cancel;
                }
            }
            Key::Named(NamedKey::Enter) => match self.sel.and_then(|i| self.entry(i)).cloned() {
                Some(e) => self.descend(&e.name),
                None => return Outcome::Submit(self.input.trim().to_string()),
            },
            Key::Named(NamedKey::Tab) if !ev.mods.shift() => {
                if !self.complete() {
                    if was_stuck {
                        self.move_sel(true, true);
                    }
                    self.tab_stuck = true;
                }
            }
            Key::Named(NamedKey::Up) => self.move_sel(false, false),
            Key::Named(NamedKey::Down) => self.move_sel(true, false),
            Key::Named(NamedKey::Right) => {
                if let Some(e) = self.sel.and_then(|i| self.entry(i)).cloned() {
                    self.descend(&e.name);
                } else if self.cursor < self.input.len() {
                    self.cursor = self.next_boundary();
                } else {
                    self.complete();
                }
            }
            Key::Named(NamedKey::Left) => {
                if self.sel.is_some() {
                    self.ascend();
                } else {
                    self.cursor = self.prev_boundary();
                }
            }
            Key::Named(NamedKey::Home) => self.cursor = 0,
            Key::Named(NamedKey::End) => self.cursor = self.input.len(),
            Key::Char('a') if ctrl => self.cursor = 0,
            Key::Char('e') if ctrl => self.cursor = self.input.len(),
            Key::Char('u') if ctrl => {
                self.input.replace_range(..self.cursor, "");
                self.cursor = 0;
                self.edited();
            }
            Key::Char('w') if ctrl => self.delete_segment(),
            Key::Named(NamedKey::Backspace) if ctrl || ev.mods.alt() => self.ascend(),
            // Some terminals send ctrl+backspace as ctrl+h.
            Key::Char('h') if ctrl => self.ascend(),
            Key::Named(NamedKey::Backspace) => {
                if self.cursor > 0 {
                    let start = self.prev_boundary();
                    self.input.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.edited();
                }
            }
            Key::Named(NamedKey::Delete) => {
                if self.cursor < self.input.len() {
                    let end = self.next_boundary();
                    self.input.replace_range(self.cursor..end, "");
                    self.edited();
                }
            }
            Key::Named(NamedKey::Space) if !ctrl => self.insert(" "),
            Key::Char(c) if !ctrl && !ev.mods.alt() => {
                let mut b = [0u8; 4];
                self.insert(c.encode_utf8(&mut b));
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Draw as a list popup; returns the text cursor position.
    pub fn draw(&self, app: &App, g: &mut Grid, title: &str) -> (u16, u16) {
        // Fit the input: keep the cursor visible by dropping chars from the left.
        let area = app.pane_area();
        let avail = 90u16
            .min(area.w.saturating_sub(2))
            .max(20)
            .saturating_sub(6) as usize;
        let mut start = 0;
        while UnicodeWidthStr::width(&self.input[start..self.cursor]) > avail {
            start += self.input[start..].chars().next().map_or(1, char::len_utf8);
        }
        let shown = &self.input[start..];
        let (x, y, w, rows) = list_frame(app, g, title, shown);
        let t = app.theme;
        let hi = Style {
            attrs: attr::BOLD | attr::UNDERLINE,
            ..t.s(t.accent)
        };
        let list = self.matches();
        let sel = self.sel.unwrap_or(0);
        let skip = sel.saturating_sub(rows.saturating_sub(1) as usize);
        for (row, (i, (ei, pos))) in list
            .iter()
            .enumerate()
            .skip(skip)
            .take(rows as usize)
            .enumerate()
        {
            let e = &self.entries[*ei];
            let mut segs = highlight(&e.name, pos, t.text(), hi);
            segs.push(("/".into(), t.dim()));
            let at = SRect {
                x,
                y: y + row as u16,
                w,
                h: 1,
            };
            let note = if e.git_repo { "git" } else { "" };
            list_row(g, at, segs, note, self.sel == Some(i), app);
        }
        if list.is_empty() {
            let msg = if self.split().0.is_empty() || self.listed.is_none() {
                ""
            } else if self.split().1.is_empty() {
                "no folders here"
            } else {
                "no matching folder"
            };
            g.put_str(x + 1, y, msg, t.dim(), w);
        }
        let before = UnicodeWidthStr::width(&self.input[start..self.cursor]) as u16;
        (x + 2 + before, y - 1)
    }
}

// ---- as a popup -------------------------------------------------------------------------------

/// A path picker standing in for a text prompt: on submit, the prompt `kind` gets the path with
/// `~` expanded.
#[derive(Debug, Clone)]
pub struct PathPrompt {
    pub kind: PromptKind,
    pub title: String,
    pub picker: PathPicker,
}

/// Open a picker for `kind` on the current machine (its disk is read only when it is local).
pub fn open(app: &mut App, kind: PromptKind, title: &str, initial: &str) {
    let fs: Arc<dyn DirSource> = if app.m().local {
        Arc::new(LocalDirs)
    } else {
        Arc::new(NoDirs)
    };
    open_with(app, kind, title, initial, fs);
}

pub fn open_with(
    app: &mut App,
    kind: PromptKind,
    title: &str,
    initial: &str,
    fs: Arc<dyn DirSource>,
) {
    app.mode = Mode::Popup(Popup::Path(Box::new(PathPrompt {
        kind,
        title: title.into(),
        picker: PathPicker::new(initial, fs),
    })));
}

pub fn popup_key(app: &mut App, ev: KeyEvent, mut p: Box<PathPrompt>) {
    match p.picker.key(&ev) {
        Outcome::Stay => app.mode = Mode::Popup(Popup::Path(p)),
        Outcome::Cancel => {}
        Outcome::Submit(_) => {
            let PathPrompt {
                kind,
                title,
                picker,
            } = *p;
            app.submit_prompt(Prompt {
                kind,
                label: title,
                input: picker.resolved(),
            });
        }
    }
}

pub fn popup_draw(app: &App, g: &mut Grid, p: &PathPrompt) -> (u16, u16) {
    let title = format!(
        "{} · tab completes · ↑↓ choose · → into · ctrl+⌫ up · enter",
        p.title
    );
    p.picker.draw(app, g, &title)
}

/// Longest common prefix of `names`, compared case-insensitively, spelled as in the first.
pub fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = names.next() else {
        return String::new();
    };
    let f: Vec<char> = first.chars().collect();
    let mut n = f.len();
    for s in names {
        n = f
            .iter()
            .zip(s.chars())
            .take(n)
            .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            .count();
    }
    f[..n].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use vk_proto::input::Mods;

    /// Folders by absolute path; names ending in `*` are git repositories.
    struct FakeDirs(BTreeMap<&'static str, Vec<&'static str>>);

    impl DirSource for FakeDirs {
        fn list(&self, dir: &Path, dot: bool) -> Option<Vec<DirEntry>> {
            let key = dir.to_str()?.trim_end_matches('/');
            let key = if key.is_empty() { "/" } else { key };
            let mut v: Vec<DirEntry> = self
                .0
                .get(key)?
                .iter()
                .map(|n| DirEntry {
                    name: n.trim_end_matches('*').to_string(),
                    git_repo: n.ends_with('*'),
                })
                .filter(|e| dot || !e.name.starts_with('.'))
                .collect();
            sort(&mut v);
            Some(v)
        }
        fn home(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/home/u"))
        }
    }

    fn fs() -> Arc<dyn DirSource> {
        Arc::new(FakeDirs(BTreeMap::from([
            ("/", vec!["home", "srv"]),
            ("/home", vec!["u"]),
            (
                "/home/u",
                vec!["code", "Documents", "dev", ".config", "notes.d"],
            ),
            ("/home/u/code", vec!["vibeke*", "vibeke-site*", "scratch"]),
            ("/home/u/code/vibeke", vec!["crates", "web"]),
            ("/home/u/.config", vec!["nvim"]),
        ])))
    }

    fn key(k: Key) -> KeyEvent {
        KeyEvent::new(k, Mods::empty())
    }
    fn named(n: NamedKey) -> KeyEvent {
        key(Key::Named(n))
    }
    fn typ(p: &mut PathPicker, s: &str) {
        for c in s.chars() {
            assert_eq!(p.key(&key(Key::Char(c))), Outcome::Stay);
        }
    }
    fn names(p: &PathPicker) -> Vec<String> {
        p.matches()
            .iter()
            .map(|(i, _)| p.entries[*i].name.clone())
            .collect()
    }

    #[test]
    fn lists_the_typed_folder_without_dot_dirs() {
        let p = PathPicker::new("~/", fs());
        assert_eq!(names(&p), ["code", "dev", "Documents", "notes.d"]);
        let p = PathPicker::new("~/.c", fs());
        assert_eq!(names(&p), [".config"]);
        // A remote machine: no listing, still a path input.
        let p = PathPicker::new("~/", Arc::new(NoDirs));
        assert!(p.matches().is_empty());
        assert_eq!(p.resolved(), "~/");
    }

    #[test]
    fn filters_with_fuzzy_matching() {
        let mut p = PathPicker::new("~/code/", fs());
        typ(&mut p, "vbk");
        assert_eq!(names(&p), ["vibeke", "vibeke-site"]);
        typ(&mut p, "s");
        assert_eq!(names(&p), ["vibeke-site"]);
        let mut p = PathPicker::new("~/", fs());
        typ(&mut p, "do");
        assert_eq!(names(&p)[0], "Documents");
    }

    #[test]
    fn tab_completes_the_common_prefix_then_walks_the_list() {
        let mut p = PathPicker::new("~/c", fs());
        p.key(&named(NamedKey::Tab));
        assert_eq!(p.input, "~/code/");
        assert_eq!(p.cursor, p.input.len());
        typ(&mut p, "v");
        p.key(&named(NamedKey::Tab));
        assert_eq!(p.input, "~/code/vibeke");
        // No progress: the first tab does nothing, the second walks the list.
        p.key(&named(NamedKey::Tab));
        assert_eq!((p.input.as_str(), p.sel), ("~/code/vibeke", None));
        p.key(&named(NamedKey::Tab));
        assert_eq!(p.sel, Some(0));
        p.key(&named(NamedKey::Tab));
        assert_eq!(p.sel, Some(1));
        p.key(&named(NamedKey::Tab));
        assert_eq!(p.sel, Some(0), "wraps");
        // Case-insensitive, spelled as listed.
        let mut p = PathPicker::new("~/doc", fs());
        p.key(&named(NamedKey::Tab));
        assert_eq!(p.input, "~/Documents/");
        assert!(!PathPicker::new("~/zzz", fs()).complete());
        assert_eq!(common_prefix(["Vibeke", "vim"].into_iter()), "Vi");
    }

    #[test]
    fn descends_and_ascends() {
        let mut p = PathPicker::new("~/code/", fs());
        p.key(&named(NamedKey::Down));
        assert_eq!(p.sel, Some(0));
        assert_eq!(p.entry(0).unwrap().name, "scratch");
        p.key(&named(NamedKey::Down));
        // → goes into the chosen folder.
        p.key(&named(NamedKey::Right));
        assert_eq!(p.input, "~/code/vibeke/");
        assert_eq!(names(&p), ["crates", "web"]);
        // Enter on a chosen row also descends (and does not submit).
        p.key(&named(NamedKey::Down));
        assert_eq!(p.key(&named(NamedKey::Enter)), Outcome::Stay);
        assert_eq!(p.input, "~/code/vibeke/crates/");
        // ctrl+backspace and ← (with a row chosen) go to the parent.
        p.key(&KeyEvent::new(Key::Named(NamedKey::Backspace), Mods::CTRL));
        assert_eq!(p.input, "~/code/vibeke/");
        p.key(&named(NamedKey::Down));
        p.key(&named(NamedKey::Left));
        assert_eq!(p.input, "~/code/");
        p.key(&KeyEvent::new(Key::Char('h'), Mods::CTRL));
        assert_eq!(p.input, "~/");
        // Above `~`: the home folder's absolute parent, then the root.
        p.key(&KeyEvent::new(Key::Named(NamedKey::Backspace), Mods::CTRL));
        assert_eq!(p.input, "/home/");
        assert_eq!(names(&p), ["u"]);
        p.key(&KeyEvent::new(Key::Named(NamedKey::Backspace), Mods::CTRL));
        assert_eq!(p.input, "/");
        assert_eq!(names(&p), ["home", "srv"]);
        p.key(&KeyEvent::new(Key::Named(NamedKey::Backspace), Mods::CTRL));
        assert_eq!(p.input, "/");
    }

    #[test]
    fn marks_git_repositories() {
        let p = PathPicker::new("~/code/", fs());
        let git: Vec<(String, bool)> = p
            .matches()
            .iter()
            .map(|(i, _)| (p.entries[*i].name.clone(), p.entries[*i].git_repo))
            .collect();
        assert_eq!(
            git,
            [
                ("scratch".into(), false),
                ("vibeke".into(), true),
                ("vibeke-site".into(), true)
            ]
        );
    }

    #[test]
    fn edits_with_a_cursor_and_submits_the_typed_path() {
        let mut p = PathPicker::new("~/code/", fs());
        typ(&mut p, "new");
        p.key(&named(NamedKey::Left));
        p.key(&named(NamedKey::Left));
        typ(&mut p, "X");
        assert_eq!(p.input, "~/code/nXew");
        p.key(&named(NamedKey::Backspace));
        p.key(&named(NamedKey::Delete));
        assert_eq!(p.input, "~/code/nw");
        p.key(&KeyEvent::new(Key::Char('a'), Mods::CTRL));
        assert_eq!(p.cursor, 0);
        p.key(&KeyEvent::new(Key::Char('e'), Mods::CTRL));
        assert_eq!(p.cursor, p.input.len());
        p.key(&KeyEvent::new(Key::Char('w'), Mods::CTRL));
        assert_eq!(p.input, "~/code/");
        p.key(&KeyEvent::new(Key::Char('w'), Mods::CTRL));
        assert_eq!(p.input, "~/");
        typ(&mut p, "tmp/x");
        p.key(&named(NamedKey::Home));
        p.key(&named(NamedKey::Right));
        p.key(&named(NamedKey::Right));
        p.key(&KeyEvent::new(Key::Char('u'), Mods::CTRL));
        assert_eq!((p.input.as_str(), p.cursor), ("tmp/x", 0));
        p.key(&named(NamedKey::End));
        // A choice made then dropped with esc: enter submits the text.
        let mut p = PathPicker::new("~/code/", fs());
        typ(&mut p, "fresh");
        p.key(&named(NamedKey::Down));
        assert_eq!(p.sel, None, "nothing matches");
        assert_eq!(
            p.key(&named(NamedKey::Enter)),
            Outcome::Submit("~/code/fresh".into())
        );
        assert_eq!(p.resolved(), "/home/u/code/fresh");
        let mut p = PathPicker::new("~/", fs());
        p.key(&named(NamedKey::Down));
        assert_eq!(p.key(&named(NamedKey::Escape)), Outcome::Stay);
        assert_eq!(p.sel, None);
        assert_eq!(p.key(&named(NamedKey::Escape)), Outcome::Cancel);
    }

    #[test]
    fn new_workspace_uses_the_picker_and_creates_at_the_expanded_path() {
        use crate::drafts::tests::{commands, fleet, screen};
        let (mut app, mut rxs) = fleet();
        open_with(
            &mut app,
            PromptKind::NewWorkspace,
            "new workspace dir",
            "~/",
            fs(),
        );
        for c in "cod".chars() {
            app.on_key(key(Key::Char(c)));
        }
        app.on_key(named(NamedKey::Tab));
        app.on_paste("vibeke\n".into());
        let Mode::Popup(Popup::Path(p)) = &app.mode else {
            panic!("picker closed: {:?}", app.mode);
        };
        assert_eq!(p.picker.input, "~/code/vibeke ");
        app.on_key(named(NamedKey::Backspace));
        let s = screen(&app);
        assert!(s.contains("> ~/code/vibeke"), "{s}");
        assert!(s.contains("vibeke-site/"), "{s}");
        assert!(s.contains("git"), "{s}");
        commands(&mut rxs[0]);
        app.on_key(named(NamedKey::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        let cmds = commands(&mut rxs[0]);
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert_eq!(cmds[0].1, "workspace.create");
        assert_eq!(cmds[0].2["cwd"], "/home/u/code/vibeke");
        assert_eq!(cmds[0].2["focus"], true);
        // Esc closes without a command.
        open_with(&mut app, PromptKind::NewWorkspace, "x", "~/", fs());
        app.on_key(named(NamedKey::Escape));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(commands(&mut rxs[0]).is_empty());
    }

    #[test]
    fn the_binding_opens_the_picker_on_a_local_machine_only() {
        let (mut app, _rx) = crate::app::test_app(2);
        app.action("new_workspace", None);
        let Mode::Popup(Popup::Path(p)) = &app.mode else {
            panic!("no picker: {:?}", app.mode);
        };
        assert_eq!(p.picker.input, "~/");
        assert_eq!(p.kind, PromptKind::NewWorkspace);
        // A remote machine's disk isn't listed: the picker is a plain path input there.
        app.mode = Mode::Normal;
        app.cur = 1;
        app.action("new_workspace", None);
        let Mode::Popup(Popup::Path(p)) = &app.mode else {
            panic!("no picker: {:?}", app.mode);
        };
        assert!(p.picker.matches().is_empty());
    }

    #[test]
    fn reads_the_local_disk() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("repo/.git")).unwrap();
        std::fs::create_dir_all(t.path().join("Plain")).unwrap();
        std::fs::create_dir_all(t.path().join(".hidden")).unwrap();
        std::fs::write(t.path().join("file"), "x").unwrap();
        let v = LocalDirs.list(t.path(), false).unwrap();
        assert_eq!(
            v,
            [
                DirEntry {
                    name: "Plain".into(),
                    git_repo: false
                },
                DirEntry {
                    name: "repo".into(),
                    git_repo: true
                }
            ]
        );
        let v = LocalDirs.list(t.path(), true).unwrap();
        assert_eq!(v.len(), 3);
        assert!(LocalDirs.list(&t.path().join("missing"), false).is_none());
    }
}
