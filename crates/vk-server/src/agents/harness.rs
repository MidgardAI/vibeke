//! Built-in harness knowledge (04 §6): Claude Code, Codex, pi and omp are code-backed; OpenCode,
//! Gemini CLI, generic ACP agents and user/repo manifests (`Harness::Custom`) are driven by
//! their manifest (04 §5, `vk_agents::manifest`) plus a per-family signal mapping.

use super::manifests;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use vk_agents::manifest::Loaded;
use vk_proto::model::*;

/// Index of a manifest slot in the process-wide registry (`agents::manifests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ManifestRef(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Harness {
    Claude,
    Codex,
    Pi,
    Omp,
    OpenCode,
    Gemini,
    /// A manifest-defined harness: user (`espi`), repo (`repo:<id>`), built-in screen/self-report
    /// harnesses (`hermes`) and ACP runs (`acp`, `acp:<name>`).
    Custom(ManifestRef),
}

/// Which signal mapping a harness uses: a custom manifest inherits its `extends` root's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Claude,
    Codex,
    Pi,
    Omp,
    OpenCode,
    Gemini,
    Acp,
    Generic,
}

/// Capability matrix rows for the validated versions (04 §2.3; verified in the M0 reality check).
pub mod caps {
    /// Claude `AskUserQuestion` answered via `PreToolUse` `updatedInput` — unverified until the
    /// golden corpus proves it, so questions use best-effort keystrokes.
    pub const CLAUDE_QUESTION_NATIVE: bool = false;
}

const FIXED: [Harness; 6] = [
    Harness::Claude,
    Harness::Codex,
    Harness::Pi,
    Harness::Omp,
    Harness::OpenCode,
    Harness::Gemini,
];

impl Harness {
    pub fn from_id(s: &str) -> Option<Harness> {
        match s {
            "claude" => Some(Harness::Claude),
            "codex" => Some(Harness::Codex),
            "pi" => Some(Harness::Pi),
            "omp" => Some(Harness::Omp),
            "opencode" => Some(Harness::OpenCode),
            "gemini" => Some(Harness::Gemini),
            _ if s.starts_with("acp:") => manifests::synth_acp(s),
            _ => manifests::lookup(s).map(|(r, _)| Harness::Custom(r)),
        }
    }
    pub fn id(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Omp => "omp",
            Harness::OpenCode => "opencode",
            Harness::Gemini => "gemini",
            Harness::Custom(r) => manifests::get(*r).map(|(id, _, _)| id).unwrap_or("unknown"),
        }
    }
    pub fn display(&self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex CLI",
            Harness::Pi => "pi",
            Harness::Omp => "oh-my-pi",
            Harness::OpenCode => "OpenCode",
            Harness::Gemini => "Gemini CLI",
            Harness::Custom(r) => manifests::get(*r).map(|(_, d, _)| d).unwrap_or("unknown"),
        }
    }
    /// This harness's manifest (built-ins included).
    pub fn manifest(&self) -> Option<Arc<Loaded>> {
        match self {
            Harness::Custom(r) => manifests::get(*r).map(|(_, _, l)| l),
            h => manifests::lookup(h.id()).map(|(_, l)| l),
        }
    }
    pub fn family(&self) -> Family {
        match self {
            Harness::Claude => Family::Claude,
            Harness::Codex => Family::Codex,
            Harness::Pi => Family::Pi,
            Harness::Omp => Family::Omp,
            Harness::OpenCode => Family::OpenCode,
            Harness::Gemini => Family::Gemini,
            Harness::Custom(_) => match self.manifest() {
                Some(l) if l.is_acp() => Family::Acp,
                Some(l) => match l.family.as_str() {
                    "claude" => Family::Claude,
                    "codex" => Family::Codex,
                    "pi" => Family::Pi,
                    "omp" => Family::Omp,
                    "opencode" => Family::OpenCode,
                    "gemini" => Family::Gemini,
                    _ => Family::Generic,
                },
                None => Family::Generic,
            },
        }
    }
    /// The built-in harness whose code paths this one uses (itself for ACP/generic manifests).
    pub fn base(&self) -> Harness {
        match self.family() {
            Family::Claude => Harness::Claude,
            Family::Codex => Harness::Codex,
            Family::Pi => Harness::Pi,
            Family::Omp => Harness::Omp,
            Family::OpenCode => Harness::OpenCode,
            Family::Gemini => Harness::Gemini,
            Family::Acp | Family::Generic => *self,
        }
    }
    pub fn is_pi_family(&self) -> bool {
        matches!(self.family(), Family::Pi | Family::Omp)
    }
    /// Signal transport recorded on bound runs (`AgentRun.integration`).
    pub fn transport(&self) -> &'static str {
        match self.family() {
            Family::Claude | Family::Codex | Family::Gemini => "hooks",
            Family::Pi | Family::Omp | Family::OpenCode => "extension",
            Family::Acp => "acp",
            Family::Generic => "self_report",
        }
    }
    /// Every harness Vibeke knows: the fixed six plus detectable/ACP manifests.
    pub fn all() -> Vec<Harness> {
        let mut v: Vec<Harness> = FIXED.to_vec();
        for (r, l) in manifests::all() {
            let id = l.m.id.as_str();
            if FIXED.iter().any(|h| h.id() == id) || !manifests::repo_active(&l) {
                continue;
            }
            if !l.m.detect.process.is_empty() || id == "acp" {
                v.push(Harness::Custom(r));
            }
        }
        v
    }
    pub fn capabilities(&self) -> Vec<String> {
        let fixed: &[&str] = match self.family() {
            Family::Claude => &[
                "observe",
                "gate",
                "answer_native:approval",
                "answer_native:plan_review",
                "answer_keystroke",
                "resume",
                "survive_disconnect",
            ],
            Family::Codex => &[
                "observe",
                "gate",
                "answer_native:approval",
                "answer_keystroke",
                "resume",
                "survive_disconnect",
            ],
            // No Vibeke gate for pi/omp (04 §6.3): observe + extension dialogs via the
            // uiContext wrapper (unverified per version until golden-tested).
            Family::Pi | Family::Omp => &[
                "observe",
                "answer_native:extension_dialog",
                "reconcile",
                "resume",
                "survive_disconnect",
            ],
            // Manifest-driven: verified rows only (OpenCode/Gemini have none yet → observe +
            // keystrokes; a user manifest may assert more, shown as user-asserted).
            Family::OpenCode | Family::Gemini | Family::Acp | Family::Generic => {
                return self
                    .manifest()
                    .map(|l| l.capabilities(None, "tui"))
                    .unwrap_or_else(|| vec!["observe".into()]);
            }
        };
        fixed.iter().map(|s| s.to_string()).collect()
    }
    pub fn answer_native(&self, kind: InteractionKind) -> bool {
        match (self.family(), kind) {
            (
                Family::Claude | Family::Codex | Family::Pi | Family::Omp,
                InteractionKind::Approval,
            ) => true,
            (Family::Claude, InteractionKind::PlanReview) => true,
            (Family::Claude, InteractionKind::Question) => caps::CLAUDE_QUESTION_NATIVE,
            // pi/omp: dialogs raised through the extension bridge are answered natively.
            (Family::Pi | Family::Omp, InteractionKind::Question) => true,
            (Family::OpenCode | Family::Gemini | Family::Acp | Family::Generic, k) => self
                .capabilities()
                .iter()
                .any(|c| c == &format!("answer_native:{}", k.as_str())),
            _ => false,
        }
    }
    pub fn keystroke_answers(&self, kind: InteractionKind) -> bool {
        matches!(
            kind,
            InteractionKind::Approval | InteractionKind::Question | InteractionKind::PlanReview
        )
    }
    pub fn preassign_session_id(&self) -> Option<String> {
        match self {
            Harness::Claude | Harness::Pi => Some(uuid_v4()),
            Harness::Codex | Harness::Omp => None,
            _ => self
                .manifest()
                .filter(|l| {
                    l.m.launch.session_id_format == "uuid"
                        && !l.m.launch.argv_with_session.is_empty()
                })
                .map(|_| uuid_v4()),
        }
    }
    pub fn launch_argv(
        &self,
        session: Option<&str>,
        args: &[String],
        prompt: Option<&str>,
    ) -> Vec<String> {
        if matches!(
            self,
            Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp
        ) {
            let mut v = vec![self.id().to_string()];
            if let (Harness::Claude | Harness::Pi, Some(s)) = (self, session) {
                v.push("--session-id".into());
                v.push(s.into());
            }
            v.extend(args.iter().cloned());
            if let Some(p) = prompt {
                v.push(p.to_string());
            }
            return v;
        }
        let Some(l) = self.manifest() else {
            let mut v = vec![self.id().to_string()];
            v.extend(args.iter().cloned());
            return v;
        };
        if l.is_acp() {
            // ACP: the host runs in the pane and drives the agent over stdio (04 §6.6).
            let bin = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "vibeke".into());
            let mut v = vec![bin, "acp-host".into(), "--harness".into(), self.id().into()];
            if let Some(p) = prompt {
                v.push("--prompt".into());
                v.push(p.into());
            }
            v.push("--".into());
            v.extend(l.m.launch.acp_argv.iter().cloned());
            v.extend(args.iter().cloned());
            return v;
        }
        let mut v = match session {
            Some(s) if !l.m.launch.argv_with_session.is_empty() => {
                vk_agents::manifest::expand(&l.m.launch.argv_with_session, &[("session_id", s)])
            }
            _ if !l.m.launch.argv.is_empty() => l.m.launch.argv.clone(),
            _ => vec![self.id().to_string()],
        };
        v.extend(args.iter().cloned());
        if let Some(p) = prompt {
            match l.m.launch.prompt_arg.as_str() {
                "none" => {}
                f if f.starts_with("flag:") => {
                    v.push(f["flag:".len()..].to_string());
                    v.push(p.to_string());
                }
                _ => v.push(p.to_string()),
            }
        }
        v
    }
    pub fn resume_argv(&self, session: &str) -> Vec<String> {
        match self {
            Harness::Claude => vec!["claude".into(), "--resume".into(), session.into()],
            Harness::Codex => vec!["codex".into(), "resume".into(), session.into()],
            Harness::Pi => vec!["pi".into(), "--session".into(), session.into()],
            Harness::Omp => vec!["omp".into(), "--resume".into(), session.into()],
            _ => self
                .manifest()
                .map(|l| l.resume_argv(session))
                .unwrap_or_default(),
        }
    }
}

fn uuid_v4() -> String {
    let b: [u8; 16] = std::array::from_fn(|_| rand::random::<u8>());
    let mut b = b;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Match a process (argv, exe) to a harness (04 §5.2): native binaries, node/bun shims, nix
/// wrappers.
pub fn detect_harness(argv: &[String], exe: Option<&str>) -> Option<Harness> {
    let base = |s: &str| {
        s.rsplit('/')
            .next()
            .unwrap_or(s)
            .trim_start_matches('-')
            .to_string()
    };
    let a0 = argv.first().map(|s| base(s)).unwrap_or_default();
    let exe_b = exe.map(base).unwrap_or_default();
    for name in [a0.as_str(), exe_b.as_str()] {
        match name {
            "pi" => return Some(Harness::Pi),
            "omp" => return Some(Harness::Omp),
            "claude" | ".claude-wrapped" => return Some(Harness::Claude),
            "codex"
            | ".codex-wrapped"
            | "codex-aarch64-apple-darwin"
            | "codex-x86_64-unknown-linux-musl"
            | "codex-aarch64-unknown-linux-musl" => {
                return Some(Harness::Codex);
            }
            _ => {}
        }
    }
    // Shell-script wrappers: `sh /path/to/claude …` (shebang scripts are exec'd this way).
    if matches!(a0.as_str(), "sh" | "bash" | "zsh" | "dash")
        && let Some(script) = argv.get(1).filter(|a| !a.starts_with('-'))
    {
        match base(script).as_str() {
            "claude" => return Some(Harness::Claude),
            "codex" => return Some(Harness::Codex),
            "pi" => return Some(Harness::Pi),
            "omp" => return Some(Harness::Omp),
            _ => {}
        }
    }
    // Interpreter + script path (node/bun shims).
    if matches!(a0.as_str(), "node" | "bun" | "deno") {
        for a in argv.iter().skip(1).take(3) {
            if a.contains("@anthropic-ai/claude-code")
                || a.contains("/claude-code/")
                || a.ends_with("/claude")
            {
                return Some(Harness::Claude);
            }
            if a.contains("@openai/codex") || a.ends_with("/codex") || a.ends_with("/codex.js") {
                return Some(Harness::Codex);
            }
            if a.contains("pi-coding-agent") && !a.contains("oh-my-pi") || a.ends_with("/pi") {
                return Some(Harness::Pi);
            }
            if a.contains("oh-my-pi") || a.ends_with("/omp") {
                return Some(Harness::Omp);
            }
        }
    }
    if let Some(e) = exe {
        if e.contains("-claude-code-") {
            return Some(Harness::Claude);
        }
        if e.contains("/codex/") && e.ends_with("/codex") {
            return Some(Harness::Codex);
        }
    }
    None
}

/// User-typed yolo flags (13 §3.1): informational badge only, never blocked.
pub fn yolo(h: Harness, argv: &[String]) -> bool {
    let has = |f: &str| argv.iter().any(|a| a == f);
    let pair = |a: &str, b: &str| {
        argv.windows(2).any(|w| w[0] == a && w[1] == b)
            || argv.iter().any(|x| x == &format!("{a}={b}"))
    };
    match h {
        Harness::Claude => {
            has("--dangerously-skip-permissions") || pair("--permission-mode", "bypassPermissions")
        }
        Harness::Codex => {
            has("--dangerously-bypass-approvals-and-sandbox")
                || has("--yolo")
                || ((pair("-a", "never") || pair("--ask-for-approval", "never"))
                    && (pair("-s", "danger-full-access")
                        || pair("--sandbox", "danger-full-access")))
        }
        // pi has no permission system by design; omp with approvals off.
        Harness::Pi => true,
        Harness::Omp => pair("--approval", "off") || has("--yolo"),
        Harness::OpenCode | Harness::Gemini | Harness::Custom(_) => {
            h.manifest().is_some_and(|l| l.yolo(argv))
        }
    }
}

/// Detection over a pane's foreground process tree (04 §5.2): a `vibeke acp-host --harness X`
/// host wins outright; then manifests with priority above 100 (user wrappers such as `espi`
/// whose real harness runs as a descendant); then the code-backed built-ins (priority 100);
/// then the remaining manifests (OpenCode, Gemini, Hermes, `repo:*`).
pub fn detect_tree(
    procs: &[(Vec<String>, Option<String>)],
    cwd: Option<&Path>,
) -> Option<(Harness, Vec<String>)> {
    for (argv, _) in procs {
        if let Some(i) = argv.iter().position(|a| a == "acp-host")
            && argv
                .first()
                .is_some_and(|a0| a0.rsplit('/').next() == Some("vibeke") || i > 0)
        {
            let id = argv
                .windows(2)
                .find(|w| w[0] == "--harness")
                .map(|w| w[1].clone())
                .unwrap_or_else(|| "acp".into());
            if let Some(h) = Harness::from_id(&id).filter(|h| h.family() == Family::Acp) {
                return Some((h, argv.clone()));
            }
        }
    }
    let all = manifests::all();
    let set = vk_agents::manifest::Set {
        manifests: all
            .iter()
            .filter(|(_, l)| manifests::repo_active(l))
            .map(|(_, l)| (**l).clone())
            .collect(),
        warnings: vec![],
    };
    let code = |id: &str| vk_agents::manifest::CODE_BACKED.contains(&id);
    let found = set.detect(procs, cwd, &code);
    let pick = |l: &Loaded, i: usize| Harness::from_id(&l.m.id).map(|h| (h, procs[i].0.clone()));
    if let Some((l, i)) = found
        && l.m.detect.priority > 100
    {
        return pick(l, i);
    }
    if let Some(x) = procs
        .iter()
        .find_map(|(argv, exe)| detect_harness(argv, exe.as_deref()).map(|h| (h, argv.clone())))
    {
        return Some(x);
    }
    found.and_then(|(l, i)| pick(l, i))
}

pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn tool_summary(tool: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "Bash" | "shell" | "exec_command" => {
            format!("{tool}: {}", s("command").lines().next().unwrap_or(""))
        }
        "Edit" | "Write" | "MultiEdit" | "Read" | "NotebookEdit" => {
            format!("{tool} {}", s("file_path"))
        }
        "WebFetch" => format!("WebFetch {}", s("url")),
        "Grep" | "Glob" => format!("{tool} {}", s("pattern")),
        "Task" | "Agent" => format!("{tool}: {}", s("description")),
        _ => tool.to_string(),
    }
}

/// Phase 1 risk heuristic (04 §7.6), shared with the installers crate.
pub fn risk(tool: &str, command: Option<&str>, paths: &[String]) -> (Risk, Vec<String>) {
    let (r, reasons) = vk_agents::assess(tool, command, paths, None);
    let r = match r {
        vk_agents::Risk::High => Risk::High,
        vk_agents::Risk::Medium => Risk::Medium,
        vk_agents::Risk::Low => Risk::Low,
        vk_agents::Risk::Unknown => Risk::Unknown,
    };
    (r, reasons)
}

fn mini_diff(old: &str, new: &str) -> String {
    let mut out = String::new();
    for l in old.lines().take(40) {
        out.push_str(&format!("-{l}\n"));
    }
    for l in new.lines().take(40) {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

fn blank_interaction(
    kind: InteractionKind,
    title: String,
    native_ref: Option<String>,
) -> Interaction {
    Interaction {
        id: crate::core::ulid(),
        handle: String::new(),
        run: String::new(),
        pane: String::new(),
        kind,
        status: InteractionStatus::Open,
        title,
        body_md: None,
        action: None,
        questions: vec![],
        plan_md: None,
        answer_channel: AnswerChannel::Native,
        native_ref,
        source: StateSource::Structured,
        confidence: 1.0,
        answerable: true,
        gate: false,
        decision_rev: 0,
        delivery: DeliveryState::None,
        delivery_error: None,
        answer: None,
        answered_by: None,
        answer_key: None,
        opened_at_ms: vk_store::now_ms(),
        answered_at_ms: None,
    }
}

/// Map a gate-capable hook payload to an Interaction (04 §6.1.1, §6.1.2, §6.2).
pub fn interaction_from_hook(h: Harness, event: &str, p: &Value) -> Option<Interaction> {
    if h.is_pi_family() && event == "Dialog" {
        return Some(dialog_interaction(p));
    }
    match h.family() {
        Family::OpenCode if event == "permission.ask" => {
            return Some(super::opencode::permission_interaction(p));
        }
        Family::Acp if event == "RequestPermission" => {
            return Some(super::acp::permission_interaction(p));
        }
        _ => {}
    }
    let tool = p
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let native_ref = p
        .get("tool_use_id")
        .or_else(|| p.get("call_id"))
        .or_else(|| p.get("turn_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let _ = h;
    if tool == "AskUserQuestion" {
        let qs = input
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut it = blank_interaction(
            InteractionKind::Question,
            qs.first()
                .and_then(|q| q.get("question"))
                .and_then(Value::as_str)
                .unwrap_or("question")
                .to_string(),
            native_ref,
        );
        it.questions = qs
            .iter()
            .enumerate()
            .map(|(i, q)| Question {
                id: q
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or(&format!("q{i}"))
                    .to_string(),
                prompt: q
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                multi: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|os| {
                        os.iter()
                            .map(|o| {
                                let label = o
                                    .get("label")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                QuestionOption {
                                    id: label.clone(),
                                    label,
                                    description: o
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                allow_free_text: true,
            })
            .collect();
        return Some(it);
    }
    if event == "PreToolUse" {
        return None;
    }
    if tool == "ExitPlanMode" {
        let mut it = blank_interaction(
            InteractionKind::PlanReview,
            "review plan".into(),
            native_ref,
        );
        it.plan_md = input
            .get("plan")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Some(it);
    }
    let command = input.get("command").and_then(|c| {
        c.as_str().map(str::to_string).or_else(|| {
            c.as_array().map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
    });
    let path = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let paths: Vec<String> = path.into_iter().collect();
    let diff = match (
        input.get("old_string").and_then(Value::as_str),
        input.get("new_string").and_then(Value::as_str),
        input.get("content").and_then(Value::as_str),
    ) {
        (Some(o), Some(n), _) => Some(mini_diff(o, n)),
        (_, _, Some(c)) => Some(mini_diff("", c)),
        _ => None,
    };
    let (risk, reasons) = risk(&tool, command.as_deref(), &paths);
    let summary = tool_summary(&tool, &input);
    let mut it = blank_interaction(InteractionKind::Approval, summary.clone(), native_ref);
    it.action = Some(ActionInfo {
        tool,
        summary,
        command,
        paths,
        diff,
        risk,
        risk_reasons: reasons,
    });
    Some(it)
}

/// Hook stdout JSON for a decision (04 §6.1.1, §6.2). Shapes pinned by the M0 reality check.
/// An extension dialog (pi/omp uiContext `confirm`/`select`/`input`, DESIGN §4.1).
fn dialog_interaction(p: &Value) -> Interaction {
    let method = p.get("method").and_then(Value::as_str).unwrap_or("confirm");
    let title = p
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("extension dialog")
        .to_string();
    let message = p.get("message").and_then(Value::as_str).map(str::to_string);
    let native_ref = p
        .get("dialog_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let looks_permission = {
        let t = format!("{title} {}", message.clone().unwrap_or_default()).to_lowercase();
        [
            "allow",
            "approve",
            "permission",
            "run ",
            "execute",
            "proceed",
        ]
        .iter()
        .any(|k| t.contains(k))
    };
    match method {
        "confirm" if looks_permission => {
            let mut it = blank_interaction(InteractionKind::Approval, title.clone(), native_ref);
            let (risk, reasons) = risk("extension", message.as_deref(), &[]);
            it.action = Some(ActionInfo {
                tool: "extension dialog".into(),
                summary: title,
                command: message.clone(),
                paths: vec![],
                diff: None,
                risk,
                risk_reasons: reasons,
            });
            it.body_md = message;
            it
        }
        "confirm" => {
            let mut it = blank_interaction(InteractionKind::Question, title.clone(), native_ref);
            it.body_md = message;
            it.questions = vec![Question {
                id: "confirm".into(),
                prompt: title,
                header: None,
                multi: false,
                options: vec![
                    QuestionOption {
                        id: "yes".into(),
                        label: "Yes".into(),
                        description: None,
                    },
                    QuestionOption {
                        id: "no".into(),
                        label: "No".into(),
                        description: None,
                    },
                ],
                allow_free_text: false,
            }];
            it
        }
        _ => {
            let mut it = blank_interaction(InteractionKind::Question, title.clone(), native_ref);
            it.body_md = message;
            let options: Vec<QuestionOption> = p
                .get("options")
                .and_then(Value::as_array)
                .map(|o| {
                    o.iter()
                        .filter_map(Value::as_str)
                        .map(|l| QuestionOption {
                            id: l.into(),
                            label: l.into(),
                            description: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            it.questions = vec![Question {
                id: "answer".into(),
                prompt: title,
                header: None,
                multi: false,
                allow_free_text: options.is_empty(),
                options,
            }];
            it
        }
    }
}

pub fn decision_json(h: Harness, it: &Interaction, a: &Answer) -> Value {
    match h.family() {
        // `permission.ask` hook output (04 §6.4): allow-always has no hook equivalent.
        Family::OpenCode => {
            let allow = matches!(a.decision, Some(Decision::Allow | Decision::AllowAlways));
            return json!({"status": if allow { "allow" } else { "deny" }});
        }
        Family::Acp => return super::acp::decision_json(it, a),
        _ => {}
    }
    if h.is_pi_family() {
        // Returned to the calling extension through the uiContext wrapper.
        let value = match it.kind {
            InteractionKind::Approval => json!(matches!(
                a.decision,
                Some(Decision::Allow | Decision::AllowAlways)
            )),
            _ => {
                let choice = a.choices.first().and_then(|(_, o)| o.first()).cloned();
                match (it.questions.first().map(|q| q.id.as_str()), choice) {
                    (Some("confirm"), Some(c)) => json!(c == "yes"),
                    (_, Some(c)) => json!(c),
                    (_, None) => json!(a.text.clone().unwrap_or_default()),
                }
            }
        };
        return json!({"value": value});
    }
    let deny_msg = a
        .text
        .clone()
        .unwrap_or_else(|| "Denied from Vibeke".into());
    if it.kind == InteractionKind::Question {
        // Native AskUserQuestion answer (only used when caps::CLAUDE_QUESTION_NATIVE).
        let mut answers = serde_json::Map::new();
        for (q, opts) in &a.choices {
            answers.insert(q.clone(), json!(opts.join(", ")));
        }
        return json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "updatedInput": {"answers": answers}}});
    }
    let decision = match a.decision {
        Some(Decision::Allow) => json!({"behavior": "allow"}),
        Some(Decision::AllowAlways) => match (h, it.action.as_ref()) {
            (Harness::Claude, Some(act)) => {
                let rule = act
                    .command
                    .as_ref()
                    .and_then(|c| c.split_whitespace().next().map(|w| format!("{w}:*")));
                json!({"behavior": "allow", "updatedPermissions": [{"type": "addRules", "rules": [{"toolName": act.tool, "ruleContent": rule}], "behavior": "allow", "destination": "session"}]})
            }
            _ => json!({"behavior": "allow"}),
        },
        Some(Decision::Deny) | None => json!({"behavior": "deny", "message": deny_msg}),
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}})
}

// ---- transcripts (04 §10) -----------------------------------------------------------------------

fn tail_lines(path: &Path, max_bytes: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return vec![];
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    let _ = f.seek(SeekFrom::Start(start));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

fn message_text(v: &Value) -> Option<String> {
    let msg = v.get("message")?;
    match msg.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let t: Vec<String> = a
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|c| c.get("text").and_then(Value::as_str).map(str::to_string))
                .collect();
            (!t.is_empty()).then(|| t.join("\n"))
        }
        _ => None,
    }
}

/// Last assistant text from a Claude JSONL transcript (Codex rollouts use `response_item`).
pub fn transcript_last_message(path: &Path) -> Option<String> {
    for l in tail_lines(path, 256 * 1024).iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("assistant")
            && let Some(t) = message_text(&v)
        {
            return Some(t);
        }
        if v.get("type").and_then(Value::as_str) == Some("response_item")
            && v.pointer("/payload/role").and_then(Value::as_str) == Some("assistant")
            && let Some(t) = v.pointer("/payload/content/0/text").and_then(Value::as_str)
        {
            return Some(t.to_string());
        }
    }
    None
}

pub fn transcript_turns(path: &Path, limit: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for l in tail_lines(path, 2 << 20) {
        let Ok(v) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        let role = v.get("type").and_then(Value::as_str).unwrap_or("");
        if (role == "user" || role == "assistant")
            && let Some(t) = message_text(&v)
        {
            out.push(json!({"role": role, "text": t, "ts": v.get("timestamp")}));
        }
    }
    let n = out.len();
    out.split_off(n.saturating_sub(limit))
}

pub fn transcript_tail(path: &Path, limit: usize) -> String {
    transcript_turns(path, limit)
        .iter()
        .map(|t| {
            format!(
                "[{}] {}",
                t["role"].as_str().unwrap_or(""),
                t["text"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Validated version ranges per harness (04 §2.2, §12.3). Outside them a run gets only
/// `observe` (+ keystrokes): native answering is withheld until the golden corpus covers it.
/// The ranges are the versions the adapter was written and tested against in Goal 01 (see
/// spec/04-harness-adapters.md: live golden fixtures still pending).
pub fn validated(h: Harness, version: &str) -> bool {
    let v: Vec<u64> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|x| x.parse().unwrap_or(0))
        .collect();
    let (major, minor) = (
        v.first().copied().unwrap_or(0),
        v.get(1).copied().unwrap_or(0),
    );
    match h {
        Harness::Claude => major == 2 && minor == 1,
        Harness::Codex => major == 0 && (157..=160).contains(&minor),
        Harness::Pi => major == 0 && (84..=90).contains(&minor),
        Harness::Omp => major == 17,
        // A custom wrapper of a code-backed harness (espi → pi) reports the real harness's version.
        Harness::Custom(_)
            if matches!(
                h.family(),
                Family::Claude | Family::Codex | Family::Pi | Family::Omp
            ) =>
        {
            validated(h.base(), version)
        }
        // Manifest-driven: the manifest's validated range, or a user/repo manifest asserting a
        // `*` capability row ("user-asserted", 04 §13). OpenCode and Gemini ship no range yet.
        _ => h.manifest().is_some_and(|l| {
            l.validated(version)
                || (matches!(
                    l.source,
                    vk_agents::manifest::Source::User(_) | vk_agents::manifest::Source::Repo { .. }
                ) && l
                    .m
                    .capabilities
                    .iter()
                    .any(|r| r.verified && r.versions.trim() == "*"))
        }),
    }
}

/// `<harness> --version` (manifest `[version] command` for manifest-driven harnesses). `None`
/// when the binary is absent or no command is declared (ACP runs).
pub fn version(h: Harness) -> Option<String> {
    let (cmd, manifest) = match h {
        Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp => {
            (vec![h.id().to_string(), "--version".to_string()], None)
        }
        _ => {
            let l = h.manifest()?;
            if l.is_acp() || l.m.version.command.is_empty() {
                return None;
            }
            (l.m.version.command.clone(), Some(l))
        }
    };
    let out = std::process::Command::new(cmd.first()?)
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    match manifest {
        Some(l) => l.parse_version(&text),
        None => parse_version(&text),
    }
}

/// First version-looking token: `2.1.290 (Claude Code)`, `codex-cli 0.160.1`, `omp/17.2.12`, `v0.84.1`.
pub fn parse_version(s: &str) -> Option<String> {
    s.split(|c: char| c.is_whitespace() || c == '/')
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))
        .map(str::to_string)
}

/// The PATH shim for user-typed `codex` (04 §6.2): exec the real codex later in PATH with a
/// per-pane embedded app-server, all user arguments untouched.
pub fn codex_shim_script() -> String {
    r#"#!/bin/sh
# managed by vibeke: per-pane embedded Codex app-server so hooks carry this pane's identity
# (spec 04 §6.2). Disable with CODEX_VIBEKE_SHIM=0.
VIBEKE_CODEX_EXTRA="--disable daemon_auto_start"
self_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
real=""
IFS_SAVE=$IFS; IFS=:
for d in $PATH; do
  [ "$d" = "$self_dir" ] && continue
  if [ -x "$d/codex" ] && [ ! -d "$d/codex" ]; then real="$d/codex"; break; fi
done
IFS=$IFS_SAVE
[ -n "$real" ] || { echo "codex: not found in PATH" >&2; exit 127; }
if [ "${CODEX_VIBEKE_SHIM:-1}" = "0" ]; then exec "$real" "$@"; fi
for a in "$@"; do
  case "$a" in daemon_auto_start*|--remote*|app-server|mcp-server|login|logout|completion|--version|-V|--help|-h) exec "$real" "$@";; esac
done
exec "$real" $VIBEKE_CODEX_EXTRA "$@"
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_and_yolo() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            detect_harness(&a(&["claude", "--resume", "x"]), None),
            Some(Harness::Claude)
        );
        assert_eq!(
            detect_harness(
                &a(&[
                    "node",
                    "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
                ]),
                None
            ),
            Some(Harness::Claude)
        );
        assert_eq!(
            detect_harness(
                &a(&["codex", "-a", "never"]),
                Some("/opt/homebrew/bin/codex")
            ),
            Some(Harness::Codex)
        );
        assert_eq!(detect_harness(&a(&["-zsh"]), Some("/bin/zsh")), None);
        assert!(yolo(
            Harness::Claude,
            &a(&["claude", "--dangerously-skip-permissions"])
        ));
        assert!(yolo(
            Harness::Codex,
            &a(&["codex", "-a", "never", "-s", "danger-full-access"])
        ));
        assert!(!yolo(Harness::Codex, &a(&["codex", "-a", "never"])));
    }

    #[test]
    fn hook_mapping_and_decisions() {
        let p = json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build && pnpm test"}, "tool_use_id": "toolu_1"});
        let it = interaction_from_hook(Harness::Claude, "PermissionRequest", &p).unwrap();
        assert_eq!(it.kind, InteractionKind::Approval);
        assert_eq!(it.native_ref.as_deref(), Some("toolu_1"));
        assert_eq!(it.action.as_ref().unwrap().risk, Risk::High);
        let d = decision_json(
            Harness::Claude,
            &it,
            &Answer {
                decision: Some(Decision::Deny),
                choices: vec![],
                text: Some("no".into()),
            },
        );
        assert_eq!(d["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(d["hookSpecificOutput"]["decision"]["message"], "no");
        let d = decision_json(
            Harness::Claude,
            &it,
            &Answer {
                decision: Some(Decision::AllowAlways),
                ..Default::default()
            },
        );
        assert_eq!(
            d["hookSpecificOutput"]["decision"]["updatedPermissions"][0]["destination"],
            "session"
        );
        let plan = json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "1. do x"}});
        assert_eq!(
            interaction_from_hook(Harness::Claude, "PermissionRequest", &plan)
                .unwrap()
                .kind,
            InteractionKind::PlanReview
        );
        let q = json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Which?", "options": [{"label": "A"}, {"label": "B"}], "multiSelect": false}]}});
        let it = interaction_from_hook(Harness::Claude, "PreToolUse", &q).unwrap();
        assert_eq!(it.questions[0].options.len(), 2);
        assert_eq!(risk("Bash", Some("pnpm test"), &[]).0, Risk::Low);
        assert_eq!(risk("Bash", Some("pnpm add zod"), &[]).0, Risk::Medium);
    }

    #[test]
    fn pi_dialogs_and_detection() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            detect_harness(
                &a(&[
                    "node",
                    "/opt/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
                ]),
                None
            ),
            Some(Harness::Pi)
        );
        assert_eq!(
            detect_harness(&a(&["omp"]), Some("/Users/x/.bun/bin/omp")),
            Some(Harness::Omp)
        );
        let perm = interaction_from_hook(Harness::Pi, "Dialog", &json!({"method": "confirm", "title": "Allow bash?", "message": "rm -rf dist", "dialog_id": "dlg1"})).unwrap();
        assert_eq!(perm.kind, InteractionKind::Approval);
        assert_eq!(
            decision_json(
                Harness::Pi,
                &perm,
                &Answer {
                    decision: Some(Decision::Allow),
                    ..Default::default()
                }
            ),
            json!({"value": true})
        );
        assert_eq!(
            decision_json(
                Harness::Pi,
                &perm,
                &Answer {
                    decision: Some(Decision::Deny),
                    ..Default::default()
                }
            ),
            json!({"value": false})
        );
        let sel = interaction_from_hook(Harness::Omp, "Dialog", &json!({"method": "select", "title": "Pick", "options": ["A", "B"], "dialog_id": "dlg2"})).unwrap();
        assert_eq!(sel.questions[0].options.len(), 2);
        let ans = Answer {
            choices: vec![("answer".into(), vec!["B".into()])],
            ..Default::default()
        };
        assert_eq!(
            decision_json(Harness::Omp, &sel, &ans),
            json!({"value": "B"})
        );
        assert_eq!(Harness::Pi.resume_argv("s1"), vec!["pi", "--session", "s1"]);
        assert!(yolo(Harness::Pi, &a(&["pi"])));
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn version_strings() {
        assert_eq!(parse_version("omp/17.2.12\n").as_deref(), Some("17.2.12"));
        assert_eq!(parse_version("0.84.1").as_deref(), Some("0.84.1"));
        assert_eq!(
            parse_version("codex-cli 0.160.1").as_deref(),
            Some("0.160.1")
        );
        assert_eq!(
            parse_version("2.1.290 (Claude Code)").as_deref(),
            Some("2.1.290")
        );
    }

    #[test]
    fn validated_ranges() {
        assert!(validated(Harness::Claude, "2.1.290"));
        assert!(!validated(Harness::Claude, "3.0.0"));
        assert!(validated(Harness::Codex, "0.160.1"));
        assert!(!validated(Harness::Codex, "0.170.0"));
    }
}

#[cfg(test)]
mod m2_tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn manifest_detection_across_the_tree() {
        let t = |procs: Vec<(Vec<String>, Option<String>)>| {
            detect_tree(&procs, None).map(|(h, _)| h.id().to_string())
        };
        assert_eq!(
            t(vec![(
                a(&["opencode"]),
                Some("/opt/homebrew/bin/opencode".into())
            )])
            .as_deref(),
            Some("opencode")
        );
        assert_eq!(
            t(vec![(
                a(&[
                    "node",
                    "/usr/local/lib/node_modules/@google/gemini-cli/dist/index.js"
                ]),
                None
            )])
            .as_deref(),
            Some("gemini")
        );
        assert_eq!(t(vec![(a(&["claude"]), None)]).as_deref(), Some("claude"));
        // A user wrapper (priority 200) beats its pi descendant; the wrapper keeps pi's family.
        let espi = manifests::test_register(vk_agents::manifest::EXAMPLES[0].1);
        assert_eq!(espi.family(), Family::Pi);
        assert_eq!(espi.base(), Harness::Pi);
        assert!(espi.is_pi_family());
        assert_eq!(espi.transport(), "extension");
        assert_eq!(
            t(vec![
                (a(&["pi"]), Some("/usr/local/bin/pi".into())),
                (a(&["bash", "/home/e/bin/espi", "-c"]), None),
            ])
            .as_deref(),
            Some("espi")
        );
        assert_eq!(espi.resume_argv("s9"), a(&["espi", "--session", "s9"]));
        assert!(
            validated(espi, "0.84.1"),
            "a pi wrapper is gated on pi's range"
        );
        assert!(
            yolo(espi, &a(&["espi"])),
            "inherits pi's no-permission-system yolo"
        );
        // The ACP host wins outright and names its harness.
        let host = a(&[
            "/x/vibeke",
            "acp-host",
            "--harness",
            "acp:gemini",
            "--",
            "gemini",
            "--experimental-acp",
        ]);
        assert_eq!(
            t(vec![
                (host.clone(), None),
                (a(&["gemini", "--experimental-acp"]), None)
            ])
            .as_deref(),
            Some("acp:gemini")
        );
    }

    #[test]
    fn new_harnesses_are_observe_only_until_validated() {
        for h in [Harness::OpenCode, Harness::Gemini] {
            assert_eq!(h.capabilities(), vec!["observe", "answer_keystroke"]);
            assert!(!h.answer_native(InteractionKind::Approval));
            assert!(!validated(h, "1.2.3"));
            assert!(h.keystroke_answers(InteractionKind::Approval));
        }
        assert!(yolo(
            Harness::Gemini,
            &a(&["gemini", "--approval-mode=yolo"])
        ));
        assert!(!yolo(Harness::OpenCode, &a(&["opencode"])));
        assert_eq!(
            Harness::Gemini.resume_argv("3"),
            a(&["gemini", "--resume", "3"])
        );
        assert_eq!(
            Harness::OpenCode.launch_argv(None, &[], Some("fix it")),
            a(&["opencode", "--prompt", "fix it"])
        );
        // A user manifest asserting a `*` row is "user-asserted": granted and validated.
        let mybot = manifests::test_register(
            "id = \"mybot\"\n[[detect.process]]\nexe_basename = [\"mybot\"]\n[version]\ncommand = [\"mybot\", \"--version\"]\n[[capabilities]]\nversions = \"*\"\nanswer_native = [\"approval\"]\ngate = true\n",
        );
        assert_eq!(mybot.family(), Family::Generic);
        assert!(mybot.answer_native(InteractionKind::Approval));
        assert!(validated(mybot, "0.0.1"));
        assert!(Harness::all().contains(&mybot));
    }

    #[test]
    fn acp_harnesses_launch_through_the_host() {
        let h = Harness::from_id("acp:gemini").unwrap();
        assert_eq!(h.family(), Family::Acp);
        assert_eq!(h.transport(), "acp");
        assert_eq!(h.display(), "Gemini CLI (ACP)");
        assert!(h.answer_native(InteractionKind::Approval));
        let argv = h.launch_argv(None, &[], Some("hello"));
        assert_eq!(
            &argv[1..5],
            &a(&["acp-host", "--harness", "acp:gemini", "--prompt"])[..]
        );
        assert_eq!(
            &argv[argv.len() - 2..],
            &a(&["gemini", "--experimental-acp"])[..]
        );
        let generic = Harness::from_id("acp").unwrap();
        let argv = generic.launch_argv(None, &a(&["python3", "agent.py"]), None);
        assert_eq!(
            &argv[argv.len() - 3..],
            &a(&["--", "python3", "agent.py"])[..]
        );
        assert!(version(h).is_none(), "no version probe for ACP runs");
        assert!(Harness::from_id("acp:").is_none());
        assert_eq!(Harness::from_id("acp:gemini"), Some(h), "synthesized once");
    }

    #[test]
    fn manifest_screen_rules_drive_keystrokes() {
        let screen = "  △ Permission required\n  $ rm -rf dist\n\n  1. Allow once\n  2. Allow always\n  3. Reject\n";
        let m = super::super::screen::evaluate(Harness::OpenCode, screen);
        let d = m.dialog.expect("dialog");
        assert_eq!(d.command.as_deref(), Some("rm -rf dist"));
        let it = interaction_from_hook(
            Harness::Claude,
            "PermissionRequest",
            &json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf dist"}}),
        )
        .unwrap();
        let keys = super::super::screen::keys_for(
            Harness::OpenCode,
            &d,
            &it,
            &Answer {
                decision: Some(Decision::Deny),
                ..Default::default()
            },
        );
        assert_eq!(keys, Some(a(&["3"])));
        // Hermes has no rules of its own: it uses generic-repl's.
        let h = Harness::from_id("hermes").unwrap();
        let m = super::super::screen::evaluate(h, "Run this command?\n  1. Yes\n  2. No\n");
        assert!(m.dialog.is_some());
    }
}
