//! Git sources for `vibeke plugin install owner/repo[/subdir][@ref] [--ref R]` (07 §7.6).
//!
//! A source resolves to `<base>/<owner>/<repo>` (`https://github.com/` unless a test redirects
//! it) and is fetched with a shallow, hardened git: a fresh repository (no template hooks), one
//! `fetch --depth 1` of the requested ref (a tag, branch or commit sha; the remote's `HEAD` when
//! none), a detached checkout of exactly that commit, no submodules, no filters, no fsmonitor,
//! protocols limited to https and file, no credential prompts, and the user's and the system's
//! git configuration ignored. The resolved commit is what the trust grant pins.
//!
//! The URL base can only be overridden by `VIBEKE_PLUGIN_GIT_BASE` when `VIBEKE_TEST_HOOKS=1`
//! (tests use `file://` bare repositories and never touch the network).

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default base for `owner/repo` sources.
pub const DEFAULT_BASE: &str = "https://github.com/";
/// Hard limit for one git step.
const GIT_TIMEOUT: Duration = Duration::from_secs(180);

/// A parsed `owner/repo[/subdir][@ref]` source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub owner: String,
    pub repo: String,
    /// Plugin directory inside the repository (Herdr's `owner/repo/subdir`).
    pub subdir: Option<String>,
    /// Tag, branch or commit sha; `None` means the remote's default branch.
    pub git_ref: Option<String>,
}

impl GitSource {
    /// `owner/repo[/subdir]`.
    pub fn spec(&self) -> String {
        match &self.subdir {
            Some(s) => format!("{}/{}/{s}", self.owner, self.repo),
            None => format!("{}/{}", self.owner, self.repo),
        }
    }

    /// The repository URL below `base`.
    pub fn url(&self, base: &str) -> String {
        format!(
            "{}/{}/{}",
            base.trim_end_matches('/'),
            self.owner,
            self.repo
        )
    }
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with(['-', '.'])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A ref that cannot be taken for a git option or a path trick.
fn ref_ok(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.contains(['\0', ' ', '~', '^', ':', '?', '*', '[', '\\'])
        && r.chars().all(|c| !c.is_control())
}

/// Parse `arg` (plus `--ref`) as a git source. `Ok(None)` means `arg` does not have the shape
/// of `owner/repo…` (a local path); `Err` is a malformed source.
pub fn parse(arg: &str, flag_ref: Option<&str>) -> Result<Option<GitSource>, String> {
    if arg.starts_with(['.', '/', '~']) || arg.contains("://") || arg.contains('\\') {
        return Ok(None);
    }
    let (path, at_ref) = match arg.split_once('@') {
        Some((p, r)) => (p, Some(r)),
        None => (arg, None),
    };
    let mut parts = path.split('/');
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let rest: Vec<&str> = parts.collect();
    if !name_ok(owner) || !name_ok(repo) {
        return Ok(None);
    }
    let subdir = if rest.is_empty() {
        None
    } else {
        let sub = rest.join("/");
        if rest
            .iter()
            .any(|c| c.is_empty() || *c == "." || *c == ".." || *c == ".git")
        {
            return Err(format!("{arg}: invalid subdirectory"));
        }
        Some(sub)
    };
    let git_ref = match (at_ref, flag_ref) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!("{arg}: `@{a}` and `--ref {b}` disagree"));
        }
        (Some(a), _) => Some(a),
        (None, b) => b,
    };
    if let Some(r) = git_ref
        && !ref_ok(r)
    {
        return Err(format!("invalid ref `{r}`"));
    }
    Ok(Some(GitSource {
        owner: owner.into(),
        repo: repo.into(),
        subdir,
        git_ref: git_ref.map(str::to_string),
    }))
}

/// The URL base: `VIBEKE_PLUGIN_GIT_BASE` only under `VIBEKE_TEST_HOOKS=1`.
pub fn base_from_env(get: impl Fn(&str) -> Option<String>) -> String {
    if get("VIBEKE_TEST_HOOKS").as_deref() == Some("1")
        && let Some(b) = get("VIBEKE_PLUGIN_GIT_BASE")
        && !b.is_empty()
    {
        return b;
    }
    DEFAULT_BASE.to_string()
}

pub fn base() -> String {
    base_from_env(|k| std::env::var(k).ok())
}

/// A fetched source: the plugin directory inside a work tree, and what was resolved.
#[derive(Debug)]
pub struct Fetched {
    /// Root of the work tree (contains `.git`); remove it when done.
    pub work: PathBuf,
    /// The plugin directory (`work` or `work/<subdir>`).
    pub plugin_dir: PathBuf,
    /// Resolved 40-hex commit.
    pub commit: String,
    pub url: String,
}

/// Flags every git step runs with. Nothing the fetched repository (or the user's own git
/// configuration) names gets executed.
const HARDEN: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.sshCommand=false",
    "-c",
    "core.alternateRefsCommand=true",
    "-c",
    "core.pager=cat",
    "-c",
    "credential.helper=",
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "protocol.file.allow=always",
    "-c",
    "submodule.recurse=false",
    "-c",
    "gc.auto=0",
    "-c",
    "advice.detachedHead=false",
];

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(HARDEN)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https:file")
        .env("GIT_ASKPASS", "true")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("git: {e}"))?;
    let start = Instant::now();
    // Drain the pipes on threads so a chatty fetch never blocks on a full pipe.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t1 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut b);
        b
    });
    let t2 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut b);
        b
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > GIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("git {} timed out", args.first().unwrap_or(&"")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("git: {e}")),
        }
    };
    let (o, e) = (t1.join().unwrap_or_default(), t2.join().unwrap_or_default());
    if !status.success() {
        let msg = String::from_utf8_lossy(&e);
        return Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            msg.trim().lines().last().unwrap_or("")
        ));
    }
    Ok(String::from_utf8_lossy(&o).trim().to_string())
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Shallow-fetch `src` below `base` into a new directory under `parent` (created, 0700).
pub fn fetch(src: &GitSource, base: &str, parent: &Path) -> Result<Fetched, String> {
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let work = parent.join(format!(
        ".fetch-{}-{}-{}",
        src.repo,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let res = fetch_into(src, base, &work);
    if res.is_err() {
        let _ = std::fs::remove_dir_all(&work);
    }
    res
}

fn fetch_into(src: &GitSource, base: &str, work: &Path) -> Result<Fetched, String> {
    let url = src.url(base);
    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
    git(work, &["init", "--quiet", "--template="])?;
    git(work, &["remote", "add", "origin", "--", &url])?;
    let want = src.git_ref.as_deref().unwrap_or("HEAD");
    let mut args = vec![
        "fetch",
        "--quiet",
        "--depth",
        "1",
        "--no-tags",
        "--no-recurse-submodules",
        "origin",
    ];
    // A tag name needs its full refspec to be fetched without `--tags`; try the plain name
    // first (branch, tag or sha all resolve), and the tag spelling for a tag-only miss.
    args.push(want);
    if let Err(first) = git(work, &args) {
        if is_sha(want) || want == "HEAD" {
            return Err(format!("{}: {first}", src.spec()));
        }
        let tag = format!("refs/tags/{want}:refs/tags/{want}");
        let br = format!("refs/heads/{want}:refs/remotes/origin/{want}");
        let alt = [tag.as_str(), br.as_str()];
        let mut ok = false;
        for r in alt {
            if git(
                work,
                &["fetch", "--quiet", "--depth", "1", "--no-tags", "origin", r],
            )
            .is_ok()
            {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(format!("{}: ref `{want}` not found ({first})", src.spec()));
        }
        let c = git(
            work,
            &["rev-parse", "--verify", &format!("{want}^{{commit}}")],
        )
        .or_else(|_| git(work, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"]))?;
        return finish(src, work, &url, c);
    }
    let commit = git(work, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])?;
    finish(src, work, &url, commit)
}

fn finish(src: &GitSource, work: &Path, url: &str, commit: String) -> Result<Fetched, String> {
    if !is_sha(&commit) {
        return Err(format!("{}: unexpected commit `{commit}`", src.spec()));
    }
    if let Some(want) = &src.git_ref
        && is_sha(want)
        && !commit.eq_ignore_ascii_case(want)
    {
        return Err(format!(
            "{}: resolved {commit}, asked for {want}",
            src.spec()
        ));
    }
    git(
        work,
        &["checkout", "--quiet", "--detach", "--force", &commit],
    )?;
    let plugin_dir = match &src.subdir {
        None => work.to_path_buf(),
        Some(sub) => {
            let p = work.join(sub);
            let canon = std::fs::canonicalize(&p)
                .map_err(|_| format!("{}: no directory `{sub}` in the repository", src.spec()))?;
            let root = std::fs::canonicalize(work).map_err(|e| e.to_string())?;
            if !canon.starts_with(&root)
                || Path::new(sub)
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err(format!(
                    "{}: subdirectory escapes the repository",
                    src.spec()
                ));
            }
            canon
        }
    };
    Ok(Fetched {
        work: work.to_path_buf(),
        plugin_dir,
        commit: commit.to_ascii_lowercase(),
        url: url.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        let p = |a, r| parse(a, r).unwrap().unwrap();
        let s = p("acme/tool", None);
        assert_eq!((s.owner.as_str(), s.repo.as_str()), ("acme", "tool"));
        assert_eq!(s.git_ref, None);
        assert_eq!(p("acme/tool@v1.2", None).git_ref.as_deref(), Some("v1.2"));
        assert_eq!(
            p("acme/tool", Some("main")).git_ref.as_deref(),
            Some("main")
        );
        assert_eq!(
            p("acme/tool@main", Some("main")).git_ref.as_deref(),
            Some("main")
        );
        let s = p("acme/tool/plugins/x@abc", None);
        assert_eq!(s.subdir.as_deref(), Some("plugins/x"));
        assert_eq!(s.spec(), "acme/tool/plugins/x");
        assert!(parse("acme/tool@a", Some("b")).is_err(), "conflicting refs");
        assert!(parse("acme/tool@-x", None).is_err(), "option-looking ref");
        assert!(parse("acme/tool@a..b", None).is_err());
        assert!(parse("acme/tool/../x", None).is_err());
        for local in [
            "./a/b",
            "/a/b",
            "~/a",
            "a",
            "a\\b",
            "https://x/y/z",
            "-a/b",
            "a/.b",
        ] {
            assert_eq!(parse(local, None).unwrap(), None, "{local}");
        }
    }

    #[test]
    fn url_base_only_under_test_hooks() {
        let env = |hooks: bool| {
            move |k: &str| match k {
                "VIBEKE_TEST_HOOKS" => hooks.then(|| "1".to_string()),
                "VIBEKE_PLUGIN_GIT_BASE" => Some("file:///x".to_string()),
                _ => None,
            }
        };
        assert_eq!(base_from_env(env(false)), DEFAULT_BASE);
        assert_eq!(base_from_env(env(true)), "file:///x");
        let s = parse("a/b", None).unwrap().unwrap();
        assert_eq!(s.url("https://github.com/"), "https://github.com/a/b");
    }

    fn g(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A bare repo `<base>/acme/tool` with tag v1 on the first commit and a second commit on main.
    fn repo(base: &Path) -> (String, String) {
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        g(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("herdr-plugin.toml"), "id = 'acme.tool'\n").unwrap();
        g(&work, &["add", "."]);
        g(&work, &["commit", "-qm", "one"]);
        g(&work, &["tag", "v1"]);
        let c1 = g(&work, &["rev-parse", "HEAD"]);
        std::fs::write(
            work.join("herdr-plugin.toml"),
            "id = 'acme.tool'\nversion = '2'\n",
        )
        .unwrap();
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(work.join("sub/herdr-plugin.toml"), "id = 'acme.sub'\n").unwrap();
        g(&work, &["add", "."]);
        g(&work, &["commit", "-qm", "two"]);
        let c2 = g(&work, &["rev-parse", "HEAD"]);
        g(&work, &["branch", "dev", &c1]);
        let bare = base.join("srv/acme/tool");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        g(
            base,
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        (c1, c2)
    }

    #[test]
    fn fetches_default_tag_branch_sha_and_subdir() {
        let t = tempfile::tempdir().unwrap();
        let (c1, c2) = repo(t.path());
        let base = format!("file://{}", t.path().join("srv").display());
        let parent = t.path().join("fetch");
        let get = |spec: &str, r: Option<&str>| {
            let s = parse(spec, r).unwrap().unwrap();
            fetch(&s, &base, &parent)
        };
        let f = get("acme/tool", None).unwrap();
        assert_eq!(f.commit, c2, "default branch head");
        assert!(f.plugin_dir.join("herdr-plugin.toml").is_file());
        let shallow = std::fs::read_to_string(f.work.join(".git/shallow")).unwrap();
        assert!(shallow.contains(&c2), "shallow clone");
        std::fs::remove_dir_all(&f.work).unwrap();
        assert_eq!(get("acme/tool", Some("v1")).unwrap().commit, c1, "tag");
        assert_eq!(get("acme/tool@dev", None).unwrap().commit, c1, "branch");
        assert_eq!(get("acme/tool", Some(&c1)).unwrap().commit, c1, "sha");
        let f = get("acme/tool/sub", None).unwrap();
        assert!(f.plugin_dir.ends_with("sub"));
        assert!(
            get("acme/tool/nope", None)
                .unwrap_err()
                .contains("no directory")
        );
        assert!(
            get("acme/tool", Some("nosuch"))
                .unwrap_err()
                .contains("not found")
        );
        assert!(get("acme/missing", None).is_err());
        // Failed fetches leave no work trees behind.
        let left: Vec<_> = std::fs::read_dir(&parent).unwrap().collect();
        assert_eq!(left.len(), 4, "only the four successful trees remain");
    }

    #[test]
    fn network_protocols_are_limited() {
        // ssh/git/ext URLs cannot be smuggled in through the base under the allowlist.
        let t = tempfile::tempdir().unwrap();
        let s = parse("acme/tool", None).unwrap().unwrap();
        for base in [
            "ext::sh -c touch% /tmp/vk-pwn",
            "git://127.0.0.1:1",
            "ssh://127.0.0.1",
        ] {
            assert!(fetch(&s, base, &t.path().join("f")).is_err(), "{base}");
        }
    }
}
