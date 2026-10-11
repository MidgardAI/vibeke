//! Pane screenshots for the CI run page (18 §10.5).
//!
//! The plugin jobs' result directories are hostile data (18 §5): plugin code ran with write
//! access to them, and that includes the SVGs the test rendered. This module never uses those
//! images. It reads the result files with the closed [`PluginResult`] schema, loads the
//! canonical grid files they point to, redacts the grids again, computes each diff again (it
//! never trusts `diff_cells`) and renders one contact sheet PNG with the trusted renderer.
//!
//! Limits: result files up to [`MAX_RESULT_BYTES`], grid files up to [`MAX_GRID_BYTES`] (the
//! largest file the result unpacker keeps), at most [`MAX_GRIDS`] grid entries, grids up to
//! [`MAX_COLS`] x [`MAX_ROWS`] cells, relative paths without `..` that do not pass through a
//! symbolic link, and a sheet of at most [`SHEET_MAX_W`] x [`SHEET_MAX_H`] pixels.

use crate::diff::{GridDiff, compare};
use crate::grid::Grid;
use crate::render::{self, Canvas};
use crate::result::{PluginResult, SCHEMA};
use anyhow::{Context, bail};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const MAX_RESULT_BYTES: u64 = 1024 * 1024;
/// A pretty-printed 120x40 grid is already larger than 1 MiB, so grid files may be as large as
/// `scripts/herdr-plugins/unpack_results.py` allows.
pub const MAX_GRID_BYTES: u64 = 8 * 1024 * 1024;
/// Grid entries per result file, and comparisons per sheet.
pub const MAX_GRIDS: usize = 64;
/// Result files read from one directory.
pub const MAX_RESULTS: usize = 64;
pub const MAX_COLS: u16 = 300;
pub const MAX_ROWS: u16 = 120;
pub const MAX_PATH: usize = 256;
pub const SHEET_MAX_W: u32 = 4096;
pub const SHEET_MAX_H: u32 = 16384;
/// Longest plugin, step or pane name shown in a label or the summary, in characters.
const MAX_NAME: usize = 80;

const PAD: u32 = 8;
const SEP: u32 = 24;

/// One pane comparison: the grids of one step and pane, redacted, with the recomputed diff.
#[derive(Debug, Clone)]
pub struct Shot {
    pub plugin: String,
    pub os: String,
    pub step: String,
    pub pane: String,
    pub placement_matches: Option<bool>,
    pub herdr: Option<Grid>,
    pub vibeke: Option<Grid>,
    /// `None` unless both sides have a grid.
    pub diff: Option<GridDiff>,
}

#[derive(Debug, Default)]
pub struct Collected {
    pub shots: Vec<Shot>,
    /// Grid entries past [`MAX_GRIDS`].
    pub dropped: usize,
    /// Why files or entries were left out. The messages can hold hostile text: escape them
    /// before showing them.
    pub skipped: Vec<String>,
}

/// A relative path inside the results directory: at most [`MAX_PATH`] bytes of
/// `[A-Za-z0-9_.-/]`, not absolute, with no empty, `.` or `..` segments. The same rule as
/// `relPath` in `scripts/herdr-plugins-report.ts`.
pub fn check_rel_path(rel: &str) -> anyhow::Result<()> {
    if rel.is_empty() || rel.len() > MAX_PATH {
        bail!("path is empty or longer than {MAX_PATH} bytes");
    }
    if !rel
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_.-/".contains(&b))
    {
        bail!("path has characters outside [A-Za-z0-9_.-/]");
    }
    if rel.starts_with('/') {
        bail!("absolute path");
    }
    if rel
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        bail!("path has empty, '.' or '..' segments");
    }
    Ok(())
}

/// Reads `dir/rel` when it is a regular file of at most `limit` bytes and no part of `rel` is a
/// symbolic link.
fn read_in(dir: &Path, rel: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    check_rel_path(rel)?;
    let parts: Vec<&str> = rel.split('/').collect();
    let mut path = dir.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        let meta = fs::symlink_metadata(&path).with_context(|| format!("{rel}: not found"))?;
        let last = i + 1 == parts.len();
        if meta.file_type().is_symlink() {
            bail!("{rel}: passes through a symbolic link");
        }
        if last && !meta.is_file() {
            bail!("{rel}: not a regular file");
        }
        if !last && !meta.is_dir() {
            bail!("{rel}: not a directory on the way");
        }
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("{rel}: cannot open"))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("{rel}: not a regular file");
    }
    if meta.len() > limit {
        bail!("{rel}: larger than {limit} bytes");
    }
    let mut buf = Vec::new();
    file.take(limit + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        bail!("{rel}: larger than {limit} bytes");
    }
    Ok(buf)
}

/// One result file directly inside `dir`, with the closed schema and at most [`MAX_GRIDS`]
/// grid entries.
pub fn load_result(dir: &Path, name: &str) -> anyhow::Result<PluginResult> {
    if name.contains('/') {
        bail!("{name}: not directly in the results directory");
    }
    let bytes = read_in(dir, name, MAX_RESULT_BYTES)?;
    let r: PluginResult =
        serde_json::from_slice(&bytes).with_context(|| format!("{name}: not a plugin result"))?;
    if r.schema != SCHEMA {
        bail!("{name}: schema is not {SCHEMA}");
    }
    let grids: usize = r.steps.iter().map(|s| s.grids.len()).sum();
    if grids > MAX_GRIDS {
        bail!("{name}: {grids} grid entries, more than {MAX_GRIDS}");
    }
    Ok(r)
}

/// One canonical grid file, `rel` relative to `dir`: exactly the serialized [`Grid`] shape (no
/// unknown fields), 1 to [`MAX_COLS`] columns, 1 to [`MAX_ROWS`] rows, every line `cols` cells.
pub fn load_grid(dir: &Path, rel: &str) -> anyhow::Result<Grid> {
    let bytes = read_in(dir, rel, MAX_GRID_BYTES)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("{rel}: not JSON"))?;
    let grid = Grid::deserialize(&value).with_context(|| format!("{rel}: not a grid"))?;
    // Serializing the parsed grid gives back the same JSON only when the file had no unknown
    // or missing fields.
    if serde_json::to_value(&grid)? != value {
        bail!("{rel}: unknown or missing fields");
    }
    if !(1..=MAX_COLS).contains(&grid.cols) || !(1..=MAX_ROWS).contains(&grid.rows) {
        bail!(
            "{rel}: {}x{} cells, outside 1x1 to {MAX_COLS}x{MAX_ROWS}",
            grid.cols,
            grid.rows
        );
    }
    if grid.cells.len() != grid.rows as usize
        || grid.cells.iter().any(|l| l.len() != grid.cols as usize)
    {
        bail!("{rel}: lines do not match the grid size");
    }
    Ok(grid)
}

fn short(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_NAME).collect();
    if s.chars().count() > MAX_NAME {
        out.push_str("...");
    }
    out
}

/// Every pane comparison in the result files directly inside `dir`, in file name order. Files
/// and entries that fail a check are left out and listed in [`Collected::skipped`].
pub fn collect(dir: &Path) -> anyhow::Result<Collected> {
    let meta = fs::symlink_metadata(dir).with_context(|| format!("{}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut c = Collected::default();
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        match entry.file_name().into_string() {
            Ok(name) if name.ends_with(".json") => names.push(name),
            Ok(_) => {}
            Err(_) => c.skipped.push("a file name that is not UTF-8".into()),
        }
    }
    names.sort();
    if names.len() > MAX_RESULTS {
        c.skipped.push(format!(
            "{} result files past the first {MAX_RESULTS}",
            names.len() - MAX_RESULTS
        ));
        names.truncate(MAX_RESULTS);
    }
    for name in names {
        let r = match load_result(dir, &name) {
            Ok(r) => r,
            Err(e) => {
                c.skipped.push(format!("{e:#}"));
                continue;
            }
        };
        for step in &r.steps {
            for art in &step.grids {
                if art.herdr.is_none() && art.vibeke.is_none() {
                    continue;
                }
                if c.shots.len() >= MAX_GRIDS {
                    c.dropped += 1;
                    continue;
                }
                let load = |rel: &Option<String>| {
                    rel.as_deref()
                        .map(|rel| load_grid(dir, rel).map(|g| render::redact(&g)))
                        .transpose()
                };
                let (herdr, vibeke) = match (load(&art.herdr), load(&art.vibeke)) {
                    (Ok(h), Ok(v)) => (h, v),
                    (Err(e), _) | (_, Err(e)) => {
                        c.skipped.push(format!("{name}: step {}: {e:#}", step.id));
                        continue;
                    }
                };
                let diff = match (&herdr, &vibeke) {
                    (Some(h), Some(v)) => Some(compare(h, v)),
                    _ => None,
                };
                c.shots.push(Shot {
                    plugin: short(&r.plugin_id),
                    os: short(&r.os),
                    step: short(&step.id),
                    pane: short(&art.pane),
                    placement_matches: art.placement_matches,
                    herdr,
                    vibeke,
                    diff,
                });
            }
        }
    }
    Ok(c)
}

fn placement_text(m: Option<bool>) -> &'static str {
    match m {
        Some(true) => "matches",
        Some(false) => "differs",
        None => "not checked",
    }
}

fn diff_text(d: Option<&GridDiff>) -> String {
    match d {
        None => "not compared".into(),
        Some(d) if d.is_empty() => "identical".into(),
        Some(d) => format!(
            "{} cells{}",
            d.cells.len(),
            if d.same_size { "" } else { ", size differs" }
        ),
    }
}

impl Shot {
    /// The one-line label above the comparison in the sheet, redacted.
    fn label(&self) -> String {
        let s = format!(
            "{} ({})  step {}  pane {}  placement {}  diff {}",
            self.plugin,
            self.os,
            self.step,
            self.pane,
            placement_text(self.placement_matches),
            diff_text(self.diff.as_ref()),
        );
        vk_redact::redact(&s).into_owned()
    }
}

/// The contact sheet: a heading, then for each comparison a label line and Herdr | Vibeke |
/// diff. Returns the PNG and how many comparisons it shows. Comparisons that do not fit in
/// [`SHEET_MAX_W`] x [`SHEET_MAX_H`] are left out, and with `dropped` they are counted in a
/// last "N more not shown" line.
pub fn sheet(shots: &[Shot], dropped: usize) -> (Vec<u8>, usize) {
    sheet_within(shots, dropped, SHEET_MAX_W, SHEET_MAX_H)
}

fn sheet_within(shots: &[Shot], dropped: usize, max_w: u32, max_h: u32) -> (Vec<u8>, usize) {
    let line = render::PX_LINE_H;
    let inner = max_w - 2 * PAD;
    let heading = render::fit(
        &format!(
            "Herdr 0.9.3 | Vibeke | diff: {} comparisons",
            shots.len() + dropped
        ),
        inner,
    );
    let mut width = render::text_px_width(&heading);
    let mut y = PAD + line + 8;
    let mut placed = Vec::new();
    for s in shots {
        let (bw, bh) = render::side_by_side_px_size(
            s.herdr.as_ref(),
            s.vibeke.as_ref(),
            s.diff.as_ref(),
            inner,
        );
        let label = render::fit(&s.label(), inner);
        let h = line + 4 + bh + SEP;
        // Keep room for the "not shown" line and the margin.
        if y + h + line + PAD > max_h {
            break;
        }
        width = width.max(bw).max(render::text_px_width(&label));
        placed.push((s, y, label));
        y += h;
    }
    let not_shown = shots.len() - placed.len() + dropped;
    let footer = (not_shown > 0).then(|| format!("{not_shown} more not shown"));
    if let Some(f) = &footer {
        width = width.max(render::text_px_width(f));
    }
    let mut cv = Canvas::new(
        (width + 2 * PAD).min(max_w),
        y + footer.as_ref().map_or(0, |_| line) + PAD,
    );
    cv.text(PAD, PAD + 2, &heading, (0, 0, 0), true);
    for (s, top, label) in &placed {
        cv.text(PAD, top + 2, label, (0, 0, 0), true);
        render::draw_side_by_side(
            &mut cv,
            PAD,
            top + line + 4,
            s.herdr.as_ref(),
            s.vibeke.as_ref(),
            s.diff.as_ref(),
            inner,
        );
    }
    if let Some(f) = &footer {
        cv.text(PAD, y + 2, f, (0x55, 0x55, 0x55), false);
    }
    (cv.encode(), placed.len())
}

/// Escapes text for a Markdown table cell on GitHub, as `md` in
/// `scripts/herdr-plugins-report.ts` does: every ASCII punctuation character gets a backslash,
/// `<`, `>` and `&` become entities, line breaks become spaces, and a zero-width space after
/// `@`, `#`, `://` and `www.` keeps GitHub from making mentions, references or links.
pub fn md(s: &str) -> String {
    const ZWSP: char = '\u{200b}';
    let forbidden = |c: char| {
        matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    };
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let ch = if forbidden(ch) { '\u{fffd}' } else { ch };
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\n' | '\r' | '\t' => out.push(' '),
            '@' | '#' => {
                out.push('\\');
                out.push(ch);
                out.push(ZWSP);
            }
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    let out = out.replace("\\:\\/\\/", &format!("\\:{ZWSP}\\/\\/"));
    // `www\.` in any case gets a zero-width space after `www`.
    let mut fixed = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.to_ascii_lowercase().find("www\\.") {
        fixed.push_str(&rest[..i + 3]);
        fixed.push(ZWSP);
        rest = &rest[i + 3..];
    }
    fixed.push_str(rest);
    fixed
}

/// A short Markdown summary of the comparisons, every value escaped with [`md`].
pub fn summary_md(c: &Collected, shown: usize) -> String {
    let mut out = String::new();
    if c.shots.is_empty() {
        out.push_str("No pane grids to show.\n");
    } else {
        out.push_str("| Plugin | OS | Step | Pane | Placement | Diff |\n");
        out.push_str("| --- | --- | --- | --- | --- | --- |\n");
        for s in &c.shots {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                md(&s.plugin),
                md(&s.os),
                md(&s.step),
                md(&s.pane),
                md(placement_text(s.placement_matches)),
                md(&diff_text(s.diff.as_ref())),
            ));
        }
    }
    let not_shown = c.shots.len().saturating_sub(shown) + c.dropped;
    if not_shown > 0 {
        out.push_str(&format!(
            "\n{not_shown} comparisons are not in the image.\n"
        ));
    }
    if !c.skipped.is_empty() {
        out.push_str(&format!(
            "\n{} files or entries failed validation and were left out.\n",
            c.skipped.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::DependencyMode;
    use crate::outcome::{Outcome, SideResult};
    use crate::result::{CoverageSummary, GridArtifact, StepResult};
    use std::path::PathBuf;

    fn grid(text: &str) -> Grid {
        let mut g = Grid::blank(text.chars().count() as u16, 1);
        for (x, ch) in text.chars().enumerate() {
            g.cells[0][x].text = ch.to_string();
        }
        g
    }

    fn art(pane: &str, herdr: Option<&str>, vibeke: Option<&str>) -> GridArtifact {
        GridArtifact {
            pane: pane.into(),
            vibeke: vibeke.map(Into::into),
            herdr: herdr.map(Into::into),
            image: Some("x-linux/evil.svg".into()),
            // A lie: the diff is recomputed.
            diff_cells: Some(0),
            placement_matches: Some(true),
        }
    }

    fn result(grids: Vec<GridArtifact>) -> PluginResult {
        PluginResult {
            schema: SCHEMA.into(),
            plugin_id: "acme.demo".into(),
            repository: "acme/demo".into(),
            rank: 1,
            os: "linux".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            dependency_mode: DependencyMode::Verified,
            reproducible: true,
            outcome: Outcome::Passed,
            fails_run: false,
            surfaces: vec![],
            coverage: CoverageSummary {
                ok: true,
                missing: vec![],
                skipped: vec![],
            },
            lifecycle: vec![],
            steps: vec![StepResult {
                id: "open".into(),
                kind: "action".into(),
                outcome: Outcome::Passed,
                vibeke: SideResult::Passed,
                herdr: SideResult::Passed,
                known: None,
                unexpected_pass: false,
                reason: None,
                evidence: vec![],
                grids,
                duration_ms: 1,
            }],
            duration_ms: 1,
        }
    }

    fn write(dir: &Path, rel: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    fn write_grid(dir: &Path, rel: &str, g: &Grid) {
        write(
            dir,
            rel,
            serde_json::to_string_pretty(g).unwrap().as_bytes(),
        );
    }

    fn write_result(dir: &Path, name: &str, r: &PluginResult) {
        write(dir, name, serde_json::to_string(r).unwrap().as_bytes());
    }

    #[test]
    fn rel_paths() {
        assert!(check_rel_path("acme-linux/open__main.vibeke.json").is_ok());
        for bad in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            "./a",
            "a//b",
            "a/",
            "a b",
            "a\\b",
            "a:b",
        ] {
            assert!(check_rel_path(bad).is_err(), "{bad:?}");
        }
        assert!(check_rel_path(&"a".repeat(MAX_PATH + 1)).is_err());
    }

    #[test]
    fn result_limits() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_result(d, "ok.json", &result(vec![]));
        assert!(load_result(d, "ok.json").is_ok());

        let mut v = serde_json::to_value(result(vec![])).unwrap();
        v["extra"] = serde_json::json!(1);
        write(d, "extra.json", v.to_string().as_bytes());
        assert!(load_result(d, "extra.json").is_err());

        let mut r = result(vec![]);
        r.schema = "other/1".into();
        write_result(d, "schema.json", &r);
        assert!(load_result(d, "schema.json").is_err());

        let many = (0..=MAX_GRIDS).map(|i| art(&format!("p{i}"), None, None));
        write_result(d, "many.json", &result(many.collect()));
        let e = load_result(d, "many.json").unwrap_err();
        assert!(format!("{e:#}").contains("grid entries"), "{e:#}");

        write(d, "big.json", &vec![b' '; MAX_RESULT_BYTES as usize + 1]);
        let e = load_result(d, "big.json").unwrap_err();
        assert!(format!("{e:#}").contains("larger than"), "{e:#}");

        std::os::unix::fs::symlink(d.join("ok.json"), d.join("link.json")).unwrap();
        let e = load_result(d, "link.json").unwrap_err();
        assert!(format!("{e:#}").contains("symbolic link"), "{e:#}");
    }

    #[test]
    fn grid_limits() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_grid(d, "g/ok.json", &grid("abc"));
        assert_eq!(load_grid(d, "g/ok.json").unwrap(), grid("abc"));

        write_grid(d, "g/wide.json", &Grid::blank(MAX_COLS + 1, 1));
        assert!(load_grid(d, "g/wide.json").is_err());
        write_grid(d, "g/tall.json", &Grid::blank(1, MAX_ROWS + 1));
        assert!(load_grid(d, "g/tall.json").is_err());
        write_grid(d, "g/empty.json", &Grid::blank(0, 0));
        assert!(load_grid(d, "g/empty.json").is_err());

        let mut short_line = grid("abc");
        short_line.cells[0].pop();
        write_grid(d, "g/short.json", &short_line);
        assert!(load_grid(d, "g/short.json").is_err());

        let mut v = serde_json::to_value(grid("abc")).unwrap();
        v["cells"][0][0]["extra"] = serde_json::json!(true);
        write(d, "g/extra.json", v.to_string().as_bytes());
        let e = load_grid(d, "g/extra.json").unwrap_err();
        assert!(format!("{e:#}").contains("unknown"), "{e:#}");

        write(d, "g/big.json", &vec![b' '; MAX_GRID_BYTES as usize + 1]);
        assert!(load_grid(d, "g/big.json").is_err());

        // A symbolic link anywhere on the path is refused.
        std::os::unix::fs::symlink(d.join("g"), d.join("h")).unwrap();
        let e = load_grid(d, "h/ok.json").unwrap_err();
        assert!(format!("{e:#}").contains("symbolic link"), "{e:#}");
        assert!(load_grid(d, "../g/ok.json").is_err());
        assert!(load_grid(d, "g").is_err(), "a directory");
    }

    fn png_size(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        let be = |i: usize| u32::from_be_bytes(png[i..i + 4].try_into().unwrap());
        (be(16), be(20))
    }

    #[test]
    fn collects_and_renders_a_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_grid(d, "x-linux/open__main.herdr.json", &grid("abc"));
        write_grid(d, "x-linux/open__main.vibeke.json", &grid("abd"));
        write_grid(d, "x-linux/open__side.vibeke.json", &grid("side"));
        let grids = vec![
            art(
                "main",
                Some("x-linux/open__main.herdr.json"),
                Some("x-linux/open__main.vibeke.json"),
            ),
            art("side", None, Some("x-linux/open__side.vibeke.json")),
            art("gone", None, Some("x-linux/missing.json")),
            art("none", None, None),
        ];
        write_result(d, "x-linux.json", &result(grids));
        write(d, "junk.json", b"{}");
        write(d, "notes.txt", b"ignored");

        let c = collect(d).unwrap();
        assert_eq!(c.shots.len(), 2);
        assert_eq!(c.skipped.len(), 2, "{:?}", c.skipped);
        let main = &c.shots[0];
        assert_eq!(main.pane, "main");
        assert_eq!(main.diff.as_ref().unwrap().cells.len(), 1, "recomputed");
        assert!(c.shots[1].diff.is_none());

        let (png, shown) = sheet(&c.shots, c.dropped);
        assert_eq!(shown, 2);
        let (w, h) = png_size(&png);
        assert!(w <= SHEET_MAX_W && h <= SHEET_MAX_H);
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!((img.width(), img.height()), (w, h));

        let md = summary_md(&c, shown);
        assert!(
            md.contains("| acme\\.demo | linux | open | main | matches | 1 cells |"),
            "{md}"
        );
        assert!(md.contains("not compared"));
        assert!(md.contains("2 files or entries failed validation"));
        assert!(!md.contains("not in the image"));
    }

    #[test]
    fn sheet_says_how_many_are_not_shown() {
        let shot = Shot {
            plugin: "p".into(),
            os: "linux".into(),
            step: "s".into(),
            pane: "main".into(),
            placement_matches: None,
            herdr: None,
            vibeke: Some(Grid::blank(80, 24)),
            diff: None,
        };
        let shots = vec![shot; 5];
        let (png, shown) = sheet_within(&shots, 3, 2000, 1200);
        assert_eq!(shown, 2);
        let (w, h) = png_size(&png);
        assert!(w <= 2000 && h <= 1200, "{w}x{h}");
        let c = Collected {
            shots,
            dropped: 3,
            skipped: vec![],
        };
        assert!(summary_md(&c, shown).contains("6 comparisons are not in the image"));
    }

    #[test]
    fn empty_directory_has_no_shots() {
        let dir = tempfile::tempdir().unwrap();
        let c = collect(dir.path()).unwrap();
        assert!(c.shots.is_empty());
        assert_eq!(summary_md(&c, 0), "No pane grids to show.\n");
        assert!(collect(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn md_escapes_markup() {
        assert_eq!(md("a|b"), "a\\|b");
        assert_eq!(md("<img>"), "&lt;img&gt;");
        assert_eq!(md("@me #1"), "\\@\u{200b}me \\#\u{200b}1");
        assert_eq!(md("a\nb\u{1b}"), "a b\u{fffd}");
        assert_eq!(md("http://x"), "http\\:\u{200b}\\/\\/x");
        assert_eq!(md("WWW.x"), "WWW\u{200b}\\.x");
        assert_eq!(md("[x](y)"), "\\[x\\]\\(y\\)");
    }
}
