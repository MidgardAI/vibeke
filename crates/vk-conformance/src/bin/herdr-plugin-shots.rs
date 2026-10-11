//! `herdr-plugin-shots <results-dir> <out.png> [--summary <file.md>]`
//!
//! Renders the pane screenshots of one plugin job as one contact sheet PNG (18 §10.5). The CI
//! workflow runs it as the trusted runner user after the plugin boundary, on the results that
//! `scripts/herdr-plugins/unpack_results.py` unpacked. It treats them as hostile data (18 §5):
//! see [`vk_conformance::shots`].
//!
//! Exit status: 0 when the image was written or there were no grids (then no image is
//! written), 1 when the results directory cannot be read or the output cannot be written, 2 for
//! bad arguments.

use std::path::PathBuf;
use std::process::ExitCode;
use vk_conformance::shots;

const USAGE: &str = "usage: herdr-plugin-shots <results-dir> <out.png> [--summary <file.md>]";

struct Args {
    dir: PathBuf,
    out: PathBuf,
    summary: Option<PathBuf>,
}

fn parse(args: &[String]) -> Option<Args> {
    let mut plain = Vec::new();
    let mut summary = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--summary" {
            summary = Some(PathBuf::from(it.next()?));
        } else if a.starts_with('-') {
            return None;
        } else {
            plain.push(PathBuf::from(a));
        }
    }
    let [dir, out] = <[PathBuf; 2]>::try_from(plain).ok()?;
    Some(Args { dir, out, summary })
}

fn run(a: &Args) -> anyhow::Result<()> {
    let c = shots::collect(&a.dir)?;
    for s in &c.skipped {
        // Hostile text: print it escaped.
        eprintln!("skipped: {}", s.escape_default());
    }
    let shown = if c.shots.is_empty() {
        println!("No pane grids in {}; no image written.", a.dir.display());
        0
    } else {
        let (png, shown) = shots::sheet(&c.shots, c.dropped);
        std::fs::write(&a.out, png)?;
        println!(
            "Wrote {}: {shown} of {} pane comparisons.",
            a.out.display(),
            c.shots.len() + c.dropped
        );
        shown
    };
    if let Some(p) = &a.summary {
        std::fs::write(p, shots::summary_md(&c, shown))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(a) = parse(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match run(&a) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdr-plugin-shots: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Option<Args> {
        parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_arguments() {
        let a = args(&["dir", "out.png", "--summary", "s.md"]).unwrap();
        assert_eq!(a.dir, PathBuf::from("dir"));
        assert_eq!(a.out, PathBuf::from("out.png"));
        assert_eq!(a.summary, Some(PathBuf::from("s.md")));
        assert!(args(&["dir", "out.png"]).unwrap().summary.is_none());
        assert!(args(&["dir"]).is_none());
        assert!(args(&["dir", "out.png", "extra"]).is_none());
        assert!(args(&["dir", "out.png", "--summary"]).is_none());
        assert!(args(&["dir", "out.png", "--other"]).is_none());
    }
}
