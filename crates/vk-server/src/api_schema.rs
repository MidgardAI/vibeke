//! The control-API schema registry (07 §1.5, M6): one source for the param/result shape of every
//! method, the payload of every event type and the error kinds. The API docs
//! (`docs/api/methods.json`), the JSON Schema bundle (`vibeke debug api-schema`, `api.schema`,
//! `docs/api/vibeke-1.schema.json`) and the generated TypeScript and Python clients
//! (`clients/`) are all derived from it, and tests keep it honest:
//!
//! - every method in a `METHODS` table has an entry (and no entry names a missing method);
//! - results of real calls and the payloads of real events validate against their shapes
//!   (`api_schema_tests`);
//! - every event type the server's source emits is listed.
//!
//! Shapes are written in the small language of [`crate::shape`], next to each other below, in
//! three registries: [`DEFS`] (shared types, mirroring the `vk-proto` model structs),
//! [`METHOD_SHAPES`] (`method :: params => result`) and [`EVENT_SHAPES`]
//! (`type :: subject => data`). Add a method: add it to its `METHODS` table, then add its line
//! here, then regenerate with `VIBEKE_UPDATE_DOCS=1 VIBEKE_UPDATE_CLIENTS=1`.

use crate::shape::{Field, Shape, parse};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Every method table of the server: (table, rows of (method, mutating)). `api.methods`,
/// the docs generator and the registry coverage test all start from this list.
pub fn method_tables() -> Vec<(&'static str, &'static [(&'static str, bool)])> {
    use crate::*;
    vec![
        ("api", api::METHODS),
        ("agents", agents::METHODS),
        ("review::t4", review::t4::METHODS),
        ("review::ext", review::ext::METHODS),
        ("preview", preview::METHODS),
        ("sandbox", sandbox::METHODS),
        ("agent_browser", agent_browser::METHODS),
        ("browser_pane", browser_pane::METHODS),
        ("parity", parity::METHODS),
        ("screenshots", screenshots::METHODS),
        ("desk", desk::METHODS),
        ("drafts", drafts::METHODS),
        ("assist", assist::METHODS),
        ("compat", compat::METHODS),
        ("notify", notify::METHODS),
        ("theme", theme::METHODS),
        ("layouts", layouts::METHODS),
        ("session_api", session_api::METHODS),
        ("config_api", config_api::METHODS),
        ("blob_api", blob_api::METHODS),
        ("blob_store", blob_store::METHODS),
        ("browse_api", browse_api::METHODS),
        ("handoff_out", handoff_out::METHODS),
        ("hardening", hardening::METHODS),
        ("machines", machines::METHODS),
        ("items", items::METHODS),
        ("pane_api", pane_api::METHODS),
        ("task_park", task_park::METHODS),
        ("security", security::METHODS),
        ("sync_input", sync_input::METHODS),
        ("tab_renumber", tab_renumber::METHODS),
        ("task_lifecycle", task_lifecycle::METHODS),
        ("plugin_native", plugin_native::METHODS),
        ("orch", orch::METHODS),
        ("review::pr", review::pr::METHODS),
        ("review::interval", review::interval::METHODS),
        ("collision", collision::METHODS),
        ("privacy", privacy::METHODS),
    ]
}

/// A parsed registry entry: the shape source text and its parse.
pub struct Entry {
    pub params_src: String,
    pub result_src: String,
    pub params: Shape,
    pub result: Shape,
}

pub struct Registry {
    pub defs: BTreeMap<String, Shape>,
    pub defs_src: BTreeMap<String, String>,
    pub methods: BTreeMap<String, Entry>,
    /// Event type -> (subject, data) with sources.
    pub events: BTreeMap<String, Entry>,
    /// Server pushes (JSON-RPC notifications): method -> params.
    pub notifications: BTreeMap<String, Entry>,
}

/// Split registry text into `(name, lhs, rhs)` entries; an entry starts on a non-indented line
/// containing ` :: ` and continues over indented lines; `#` lines are comments.
fn entries(text: &str) -> Result<Vec<(String, String, String)>, Vec<String>> {
    let mut raw: Vec<String> = vec![];
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            match raw.last_mut() {
                Some(l) => {
                    l.push(' ');
                    l.push_str(line.trim());
                }
                None => return Err(vec![format!("continuation without entry: {line}")]),
            }
        } else {
            raw.push(line.trim().to_string());
        }
    }
    let mut out = vec![];
    let mut errs = vec![];
    for r in raw {
        let Some((name, rest)) = r.split_once(" :: ") else {
            errs.push(format!("no ` :: ` in `{r}`"));
            continue;
        };
        match rest.split_once(" => ") {
            Some((l, rr)) => out.push((name.trim().to_string(), l.trim().into(), rr.trim().into())),
            None => errs.push(format!("{name}: no ` => `")),
        }
    }
    if errs.is_empty() { Ok(out) } else { Err(errs) }
}

fn build() -> Result<Registry, Vec<String>> {
    let mut errs = vec![];
    let mut defs = BTreeMap::new();
    let mut defs_src = BTreeMap::new();
    for line in DEFS
        .lines()
        .chain(BATCH_3D_DEFS.lines())
        .chain(BATCH_3F_DEFS.lines())
        .chain(BATCH_3A_DEFS.lines())
    {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, src)) = line.split_once(" = ") else {
            errs.push(format!("definition without ` = `: {line}"));
            continue;
        };
        match parse(src) {
            Ok(s) => {
                if defs.insert(name.trim().to_string(), s).is_some() {
                    errs.push(format!("duplicate definition {name}"));
                }
                defs_src.insert(name.trim().to_string(), src.trim().to_string());
            }
            Err(e) => errs.push(format!("def {name}: {e}")),
        }
    }
    let load = |text: &[&str], what: &str, errs: &mut Vec<String>| {
        let mut out: BTreeMap<String, Entry> = BTreeMap::new();
        for chunk in text {
            match entries(chunk) {
                Ok(es) => {
                    for (name, l, r) in es {
                        match (parse(&l), parse(&r)) {
                            (Ok(p), Ok(res)) => {
                                let e = Entry {
                                    params_src: l,
                                    result_src: r,
                                    params: p,
                                    result: res,
                                };
                                if out.insert(name.clone(), e).is_some() {
                                    errs.push(format!("{what} {name}: defined twice"));
                                }
                            }
                            (a, b) => {
                                for e in [a.err(), b.err()].into_iter().flatten() {
                                    errs.push(format!("{what} {name}: {e}"));
                                }
                            }
                        }
                    }
                }
                Err(e) => errs.extend(e),
            }
        }
        out
    };
    let methods = load(METHOD_SHAPES, "method", &mut errs);
    let events = load(
        &[
            EVENT_SHAPES,
            BATCH_3D_EVENT_SHAPES,
            BATCH_3B_EVENT_SHAPES,
            BATCH_3F_EVENT_SHAPES,
            BATCH_3A_EVENT_SHAPES,
            crate::orch_shapes::EVENTS,
            crate::handoff_out::EVENTS,
        ],
        "event",
        &mut errs,
    );
    let notifications = load(&[NOTIFICATION_SHAPES], "notification", &mut errs);
    let mut reg = Registry {
        defs,
        defs_src,
        methods,
        events,
        notifications,
    };
    // Every reference resolves.
    let mut refs = vec![];
    let mut each =
        |s: &Shape, who: &str, errs: &mut Vec<String>, defs: &BTreeMap<String, Shape>| {
            refs.clear();
            s.refs(&mut refs);
            for r in &refs {
                if !defs.contains_key(r) {
                    errs.push(format!("{who}: unknown definition `{r}`"));
                }
            }
        };
    for (n, d) in &reg.defs {
        each(d, &format!("def {n}"), &mut errs, &reg.defs);
    }
    for (n, e) in reg
        .methods
        .iter()
        .chain(&reg.events)
        .chain(&reg.notifications)
    {
        each(&e.params, n, &mut errs, &reg.defs);
        each(&e.result, n, &mut errs, &reg.defs);
    }
    // Spec 07 §1.3: every mutating result carries `cursor`; make it part of the shape.
    let mutating: BTreeMap<&str, bool> = method_tables()
        .into_iter()
        .flat_map(|(_, rows)| rows.iter().copied())
        .collect();
    for (name, e) in reg.methods.iter_mut() {
        if mutating.get(name.as_str()) == Some(&true) {
            add_cursor(&mut e.result);
        }
    }
    if errs.is_empty() { Ok(reg) } else { Err(errs) }
}

fn add_cursor(s: &mut Shape) {
    match s {
        Shape::Obj(fs) if !fs.iter().any(|f| f.name == "cursor") => fs.push(Field {
            name: "cursor".into(),
            optional: true,
            shape: Shape::Ref("Cursor".into()),
            default: None,
        }),
        Shape::Object => {
            *s = Shape::Obj(vec![Field {
                name: "cursor".into(),
                optional: true,
                shape: Shape::Ref("Cursor".into()),
                default: None,
            }])
        }
        Shape::Union(arms) => arms.iter_mut().for_each(add_cursor),
        _ => {}
    }
}

/// Parse problems of the registry, empty when it is well formed.
pub fn problems() -> Vec<String> {
    match build() {
        Ok(_) => vec![],
        Err(e) => e,
    }
}

/// The parsed registry (panics with every problem if the text is malformed; the
/// `registry_is_well_formed` test reports them first).
pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| build().unwrap_or_else(|e| panic!("api_schema registry: {e:#?}")))
}

/// Error kinds: (kind, `details` shape). Codes, retryability and the kind list itself come from
/// `vk_proto::rpc::ErrorKind`.
const ERROR_DETAILS: &[(&str, &str)] = &[
    ("not_found", "{object?: string, id?: string}"),
    ("permission_denied", "{scope?: string}"),
    ("conflict", "{reason?: string}"),
    ("timeout", "{last_state?: any}"),
    ("unsupported", "{fallback?: any}"),
    ("truncated", "{earliest_seq?: int}"),
    ("invalid_key", "{key?: string}"),
    ("internal", "{trace_id?: string}"),
];

fn pane_scope_json(method: &str) -> (&'static str, &'static str) {
    let ps = crate::api::pane_scope_of(method).as_str();
    (if ps == "forbidden" { "full" } else { "pane" }, ps)
}

/// The JSON Schema 2020-12 bundle of the running binary: shared types under `$defs`, and
/// `x-methods`, `x-events`, `x-notifications` and `x-errors` holding the shapes. `$ref`s are
/// `#/$defs/<Name>`.
pub fn bundle() -> Value {
    let reg = registry();
    let mutating: BTreeMap<&str, bool> = method_tables()
        .into_iter()
        .flat_map(|(_, rows)| rows.iter().copied())
        .collect();
    let mut defs = Map::new();
    for (n, d) in &reg.defs {
        defs.insert(n.clone(), d.to_schema());
    }
    let mut methods = Map::new();
    for (name, m) in mutating.iter() {
        let (scope, pane_scope) = pane_scope_json(name);
        let (params, result) = match reg.methods.get(*name) {
            Some(e) => (e.params.to_schema(), e.result.to_schema()),
            None => (json!({"type": "object"}), json!({})),
        };
        methods.insert(
            name.to_string(),
            json!({
                "mutating": m,
                "scope": scope,
                "pane_scope": pane_scope,
                "params": params,
                "result": result,
            }),
        );
    }
    let mut events = Map::new();
    for (t, e) in &reg.events {
        events.insert(
            t.clone(),
            json!({"subject": e.params.to_schema(), "data": e.result.to_schema()}),
        );
    }
    let mut notifications = Map::new();
    for (t, e) in &reg.notifications {
        notifications.insert(t.clone(), json!({"params": e.params.to_schema()}));
    }
    let errors: Vec<Value> = vk_proto::rpc::ErrorKind::ALL
        .iter()
        .map(|k| {
            let details = ERROR_DETAILS
                .iter()
                .find(|(n, _)| *n == k.as_str())
                .map(|(_, s)| parse(s).expect("error details shape").to_schema());
            json!({
                "kind": k.as_str(),
                "code": k.code(),
                "retryable": k.retryable(),
                "details": details,
            })
        })
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("urn:vibeke:api:{}", vk_proto::API_VERSION),
        "title": "Vibeke control API",
        "description": "Generated from the schema registry in crates/vk-server/src/api_schema.rs. Methods are JSON-RPC 2.0 over newline-delimited JSON on the session socket; see x-transport.",
        "x-api": vk_proto::API_VERSION,
        "x-transport": {
            "framing": "one JSON object per line (\\n); split on \\n only",
            "max_line_bytes": 16 * 1024 * 1024,
            "jsonrpc": "2.0",
            "socket": "$VIBEKE_RUNTIME_DIR/<session>/vibeke.sock (default session: default)",
            "handshake": "client.hello (optional; pane tokens go in params.token)",
        },
        "$defs": defs,
        "x-methods": methods,
        "x-events": events,
        "x-notifications": notifications,
        "x-errors": errors,
    })
}

/// The docs-friendly source text of a method's shapes (whitespace collapsed).
pub fn method_sources(name: &str) -> Option<(String, String)> {
    let e = registry().methods.get(name)?;
    let one = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    Some((one(&e.params_src), one(&e.result_src)))
}

/// `api.schema {method?}`: the bundle, or one method's entry plus the shared `$defs`.
pub fn api_schema(method: Option<&str>) -> Option<Value> {
    let b = bundle();
    let Some(m) = method else { return Some(b) };
    let entry = b["x-methods"].get(m)?.clone();
    Some(json!({
        "$schema": b["$schema"],
        "$defs": b["$defs"],
        "method": m,
        "x-method": entry,
    }))
}

/// Validate `value` against a named shape of the registry (tests).
pub fn validate_result(method: &str, value: &Value) -> Vec<String> {
    let reg = registry();
    match reg.methods.get(method) {
        Some(e) => e.result.validate(value, &reg.defs),
        None => vec![format!("{method}: no schema")],
    }
}

pub fn validate_params(method: &str, value: &Value) -> Vec<String> {
    let reg = registry();
    match reg.methods.get(method) {
        Some(e) => e.params.validate(value, &reg.defs),
        None => vec![format!("{method}: no schema")],
    }
}

pub fn validate_event(event: &Value) -> Vec<String> {
    let reg = registry();
    let Some(t) = event["type"].as_str() else {
        return vec!["event without type".into()];
    };
    let mut out = event_envelope_problems(event, reg);
    match reg.events.get(t) {
        Some(e) => {
            out.extend(e.params.validate(&event["subject"], &reg.defs));
            out.extend(e.result.validate(&event["data"], &reg.defs));
        }
        None => out.push(format!("{t}: event type not in the registry")),
    }
    out
}

fn event_envelope_problems(event: &Value, reg: &Registry) -> Vec<String> {
    match reg.defs.get("Event") {
        Some(s) => s.validate(event, &reg.defs),
        None => vec!["no Event definition".into()],
    }
}

// ---------------------------------------------------------------------------------------------
// The registry. Edit here; the comments above each group say where the truth lives.
// ---------------------------------------------------------------------------------------------

/// Shared types. Model structs mirror `vk-proto::model`.
pub const DEFS: &str = r##"
# Shared definitions. Model structs live in vk-proto (model.rs); enum-valued fields are `string`
# here unless the wire form is a stable snake_case set. Option fields are always serialized
# (null when unset), hence `T|null` rather than `?`. Everywhere: `f?: T` may be absent but is
# never null; a field that can be null says `T|null` (`f?: T|null`: absent or null).
Target = string
ReviewPurge = {tasks: int, runs: int, messages: int, intent_excerpts: int, turns: int, tool_items: int, reviewer_prompts: int, notes: int, human_notes: int, check_logs: int, projections: int}
Cursor = {machine_uuid: string, session_uuid: string, log_epoch: string, seq: int}
ConfigDiagnostic = {line: int, col: int, message: string}
ConfigWarning = {key: string, line: int|null, col: int|null, message: string}
SessionEntry = {name: string, running: bool, socket: string, state: string, current: bool, pid?: int}
Event = {seq: int, ts: int, v: int, tier: sync|history, type: string, subject: object, actor: object, data: any}
IsolationLevel = host|sandbox|container|vm
Isolation = {level: IsolationLevel, provider: string, network: string, yolo: bool, scope: string, visible_roots: [string]}
Workspace = {id: string, handle: string, name: string|null, auto_name: string, root_path: string, task: string|null, order: number, branch: string|null}
LayoutNode = {Leaf: {pane: string}} | {Split: {dir: 'Horizontal'|'Vertical', children: [any]}}
FloatingPane = {pane: string, x: number, y: number, w: number, h: number, z: int}
Tab = {id: string, handle: string, workspace: string, title: string|null, number: int, layout: LayoutNode, focused_pane: string|null, zoomed_pane: string|null, order: number, floating?: [FloatingPane], floats_hidden?: bool}
Group = {id: string, handle: string, name: string, parent: string|null, collapsed: bool, order: number, workspaces: [string]}
BrowserPane = {url: string, machine: string, task: string|null, preview: string|null, source_pane: string|null, history: [string], history_index: int, title: string, watch?: string|null, device?: string|null, viewport?: string|null}
Pane = {id: string, handle: string, tab: string, workspace: string, title: string|null, auto_title: string, cwd: string|null, cols: int, rows: int, child_pid: int|null, fg_cmdline: [string], exited: bool, exit_code: int|null, unread: bool, marked_unread: bool, pinned: bool, created_by: string, recovered: string|null, isolation?: Isolation, browser?: BrowserPane|null}
AgentState = starting|working|idle|error|rate_limited|exited|unknown
Facet = {value: string, since_ms: int, source: string, confidence: number, detail: string|null}
RunUsage = {input_tokens: int, output_tokens: int, cache_read_tokens: int, cache_write_tokens: int, cost_usd: number|null, model: string|null, source: string, updated_at_ms: int}
RateLimitInfo = {limited: bool, resets_at_ms: int|null, scope: string|null, used_percent: number|null, message: string|null, observed_at_ms: int}
AgentRun = {id: string, handle: string, name: string|null, pane: string, harness: string, harness_version: string|null, integration: string, harness_session_id: string|null, transcript_path: string|null, resume_argv: [string], cwd: string|null, model: string|null, task: string|null, execution: Facet, health: string, yolo: bool, permission_mode: string|null, last_message: string|null, last_tool: string|null, turns_completed: int, done_rev: int, started_at_ms: int, ended_at_ms: int|null, capabilities: [string], usage?: RunUsage, rate_limit?: RateLimitInfo|null}
Answer = {decision: allow|allow_always|deny|string|null, choices?: [any], text?: string|null}
Interaction = {id: string, handle: string, run: string, pane: string, kind: string, status: string, title: string, body_md: string|null, action: object|null, questions: [object], plan_md: string|null, answer_channel: string, native_ref: string|null, source: string, confidence: number, answerable: bool, gate: bool, decision_rev: int, delivery: string, delivery_error: string|null, answer: Answer|null, answered_by: string|null, answer_key?: string|null, opened_at_ms: int, answered_at_ms: int|null}
PolicyRule = object
Notification = {id: string, kind: string, pane: string|null, title: string, body: string, urgency: string, created_at_ms: int, read: bool, channels?: [string]}
MaterializedFile = {path: string, outcome: copied|linked|cloned|missing|exists|rejected|failed, method?: string|null, hash?: string|null, error?: string|null}
PrLookup = {kind: pr|no_pr|unavailable, pr?: {number: int, state: string, is_draft: bool, review_decision: string|null, checks: none|pending|passing|failing, url: string, label: string}, reason?: string}
Task = {id: string, handle: string, title: string, slug: string, workspace: string|null, repo_root: string, worktree_path: string|null, branch: string|null, base_ref: string|null, port_range: [int]|null, status: string, setup_status: string|null, created_at_ms: int, ownership?: owned|attached, owner_machine?: string, intent_revision?: int|null, priority?: int|null, rev?: int, review_label?: string|null, effort?: string|null, isolation?: Isolation, checkout?: string|null, rate_limit?: RateLimitInfo|null}
Preview = {id: string, handle: string, machine: string, pane: string|null, task: string|null, port: int, path: string, label: string|null, url: string, scheme: string, status: suggested|declared|up|down|gone, source: declared|listener|output_url|banner, pid: int|null, first_seen_ms: int, last_seen_ms: int, pane_handle?: string|null, task_handle?: string|null}
PreviewCa = {path: string, sha256: string, spki_sha256: string, trust: string}
PreviewMirror = {machine: string, preview: string, preview_handle: string, local_port: int, addrs: [string], since_ms: int, accepted: int, rejected: int, authenticated: bool, peer_check: same_user}
BrowserSession = {session: string, session_id: string, owner: {pane: string, pane_handle: string|null, run: string|null} | {user: string}, preview: string|null, url: string, created_ms: int, viewport: {width: int, height: int}, device: string|null, previews: own|machine, human_control: bool, screencast: bool, proxy_port: int|null, machine: string, environment: {kind: string, machine: string, runner: string, browser: any, fresh_context: bool, device: string|null, viewport: {width: int, height: int}, dpr: number}, status?: int|null, final_url?: string, title?: string}
Appearance = {known: bool, dark: bool, mode: string, theme: string, source: string}
PaneLive = {pane: string, progress: {state: normal|error|indeterminate|paused, pct: int|null}|null, last_exit: {code: int, at_ms: int}|null, user_vars: [[string]]}
LayoutSpec = object
ScreenshotMeta = {id: string, handle?: string, workspace?: string, task?: string|null, run?: string|null, preview?: string|null, mime?: string, width?: int, height?: int, ts?: int, blob?: string, environment?: object, code?: object}
AgentSummary = {working: int, needs_input: int, done: int, idle: int}
RpcErrorData = {kind: string, details?: any, retryable: bool}
RpcError = {code: int, message: string, data: RpcErrorData}
PolicyMatch = {tool?: string, command_regex?: string, path_glob?: string, url_glob?: string}
PolicyRuleInfo = {id: string, source: config|user|repo, repo?: string, match: PolicyMatch, effect: allow|deny|ask, scope?: string, note?: string, ignored?: string, created_at_ms?: int, created_by?: string}
PolicyRepo = {repo: string, file: string, exists: bool, trusted: bool, allow_policy_grants: bool, rules: int, errors: [string]}
PolicyScope = string | {cwd?: string, repo?: string, pane?: Target, run?: Target}
ElevationRequest = {request: string, pane: string, reason: string, created_at_ms: int, status: pending|approved|denied}
AuditEntry = {seq: int, ts: int, type: string, actor: object, subject: any, data: any, prev_hash: string, hash: string}
SyncGroup = {id: string, panes: [string], members: [{pane: string, handle: string|null, agent: bool}], tab: string|null, agents: [string], created_at_ms: int}
ConfigLayer = {source: default|user|repo|runtime|cli, path?: string|null, exists?: bool, trusted?: bool, applied?: bool, keys?: [string]}
TaskPorts = {task: string, handle: string, lease: {start: int, end: int, count: int}|null, env: object}
"##;

/// `method :: params => result`. Methods whose spec 07 §2 row exists follow it; the rest follow
/// the handler. Optional (`?`) fields may be absent; only `|null` types may be null.
pub const METHOD_SHAPES: &[&str] = &[
    CORE_SHAPES,
    MORE_SHAPES,
    INTERNAL_SHAPES,
    BATCH_2A_SHAPES,
    SECURITY_SHAPES,
    V1_REMAINDER_SHAPES,
    crate::orch_shapes::SHAPES,
    crate::handoff_out::SHAPES,
    ADAPTER_POLISH_SHAPES,
    PRIVACY_SHAPES,
    BATCH_3D_SHAPES,
    BATCH_3B_SHAPES,
    BATCH_3F_SHAPES,
    BATCH_3A_SHAPES,
];

/// Lane 3E (09 §9.1–9.3): `state.forget` and state encryption.
pub const PRIVACY_SHAPES: &str = r##"
# vibeke forget: scrollback.forget's scope, plan and counts, then everything else stored for the scope (full scope only)
state.forget :: {pane?: Target, workspace?: Target, before?: string|int, all?: bool, dry_run?: bool = false, plan?: string, scrollback_only?: bool = false}
  => {scope: any, pane_ids: [string]|null, plan: any, dry_run: bool, panes?: int, segments_deleted?: int, bytes_deleted?: int, fts_rows_deleted?: int, archive_panes_dropped?: int, assistant_purged?: int, scrollback_only?: bool, review?: ReviewPurge, also?: {items: int, collisions?: int, blobs: {screenshots: int, files: int}, uploads: {removed: int, kept_shared: int}, drafts: {drafts: int, notes: int, failed: int}, assistant: {purged: int|null, by: string}, desk: {calls: int, rows: int|null, errors?: [string]}, snapshots: int, events_tombstoned: int}, not_covered?: [string]}
# security.encrypt_state (09 §9.1): what is sealed, with which key, and how many files are in each mode (full scope only)
security.encryption.status :: {}
  => {encrypt_state: bool, active: bool, key_id: string|null, keychain: string|null, readable: bool, error: string|null, files: {sealed: int, plain: int}, covers: [string], not_covered: [string], redact_scrollback_index: bool, note: string}
# rewrite existing scrollback segments and session blobs sealed or plain (full scope only)
security.encryption.migrate :: {to?: sealed|plain|encrypted|decrypted = sealed, dry_run?: bool = false}
  => {to: sealed|plain, dry_run: bool, files: int, changed: int, unchanged: int, failed: [string]}
"##;

const CORE_SHAPES: &str = r##"
# --- client.*, api.*, server.*, status, theme ---
client.hello :: {client: string, version: string, api: string, kind: tui|cli|plugin|gateway|agent|string, token?: string, host?: {bundle_id?: string, term_program?: string}}
  => {server_version: string, api: string, session: string, machine: string, capabilities: [string], features: [string], client_id?: string}
client.list :: {} => {clients: [{id: string, kind: string, client?: string, version?: string, attached_at: int, peer?: {uid: int, pid?: int, machine?: string}, focused_pane: string|null}]}
client.appearance :: {dark: bool, source?: osc11|csi996|string} => {appearance: Appearance, colorfgbg: string}
# full scope only
client.focus :: {pane?: Target, url?: string, raise?: bool = true} => {pane: string, client: string, focused: bool, raised: bool, host: any}
theme.get :: {} => {appearance: Appearance, colorfgbg: string, reports?: any}
theme.set_mode :: {mode: auto|light|dark|null} => {appearance: Appearance, colorfgbg: string, reports?: any}
status.segments :: {pane?: Target, client?: string}
  => {segments: object, focus: any, appearance: Appearance, client_side: [string]}
api.methods :: {} => {methods: [{name: string, mutating: bool, milestone?: string, capability?: string}]}
# the JSON Schema bundle of this binary (07 §1.5); with `method`, only that method's params/result
api.schema :: {method?: string} => {schema: object}
# `boot_id` changes when the server image changes (start or `server.restart`); `restart_error` says why the last restart could not exec (the old image keeps serving)
server.status :: {}
  => {pid: int, version: string, uptime_ms: int, boot_id?: string, restart_error?: string|null, session: string, machine: string, panes: int, holders: {live: int, orphaned?: int}, clients: int, event_seq: int, socket?: string, degraded?: any, ephemeral?: int, preview?: object, timers?: object, db_size?: int, rss?: int}
# re-read config.toml (runtime overrides on top); errors leave the applied config in force; full scope only
server.reload_config :: {} => {changed: [string], errors: [ConfigDiagnostic], warnings?: [ConfigWarning]}
# with kill_panes false the holders keep running and the next server reattaches; full scope only
server.stop :: {kill_panes?: bool = false} => {}
session.snapshot :: {include?: [workspaces|tabs|panes|runs|interactions|tasks|previews|layouts|machines|groups|string]}
  => {at_seq: int, workspaces?: [Workspace], groups?: [Group], tabs?: [Tab], panes?: [Pane], runs?: [AgentRun], interactions?: [Interaction], tasks?: [Task], previews?: [Preview], layouts?: [any], focused?: {*: {workspace: string|null, tab: string|null, pane: string|null}}}

# --- events ---
events.subscribe :: {after?: Cursor|int, types?: [string] | string, subjects?: {workspace?: Target, tab?: Target, pane?: Target, run?: Target, task?: Target}, include_snapshot?: bool = false, machine?: string}
  => {subscription_id: string, at: Cursor}
# stops a subscription of this connection; idempotent (unknown or finished ids answer unsubscribed: false)
events.unsubscribe :: {subscription_id: string} => {unsubscribed: bool}
events.read :: {after?: Cursor|int, before?: Cursor|int, types?: [string] | string, subjects?: object, limit?: int = 500} => {events: [Event], next: Cursor}
events.wait :: {types?: [string] | string, subjects?: object, after?: Cursor|int, timeout_ms?: int = 60000} => {event: Event}

# --- groups, workspaces ---
group.list :: {} => {groups: [Group], ungrouped: [string]}
group.create :: {name: string, parent?: Target} => {group: Group}
group.rename :: {group: Target, name: string} => {group: Group}
group.collapse :: {group: Target, collapsed?: bool} => {group: Group}
group.move :: {group: Target, parent?: Target|null, index?: int, delta?: int} => {group: Group}
group.delete :: {group: Target} => {}
group.add :: {group: Target, workspace?: Target, index?: int} => {workspace: Workspace, group: string|null}
group.remove :: {workspace?: Target, index?: int} => {workspace: Workspace, group: string|null}
workspace.list :: {group?: Target} => {workspaces: [Workspace]}
workspace.get :: {workspace?: Target} => {workspace: Workspace}
workspace.create :: {cwd?: string, name?: string, group?: Target, focus?: bool = false, layout?: LayoutSpec, command?: [string]} => {workspace: Workspace, tab: Tab, root_pane: Pane}
workspace.rename :: {workspace?: Target, name: string|null} => {workspace: Workspace}
workspace.focus :: {workspace: Target, client?: string} => {workspace: Workspace}
workspace.move :: {workspace: Target, group?: Target|null, index?: int, delta?: int} => {workspaces: [Workspace]}
# refuses with conflict:pane_busy when a non-shell foreground process runs, unless force
workspace.close :: {workspace?: Target, force?: bool = false} => {}

# --- tabs ---
tab.list :: {workspace?: Target} => {tabs: [Tab]}
tab.create :: {workspace?: Target, cwd?: string, title?: string, focus?: bool = false, command?: [string], layout?: LayoutSpec} => {tab: Tab, root_pane: Pane}
tab.rename :: {tab: Target, title: string|null} => {tab: Tab}
tab.focus :: {tab: Target, client?: string} => {tab: Tab}
tab.move :: {tab: Target, index: int, workspace?: Target} => {tabs: [Tab]}
tab.close :: {tab: Target, force?: bool} => {}
tab.floats :: {tab?: Target, visible?: bool} => {tab: Tab}

# --- panes ---
pane.list :: {workspace?: Target, tab?: Target, has_agent?: bool} => {panes: [Pane]}
pane.get :: {pane?: Target} => {pane: Pane, run?: AgentRun|null, open_interactions?: [Interaction], live?: PaneLive|null}
pane.current :: {} => {pane: Pane}
pane.split :: {pane?: Target, direction?: right|down|left|up = right, ratio?: number = 0.5, cwd?: string, command?: [string], env?: {*: string}, focus?: bool = false, title?: string} => {pane: Pane}
pane.close :: {pane?: Target, force?: bool} => {}
pane.focus :: {pane: Target, client?: string} => {pane: Pane}
pane.rename :: {pane?: Target, title: string|null} => {pane: Pane}
pane.pin :: {pane?: Target, pinned: bool} => {pane: Pane}
pane.mark_seen :: {pane?: Target} => {pane: Pane}
pane.mark_unread :: {pane?: Target} => {pane: Pane}
pane.zoom :: {pane?: Target, zoomed?: bool} => {tab: Tab}
pane.resize :: {pane?: Target, direction: right|down|left|up, cells?: int, percent?: number} => {layout: any}
pane.float :: {pane?: Target, tab?: Target, rect?: {x?: number, y?: number, w?: number, h?: number}, cwd?: string, command?: [string], focus?: bool} => {pane: Pane}
pane.embed :: {pane: Target, target?: Target, direction?: right|down|left|up = right, ratio?: number} => {pane: Pane, tab: Tab}
pane.send_text :: {pane?: Target, text: string, paste?: auto|bracketed|raw = auto} => {bytes: int}
pane.send_keys :: {pane?: Target, keys: [string]} => {}
pane.run :: {pane?: Target, command: string, wait?: bool, timeout_ms?: int} => {exit_code?: int|null, output_tail?: string}
pane.read :: {pane?: Target, source?: visible|recent|recent_unwrapped|scrollback|detection|last-command = visible, lines?: int = 200, from_line?: int, format?: text|ansi|cells = text, include_cursor?: bool}
  => {text?: string, cells?: any, rows?: int, revision?: int, truncated?: bool, scroll?: any, cursor?: any, running?: bool, exit_code?: int|null, prompt_line?: int}
pane.wait_idle :: {pane?: Target, quiet_ms?: int = 2000, timeout_ms?: int} => {revision: int}
pane.wait_output :: {pane?: Target, match?: string, regex?: string, source?: string, timeout_ms?: int, since_revision?: int} => {matched: string, line: any, revision: int}

# --- agents ---
agent.list :: {workspace?: Target, state?: [AgentState], harness?: string} => {runs: [AgentRun]}
agent.get :: {target: Target} => {run: AgentRun, pane: Pane, open_interactions: [Interaction], last_turn?: any}
agent.harnesses :: {} => {harnesses: [{id: string, display?: string, version_detected?: string|null, integration_installed?: bool, capabilities?: [string]}]}
agent.start :: {pane?: Target, harness: string, name?: string, mode?: tui|headless = tui, args?: [string], env?: {*: string}, model?: string, task?: Target, ready_timeout_ms?: int = 30000} => {run: AgentRun}
agent.spawn :: {harness: string, name?: string, where?: object, prompt?: string, args?: [string], focus?: bool = false} => {pane: Pane, run: AgentRun}
agent.prompt :: {target: Target, text: string, images?: [string], mode?: send|steer|follow_up = send, wait?: bool, until?: [string], timeout_ms?: int} => {run: AgentRun, turn?: any}
agent.read :: {target: Target, source?: visible|recent|transcript = visible, lines?: int, format?: text|ansi|cells} => {text?: string, turns?: [any], rows?: int, revision?: int, truncated?: bool}
agent.wait :: {target: Target, until?: [string], timeout_ms?: int} => {run: AgentRun, state: string, interaction?: Interaction}
agent.interrupt :: {target: Target} => {run: AgentRun}
agent.send_keys :: {target: Target, keys: [string]} => {}
agent.rename :: {target: Target, name: string|null} => {run: AgentRun}
agent.release :: {target: Target} => {}
agent.resume :: {pane?: Target, run: Target, mode?: string} => {run: AgentRun}

# --- interactions ---
interaction.list :: {status?: open|string, run?: Target, workspace?: Target, kind?: string} => {interactions: [Interaction]}
interaction.get :: {interaction: Target} => {interaction: Interaction}
interaction.answer :: {interaction: Target, decision?: allow|allow_always|deny, choices?: {*: [string]}, text?: string, scope?: once|session|rule, rule?: PolicyRule, idempotency_key?: string, actor?: string, expected_decision_rev?: int}
  => {interaction: Interaction, delivery: {state: string, channel?: native|keystrokes|none|string}, duplicate?: bool}
interaction.cancel :: {interaction: Target} => {interaction: Interaction}

# --- tasks, worktrees ---
task.list :: {status?: string, repo?: string} => {tasks: [Task]}
task.get :: {task: Target} => {task: Task, branch_status: {branch: string|null, ahead: int, behind: int, dirty_files: int, upstream: string|null, compared_to: string|null}|null, pr?: PrLookup|null, collisions?: [Collision]}
task.create :: {title: string, repo: string, base?: string, isolation?: worktree|none|auto, slug?: string, branch?: string, agents?: [{harness: string, name?: string, prompt?: string}], setup?: bool = true, ports?: int, group?: Target, root?: string, branch_template?: string, fetch?: bool, dry_run?: bool = false}
  => {task: Task, workspace: Workspace, panes: [Pane], runs: [AgentRun], copied?: [string], files?: [MaterializedFile], deps?: any, setup?: {pane: string|null, status: string|null, agents_pending: bool, commands: [{source: string, command: string}]}, warnings?: [string]}
  | {dry_run: true, title: string, repo_root: string, slug: string, plan: object, agents: any, setup: bool}
task.setup :: {task: Target, setup_script?: string} => {task: string, started: bool, pane: string|null, setup_status: string|null, commands: [{source: string, command: string}], needs_trust: bool}
task.pr :: {task: Target, refresh?: bool = false} => {task: string, pr: PrLookup}
task.reconcile :: {repo?: string} => {reports: [{repo: string, missing: [{task_id: string, path: string, reason: string}], branch_moved: [{task_id: string, path: string, expected: string|null, actual: string|null}], orphans: [{path: string, kind: string, branch: string|null}]}]}
task.finish :: {task: Target, remove_worktree?: ask|bool = false, force?: bool = false, archive?: bool, status?: string} => {task: Task, job?: any}
worktree.list :: {repo?: string, cwd?: string} => {worktrees: [{path: string, branch: string|null, head: string, task?: string|null, workspace?: string|null, locked: bool, prunable: bool}]}
worktree.create :: {repo?: string, cwd?: string, branch: string, path?: string, base?: string, open?: bool, focus?: bool = false} => {worktree: any, workspace?: Workspace}
worktree.open :: {path: string, focus?: bool = false} => {workspace: Workspace}
# with dry_run nothing is removed: the result says whether it would proceed
worktree.remove :: {path: string, force?: bool = false, dry_run?: bool = false}
  => {job: any} | {dry_run: true, path: string, branch: string|null, dirty_files: int, unpushed_commits: int, would_remove: bool, force: bool}
worktree.repo_root :: {cwd: string} => {repo_root: string, vcs: string}

# --- layouts, notes, notifications, blobs, search ---
layout.list :: {name?: string} => {layouts: [{name: string, description?: string|null, cwd?: string|null, tabs: int, panes: int, valid: bool}]} | {layout: LayoutSpec}
layout.get :: {name?: string} => {layouts: [{name: string, description?: string|null, cwd?: string|null, tabs: int, panes: int, valid: bool}]} | {layout: LayoutSpec}
layout.apply :: {layout: LayoutSpec | string, workspace?: Target, new_workspace?: {cwd: string, name?: string}} => {workspace: Workspace, tabs: [Tab], panes: [Pane]}
layout.export :: {tab?: Target, workspace?: Target} => {layout: LayoutSpec}
notes.get :: {workspace?: Target} => {notes: {workspace: string, text: string, rev: int}}
notes.set :: {workspace?: Target, text: string, expected_rev?: int} => {notes: {workspace: string, text: string, rev: int}}
notification.list :: {unread_only?: bool = true, limit?: int} => {notifications: [Notification]}
notification.read :: {notification?: Target, all?: bool} => {}
notification.send :: {title: string, body?: string, urgency?: string = normal, subject?: string, sound?: bool} => {notification: Notification}
notification.config :: {} => {channels: [string], rules: any, native?: object, hosts?: any}
blob.put :: {mime: string, data_b64?: string, path?: string} => {hash: string, size: int}
image.upload :: {pane: Target, mime: string, data_b64?: string, path_on_client?: string} => {path_on_machine: string, blob?: string}
search.query :: {q: string, scope?: {workspace?: Target, pane?: Target, run?: Target}, sources?: [scrollback|transcript|events], limit?: int = 50, regex?: bool = false} => {hits: [{pane?: string, run?: string|null, source: string, line?: any, text: string, ts?: int, context?: any}], redacted?: bool}
scrollback.forget :: {pane?: Target, workspace?: Target, before?: string|int, all?: bool, dry_run?: bool = false, plan?: string}
  => {scope: any, pane_ids: [string], plan: any, dry_run: bool, panes?: int, segments_deleted?: int, bytes_deleted?: int, fts_rows_deleted?: int, archive_panes_dropped?: int, assistant_purged?: int, review?: ReviewPurge}

# --- fs, git ---
fs.list :: {pane?: Target, path?: string = ''} => {path: string, entries: [{name: string, kind: file|dir|symlink|other, size?: int, ignored: bool, secret: bool}], truncated: bool}
fs.read :: {pane?: Target, path: string} => {path: string, text?: string, binary: bool, truncated: bool, size: int, secret: bool}
git.status :: {pane?: Target, path?: string} => {repo_root: string, branch?: string|null, upstream?: string|null, ahead: int, behind: int, clean: bool, truncated: bool, files: [{path: string, orig_path?: string|null, x: string, y: string, kind: string, staged: bool, adds?: int|null, dels?: int|null, binary: bool, secret: bool}]}
git.diff :: {pane?: Target, path?: string, file: string, staged?: bool} => {file: string, diff: string, truncated: bool, binary: bool, untracked: bool, secret?: bool}
git.log :: {pane?: Target, path?: string, base?: string, limit?: int = 50} => {commits: [{sha: string, short: string, author: string, ts: int, subject: string}], truncated: bool}
fs.browse :: {path?: string = '~', prefix?: string} => {path: string, parent: string|null, git_repo: bool, entries: [{name: string, git_repo: bool}], truncated: bool}
repo.candidates :: {origin: string} => {repos: [{path: string, remote: string}]}

# --- previews ---
preview.list :: {machine?: string, task?: Target, pane?: Target, status?: suggested|declared|up|down|gone|all, all?: bool} => {previews: [Preview], machine?: string}
preview.get :: {preview: Target, machine?: string} => {preview: Preview}
preview.declare :: {port: int|string, path?: string = '/', scheme?: http|https = http, label?: string, pane?: Target, task?: Target, tls_origin?: bool} => {preview: Preview}
preview.promote :: {preview: Target, machine?: string} => {preview: Preview}
preview.dismiss :: {preview: Target, machine?: string} => {}
preview.forget :: {preview: Target, machine?: string} => {}
preview.open :: {preview?: Target, url?: string, machine?: string, mode?: pane|window|proxy, window?: bool, proxy?: bool, split?: right|down|left|up|tab|float, pane?: Target, focus?: bool = true, device?: string, viewport?: string|{width: int, height: int}, profile?: string, no_open?: bool = false, open?: bool, tls_origin?: bool}
  => {opened_in: pane, pane: string, pane_handle: string, tab: string, url: string, machine: string, source_pane: string|null}
   | {opened_in: window, url: string, machine: string, profile: string, profile_dir: string, browser: string, browser_kind: string, pid: int, reused: bool, socks_port: int|null, route: none|loopback|remote}
   | {opened_in: default_browser, url: string}
   | {opened_in: proxy, url: string, proxy_url: string, host: string, proxy_port: int, machine: string, preview: string, opened: bool, token_ttl_s: int, session_ttl_s: int, tls_origin: bool, remote_url: string, caveats: string, ca?: PreviewCa, open_url?: string}
preview.url :: {preview: Target, machine?: string} => {remote_url: string, profile_url: string, proxy_url: string|null}
preview.status :: {} => {socks_port: int|null, browsers: [{profile: string, machine: string, route: string, pid: int, running: bool}], links: [{machine: string, connected: bool, bytes_in: int|null, bytes_out: int|null, rtt_ms: int|null}], accepted: int, rejected: int, proxy: {port: int, tls: bool, routes: [{host: string, machine: string, preview: string, handle: string, port: int, scheme: http|https, tls: bool}], stats: {requests: int, denied: int, websockets: int}}|null, mirrors: [PreviewMirror]}
# full scope only
preview.mirror :: {preview: Target, machine?: string} => {machine: string, preview: string, preview_handle: string, local_port: int, addrs: [string], since_ms: int, accepted: int, rejected: int, authenticated: bool, peer_check: same_user, warning: string, url?: string, already?: bool}
# full scope only
preview.unmirror :: {preview?: Target|int, port?: int, machine?: string} => {local_port: int, machine: string, preview: string}

# --- screenshots ---
screenshot.list :: {task?: Target, preview?: Target, run?: Target, since?: string|int, since_ms?: int, limit?: int = 50} => {screenshots: [ScreenshotMeta], count: int, total: int}
screenshot.get :: {id: string} => ScreenshotMeta
screenshot.open :: {id: string} => ScreenshotMeta
screenshot.delete :: {id: string, force?: bool} => {id: string, handle: string, deleted: bool, blob_removed: bool}
"##;

const MORE_SHAPES: &str = r##"
# --- assistant (14) ---
assistant.status :: {} => {enabled: bool, configured: bool, config_problem?: string|null, coordinator?: {machine: string, session: string}, profile?: object|null, limits?: object, today?: {utc_day: int, used: any, reserved: any, remaining: any}, background?: bool, background_detail?: object, scheduler?: {capacity: int, running: int, waiting: int}, capabilities?: object|null, cache?: {enabled: bool, entries: int}, remote_sources?: bool, keychain_backend?: string}
assistant.providers :: {} => {connections: [{id: string, adapter: string, endpoint: string, endpoint_error?: string|null, credential: string, verified: bool}], profiles: [object], default_profile?: string|null, coordinator_note?: string, targets?: [{name: string, adapter: string, endpoint: string}]}
assistant.consent :: {workspace?: Target, remote_workspace?: string, connection?: string, profile?: string, classes?: [selected_text|structured_state|review_package|screen], operations?: [string], auto_send?: [string]} => {consent: object, notice: string}
assistant.revoke :: {workspace?: Target, remote_workspace?: string, connection?: string} => {revoked: int, cancelled_requests: int}
assistant.generate :: {operation: suggest_task_details|review_summary|pane_title|briefing|handoff|effort_estimate|navigate|decision_card|stall_notice|background_summary|task_title, profile?: string, priority?: interactive|background, stream?: bool, idempotency_key?: string, retry_of?: string, inputs?: object, run?: Target, turns?: [int], pane?: Target, task?: Target, workspace?: Target, interaction?: Target, query?: string, remote_sources?: [object], include_screen?: bool}
  => {request: object, preview?: {digest: string, system?: string, user?: string, model?: string, adapter?: string, endpoint_host?: string, execution_machine?: string, max_output_tokens?: int, bytes?: int, estimated_input_tokens?: int, estimated_max_cost_usd?: number|null, sources?: [object], omitted?: any, redactions?: any, notice?: string, priority?: string, coverage_notes?: [string]}, requires_confirmation?: bool, confirm_with?: object|null, deduplicated?: bool, coalesced?: bool, cached?: bool, note?: string}
assistant.confirm :: {request: string, preview_digest: string} => {request: object}
assistant.cancel :: {request: string} => {request: object, detached?: bool, note?: string}
assistant.models :: {connection?: string, profile?: string, refresh?: bool} => {connection: string, adapter: string, provenance: live|cached|bundled, refreshed_at_ms: int|null, models: [{id: string, display_name?: string|null, created?: string|null, capabilities: object}], note: string, endpoint_host?: string, execution_machine?: string, current_model?: string|null, current_capabilities?: object|null, explicit_model?: string}
assistant.test :: {profile?: string, probe?: [streaming|json_schema] | string} => {ok: bool, id: string, profile: string, connection: string, adapter: string, model: string, endpoint_host: string, execution_machine: string, latency_ms: int, attempts: int, usage: object, estimated_cost_usd?: number|null, counted: bool, error?: object|null, probes: [object]}
assistant.background :: {action?: status|tick} => any
assistant.get :: {request: string} => {request: object}
assistant.list :: {workspace?: Target, state?: string, limit?: int} => {requests: [object]}
assistant.purge :: {request?: string, workspace?: Target, all?: bool} => {purged: int}

# --- desk (R2) and drafts (R3) ---
desk.status :: {} => {db: string, selection: object, exclude: any, retention_days: int, counts: object, sources: [object], last_pass: any}
desk.index :: {budget?: int} => {pass: {registered: int, bytes: int, rows: int, purged: int, reset: int, pending: int, errors: any}}
desk.sessions :: {repo?: string, harness?: string, limit?: int = 50} => {sessions: [{session: string, harness: string, machine?: string, repo?: string|null, cwd?: string|null, workspace?: string|null, run?: string|null, first_ts?: int, last_ts?: int, turns?: int, rows?: int, paths?: any, status?: string, live?: any, resume?: any}]}
desk.search :: {text: string, repo?: string, harness?: string, since?: string|int, until?: string|int, session?: string, limit?: int = 20, sort?: relevance|recent, fresh?: bool = true} => {hits: [object], index: object}
desk.context :: {session: string, turns?: [int] | string, path?: string, objective?: string} => {package: object, text: string}
desk.open :: {session: string, turn?: int, path?: string, focus?: bool = false} => {status: string, action: string, turn_items?: any, focused?: bool, actions?: [object]}
desk.resume :: {session: string, mode?: native, pane?: Target, workspace?: Target} => object
desk.forget :: {session?: string, repo?: string, workspace?: Target, before?: string|int} => {rows_deleted: int, sessions_forgotten: int}
draft.create :: {scope?: workspace|task, id?: string, text: string, title?: string, attachments?: [{kind: file|screenshot, path?: string, blob?: string, data_b64?: string, name?: string, label?: string}], idempotency_key?: string} => {draft: object}
draft.get :: {draft: Target} => {draft: object}
draft.list :: {scope?: workspace|task, id?: string, all?: bool} => {drafts: [object]}
draft.update :: {draft: Target, text?: string, title?: string, attachments?: [any], add_attachment?: object, remove_attachment?: int, expected_rev?: int} => {draft: object}
draft.delete :: {draft: Target} => {deleted: any}
draft.reorder :: {order: [string]} => {drafts?: [object]}
draft.combine :: {ids: [string], title?: string, separator?: string, delete_sources?: bool = false} => {draft: object}
draft.check :: {target_run: Target, draft?: Target} => {run: string, pane: string, harness: string, native_conversation_id: string|null, send_path: prompt_input|open_pane_only, unsafe?: any, follow_up?: string, steer?: bool, hidden_attachments?: any}
draft.send :: {draft: Target, target_run: Target, idempotency_key: string, include_notes?: bool = false, keep?: bool = false, retry_despite_unknown?: bool} => {draft: object, send: object, send_path: string}
draft.reconcile :: {draft: Target} => {send: object, receipt: object, may_retry: bool, note: string}

# --- browser (06 B) ---
browser.list :: {} => {sessions: [object], browser: object, machine: string}
browser.session_open :: {url?: string, preview?: string, machine?: string, device?: string, viewport?: string | {width?: int, height?: int, w?: int, h?: int}, dpr?: number, color_scheme?: string, dark?: bool, wait?: string, timeout_ms?: int} => BrowserSession
browser.session_close :: {session?: string, browser_session?: string, target?: string} => {session: string, closed: bool}
browser.navigate :: {session?: string, browser_session?: string, target?: string, url?: string, path?: string, wait?: load|domcontentloaded|commit|none|string = load, timeout_ms?: int = 15000} => {session: string, status: int|null, final_url: string, title: string}
browser.click :: {session?: string, browser_session?: string, target?: string, selector?: string, text?: string, x?: number, y?: number, click_count?: int = 1, timeout_ms?: int = 5000} => {session: string, x: number, y: number, element: {x: number, y: number, width: number, height: number, tag: string, text: string}|null}
browser.type :: {session?: string, browser_session?: string, target?: string, text: string, selector?: string, clear?: bool, submit?: bool, timeout_ms?: int} => {session: string, typed: int}
browser.press :: {session?: string, browser_session?: string, target?: string, key: string} => {session: string, key: string}
browser.wait :: {session?: string, browser_session?: string, target?: string, for?: string = load, timeout_ms?: int = 15000} => {session: string, waited: string}
browser.eval :: {session?: string, browser_session?: string, target?: string, expression?: string, js?: string} => {session: string, value: any}
browser.dom :: {session?: string, browser_session?: string, target?: string, format?: a11y|accessibility|text|html = a11y, max_bytes?: int, selector?: string} => {session: string, url: string, format: string, content: string, truncated: bool}
browser.console :: {session?: string, browser_session?: string, target?: string, level?: error|warn|warning|all, since_ms?: int, since?: string, limit?: int = 200} | {pane: Target, kind?: all|console|network, level?: string, errors?: bool, failed?: bool, failed_only?: bool, after?: int, since_ms?: int, since?: string, limit?: int}
  => {session: string, entries: [{ts: int, level: string, text: string, source: string, url?: any, line?: int|null}]} | {pane: string, entries: [object], last_seq: int, source: local|relayed|none, url: string|null}
browser.network :: {session?: string, browser_session?: string, target?: string, failed_only?: bool, failed?: bool, since_ms?: int, since?: string, limit?: int = 200} | {pane: Target, kind?: all|console|network, level?: string, errors?: bool, failed?: bool, failed_only?: bool, after?: int, since_ms?: int, since?: string, limit?: int}
  => {session: string, entries: [{ts: int, method?: string|null, url?: string, type: string|null, status?: int|null, error?: string|null, mime?: string|null, blocked_reason?: string, blocked_by_policy?: string, layer?: string, duration_ms?: int}]} | {pane: string, entries: [object], last_seq: int, source: local|relayed|none, url: string|null}
browser.screenshot :: {session?: string, browser_session?: string, target?: string, url?: string, preview?: string, device?: string, viewport?: string|object, dpr?: number, color_scheme?: string, dark?: bool, full_page?: bool, selector?: string, timeout_ms?: int, inline?: bool}
  => {session: string|null, id: string, handle: string, blob: string, path_on_machine: string, width: int, height: int, bytes: int, binding: bound|illustrative, label: string, meta: ScreenshotMeta, data_b64?: string, mime?: string, inline_skipped?: string, one_shot?: bool, opened_session?: string, status?: int|null, final_url?: string|null, title?: string|null}
browser.diff :: {a: string, b: string, threshold?: number|string = 0.1, force?: bool, inline?: bool}
  => {a: object, b: object, threshold: number, channel_threshold: any, width: int, height: int, size_mismatch: bool, a_size: {width: int, height: int}, b_size: {width: int, height: int}, changed_pixels: int, total_pixels: int, changed_ratio: number, regions: [{x: int, y: int, width: int, height: int, pixels: int}], regions_total: int, forced: bool, blob: string, path_on_machine: string, bytes: int, data_b64?: string, mime?: string, inline_skipped?: string}
browser.install :: {confirm?: bool, version?: string, url?: string, sha256?: string} => {plan: {version: string, platform: string, url: string, sha256: string|null, checksum_known: bool, dir: string, binary: string, installed: bool}, confirm_required: bool} | {installed: bool, binary: string, plan: object}
browser.status :: {}
  => {running: true, pid: int|null, product: string, binary: string, kind: string, uptime_ms: int, sessions: int, denied: int, profile_dir: string, idle_timeout_ms: int}
   | {running: false, sessions: int, denied: int, binary: string|null, kind: string|null, profile_dir: string, idle_timeout_ms: int}
browser.take_over :: {session?: string, browser_session?: string, target?: string} => {session: string, human_control: bool}
browser.release :: {session?: string, browser_session?: string, target?: string} => {session: string, human_control: bool}
browser.attach_screencast :: {session?: string, browser_session?: string, target?: string} => {session: string, delivery: string, width: int, height: int, poll: string}
browser.detach_screencast :: {session?: string, browser_session?: string, target?: string} => {session: string, detached: bool}
browser.screencast_frame :: {session?: string, browser_session?: string, target?: string, after_seq?: int} => {session: string, seq: int, mime: string, width: int|null, height: int|null, received_ms: int, data_b64: string} | {session: string, seq: int|null, data_b64: null}
browser.watch :: {session?: string, agent_pane?: Target, pane?: Target, split?: right|down|left|up|tab|float = right, focus?: bool = true, focus_client?: string} => {opened_in: string, session: string, read_only: bool, pane: string, pane_handle: string, tab: string, url: string, machine: string, source_pane: string|null}
browser.command :: {pane: Target, cmd: back|forward|reload|stop|navigate|screenshot|text|window|pane, url?: string, hard?: bool, text?: string}
  => {pane: string} | {pane: string, profile: string} | {opened_in: window, url: string, machine: string, profile: string, profile_dir: string, browser: string, browser_kind: string, pid: int, reused: bool, socks_port: int|null, route: string} | {opened_in: default_browser, url: string} | object
browser.pane.create :: {preview?: Target, url?: string, split?: right|down|left|up|tab|float = right, pane?: Target, machine?: string, device?: string, viewport?: string|{width: int, height: int}, fit?: bool, focus?: bool = true, focus_client?: string} => {opened_in: pane, pane: string, pane_handle: string, tab: string, url: string, machine: string, source_pane: string|null}
browser.pane.list :: {} => {panes: [{pane: string, handle: string, tab: string, browser: BrowserPane}]}
browser.pane.update :: {pane: Target, url?: string, title?: string, history?: [string], history_index?: int, device?: string, viewport?: string|{width: int, height: int}, fit?: bool} => {pane: string}
browser.pane.console :: {pane?: Target, toggle?: bool = true, focus?: bool = false} => {closed: string, pane_handle: string, browser_pane: string} | {pane: string, pane_handle: string, browser_pane: string, existing?: bool}
browser.pane.console_push :: {pane: Target, entries: [object]} => {pane: string, stored: int}

# --- plugins ---
plugin.list :: {} => {plugins: [{id?: string, plugin_id?: string, version?: string|null, enabled: bool, native?: bool, kind?: actions|process|string, capabilities?: any, status?: any}]}
"##;

/// Methods outside spec 07 §2's tables (tasks' review/check/dependency surface, adapters,
/// sandbox, plugins, compat, blobs, ...), written from their handlers.
const INTERNAL_SHAPES: &str = r##"
# pane-token only (PermissionDenied otherwise); unknown/missing harness returns {} without effect
adapter.delivery_ack :: {interaction: Target, idempotency_key: string, applied?: bool} => {}
# pane-token only; opens an interaction and may block until a decision (observe mode returns at once)
adapter.gate :: {harness: string, event: string, payload?: any, pid?: int} => {decision: object | null, interaction?: Target, idempotency_key?: string, mode?: observe, resumed?: bool}
# pane scope limited to own pane (or panes it created); dropped when seq <= last seen from (pane, source)
adapter.report_self :: {pane?: Target, pane_id?: Target, source?: string = vibeke, seq?: int, agent?: string, harness?: string, state: idle|working|blocked|done|string, message?: string, resume_argv?: [string]}
  => {type: 'ok', dropped?: 'stale_seq', applied?: bool}
# pane-token only; fire-and-forget event signal from a hook/extension
adapter.signal :: {harness: string, event: string, payload?: any, pid?: int} => {hook_output?: object}
agent.manifests :: {} => {manifests: [{id: string, name: string, source: string, family: string, transports: [string], validated_range: any, capabilities_unversioned: any, capabilities_unverified: any, detects: bool, screen_rules: bool, warnings: [string]}], warnings: [string]}
agent.manifests_reload :: {} => {warnings: [string], manifests: [{id: string, name: string, source: string, family: string, transports: [string], validated_range: any, capabilities_unversioned: any, capabilities_unverified: any, detects: bool, screen_rules: bool, warnings: [string]}]}
agent.report :: {pane?: Target, state: string, harness?: string = claude, message?: string} => {}
agent.resumable :: {} => {runs: [AgentRun]}
# pane-scoped callers only see items in their workspace; coverage.excluded counts the hidden ones
attention.list :: {budget_ms?: int, effort?: quick|minutes|deep}
  => {items: [{key: {kind: string, id: string}, class: int | 'finished_turns', title: string, subtitle: string, task: string | null, run: string | null, pane: string | null, interaction: string | null, explanation: string, age_ms: int, risk: string | null, effort: string | null, effort_estimate: {effort: string, source: 'heuristic'} | null, blocks_tasks: int, snoozed_until_ms: int | null, woke_from_snooze: string | null, urgent: bool, deadline_ms?: int | null, deadline_source?: native|gate|null, deadline_in_ms?: int | null, batch?: {id: string, size: int} | null}],
  coverage: {complete: bool, notes: [string], scope: 'all' | {workspace: string}, excluded: int},
  five_minute: {keys: [{kind: string, id: string}], omitted_count: int, note: string, item_notes: [{key: {kind: string, id: string}, note: string}]} | null,
  batches?: [{id: string, members: [{kind: string, id: string}]}],
  also_working?: [{run: string, pane: string, name: string, harness: string, task: {id: string, handle: string, title: string} | null, since_ms: int, working_for_ms: int}]}
# equivalent natively answerable approvals (15 §8.3); answer each member with interaction.answer (expected_decision_rev)
attention.batch :: {interaction: string}
  => {interaction: string, batchable: bool, batch?: string, reason?: string, members: [{interaction: string, handle: string, run: string, pane: string, title: string, decision_rev: int, opened_at_ms: int}], facts?: object | null, note?: string}
# key.kind is one of interaction|review|check_failed|send_unknown|finished_turn|binding_suspended; snooze_until_ms: null clears
attention.update :: {key: {kind: string, id: string}, seen?: bool, item_rev?: int, snooze_until_ms?: int | null, pin?: bool}
  => {key: {kind: string, id: string}, seen: bool, seen_rev: int | null, snoozed_until_ms: int | null, pinned: bool, warning: string | null}
pane.can_see_paths :: {pane?: Target, paths?: [string]} => {visible: [bool]}
pane.equalize :: {tab?: Target} => {layout: LayoutSpec}
# also accepted: pane_id instead of pane; stale-seq drop same as adapter.report_self
pane.report_agent :: {pane?: Target, pane_id?: Target, source?: string = herdr, seq?: int, agent?: string, harness?: string, state: idle|working|blocked|done|string, message?: string, resume_argv?: [string]}
  => {type: 'ok', dropped?: 'stale_seq', applied?: bool}
pane.report_agent_session :: {pane?: Target, pane_id?: Target, source?: string = herdr, seq?: int, agent?: string, harness?: string, agent_session_id?: string, agent_session_path?: string, session_start_source?: string}
  => {type: 'ok', dropped?: 'stale_seq'}
pane.send_bytes :: {pane?: Target, data_b64: string} => {}
task.check.authorize :: {task: Target, check: string, subject: string, subject_id?: string, definition_digest?: string, idempotency_key?: string}
  => {grant: object, definition: object, provenance: any, subject: string, head_sha: string, confirmation_label: string, execution: {machine: any, runner: 'host', checkout: string}}
task.check.cancel :: {check_run: string} => {check_run: object, cancelling: bool}
task.check.get :: {check_run: string} => {check_run: object, definition: object, task: Target, log_tail: string | null}
task.check.list :: {task: Target, subject?: string, subject_id?: string} => {task: Target, subject: object | null, checks: [object], confirmation_label: string}
# authorize: true performs task.check.authorize first (idempotency_key suffixed ':authorize'), full scope only
task.check.run :: {task: Target, check: string, subject: string, subject_id?: string, definition_digest?: string, idempotency_key?: string, authorize?: bool}
  => {check_run: object, definition: object, provenance: any}
task.dependency.add :: {task: Target, depends_on: Target, kind?: blocks|related = blocks, idempotency_key?: string} => {edge: object, note: string}
task.dependency.list :: {task?: Target}
  => {task: Target, dependencies: {depends_on: [{edge: object, title: string | null} | {hidden: true, title: string, edge: {kind: string}}], dependents: [{edge: object, title: string | null} | {hidden: true, title: string, edge: {kind: string}}], blocks_open_tasks: int, note: string}} | {edges: [object]}
# without task: full scope only (pane-scoped callers must name a task)
task.dependency.remove :: {edge: string} | {task: Target, depends_on: Target, kind?: blocks|related, idempotency_key?: string} => {removed: [object], removed_by: object}
task.effort.estimate :: {task: Target} => {task: Target, effort: {set: string | null, heuristic: any, note: string}, model_estimate: {method: string, params: object, note: string}}
task.review.accept :: {task: Target, subject_id: string, intent_revision: int, expected_intent_revision?: int, package_revision?: int, expected_package_revision?: int, exceptions?: [{criterion: string, criterion_id?: string, reason?: string}], idempotency_key?: string}
  => {acceptance: object, label: string, label_text: string, task: Task, note: string}
task.review.candidates :: {task: Target} => {task: Target, current: string | null, candidates: [object], inspect_only: any, no_end_candidate: any, review_base: any, warnings: [string]}
task.review.diff :: {task: Target, subject?: string, path?: string, max_bytes?: int} => {task: Target, subject: string, base_sha: string, head_sha: string, content_sha: string, path: string | null, diff: string, truncated: bool, total_bytes: int, max_bytes: int}
# the review package; flat aliases revision/criteria/observed/blockers added
task.review.get :: {task: Target, subject?: string}
  => {task: Target, task_title: string, package_revision: int, revision: int, intent_revision: int | null, intent: object | null, subject: object | null, subject_current: bool, accept_capable: bool, candidates: [object], inspect_only: any, no_end_candidate: any, review_base: any, baseline: any, warnings: [string], diff_stat: any, sources_verified: bool, historical_only: bool, observed_commands: [object], claims: [object], observed: [object], screenshots: [ScreenshotMeta], checks: [object], check_runs: [object], assessment: object, criteria: [object], blockers: any, label: string, label_text: string, readiness: {label: string, label_text: string}, acceptance: object | null, acceptance_history: [object], live: object, mappings_confirmed: any, review_notes: [object], reviewer_runs: [object], dependencies: object, effort: {set: string | null, heuristic: any, note: string}, snapshot: {available: bool, method: string, note: string, selection?: object}, actions: {accept: {available: bool, reason: string | null, requires_exceptions: any}}, human_reviews?: [object], purged?: any}
task.review.note.classify :: {note: string, classification: blocking|not_blocking|dismissed, reason?: string, idempotency_key?: string} => {note: object}
task.review.notes :: {task: Target} => {task: Target, notes: [object], reviewer_runs: [object]}
task.review.request_reviewer :: {task: Target, harness?: string = claude, subject?: string, expected_subject?: string, prompt?: string, idempotency_key?: string}
  => {request: object, prompt: string, prompt_digest: string, harness: string, subject: string, requires_confirmation: true, label: string, uses_provider: string, confirm_with: {method: string, params: {request: string, prompt_digest: string}}}
# paths (whole files) or patch (a unified diff) capture a selected-patch subject instead of all uncommitted work
task.review.snapshot :: {task: Target, paths?: [string], patch?: string, idempotency_key?: string} => {subject: object, snapshot: object, selection?: object, label: string, note: string}
# full scope only; supported | failed (needs a note) | withdrawn; screenshots must be bound to the reviewed subject
task.review.human_review :: {task: Target, criterion: string, verdict: supported|failed|withdrawn, subject?: string, expected_subject?: string, note?: string, screenshots?: [string], idempotency_key?: string}
  => {review: object, note: string}
# full scope only; exactly one of task, pane, workspace, before, all
task.review.forget :: {task?: Target, pane?: Target, workspace?: Target, before?: string|int, all?: bool, dry_run?: bool = false}
  => {scope: any, dry_run: bool, purged: ReviewPurge}
# why a run's identity is not verified, remedies, and verified runs nearby; never binds or installs
task.link.status :: {run?: string, pane?: Target}
  => {run: {id: string, handle: string, harness: string, integration: string, pane: string, session_reported: bool}, verified: bool, reasons: [string], remedies: [{action: string, label: string, command?: string, note?: string}], candidates: [object], note: string}
# not allowed from a pane scope
task.review.snapshot.gc :: {task: Target, include_unrecorded?: bool = false, dry_run?: bool = false} | {repo: string, include_unrecorded?: bool = false, dry_run?: bool = false}
  => {repo: string, task: Target | null, dry_run: bool, removed: [object], kept: [object], unrecorded: [object], note: string}
# replay of an already-started request returns {request, run, binding, replayed: true}
task.review.start_reviewer :: {request: string, prompt_digest: string, pane?: Target, split_of?: Target, direction?: string = right, checkout?: disposable|task, idempotency_key?: string}
  => {request: object, run: Target, binding: object, note?: string, replayed?: bool}
# user client only (pane tokens get PermissionDenied); container sandbox with private clone required
task.sync :: {task: Target, direction?: pull|push|both = pull, force?: bool = false} => {task: Target | null, synced: [{direction: string, status: string, commits: any, from: any, to: any, ref: any}]}

blob.abort :: {upload_id: string} => {aborted: bool}
blob.append :: {upload_id: string, offset: int, data_b64: string} => {offset: int}
blob.begin :: {name: string, size: int, sha256?: string} => {upload_id: string, max_chunk: int}
# unpack: tar = a dropped directory sent as a tar stream; unpacked under the inbox (regular files and directories only) and path_on_machine is the directory
blob.commit :: {upload_id: string, stage?: browser, unpack?: tar} => {hash: string, sha256: string, size: int, path_on_machine: string, path: string, unpacked?: bool}
# user client only; local_name is reduced to a basename (no local paths in the event log)
paste.translated :: {pane?: Target, files: [{blob: string, bytes?: int, local_name?: string, dir?: bool}], target_namespace?: string} => {recorded: int}
browser.close :: {session?: string, browser_session?: string, target?: string} => {session: string, closed: bool}
# session is required (any of session|browser_session|target); preview is a preview id/handle or URL; viewport is "WxH" or {width|w, height|h}
browser.open :: {url?: string, preview?: string, machine?: string, device?: string, viewport?: string | {width?: int, height?: int, w?: int, h?: int}, dpr?: number, color_scheme?: string, dark?: bool, wait?: string, timeout_ms?: int} => BrowserSession
browser.pane.status :: {} => {browsers: [{profile: string, dpr: number, pid?: int, targets: int, up_ms: int}], targets: [{pane: string, owner: string, url?: string, title?: string, profile: string, machine: string, route: string, running: bool, screencast: bool, viewers: int, frames: int, fps: number, decode_ms: number, css: [number], frame?: [int], history: any, error?: string, pin?: {device?: string, width: int, height: int}, letterbox?: {rect: [number], scale: number}, console: int, network: int, clipboard: {forwarded: int, blocked: int}}], windowed: [string], tiles_sent: int, media_bytes: int}
# session is required (any of session|browser_session|target); aliases: browser.dom
browser.snapshot :: {session?: string, browser_session?: string, target?: string, format?: a11y|accessibility|text|html = a11y, max_bytes?: int, selector?: string} => {session: string, url: string, format: string, content: string, truncated: bool}
# 'as_plugin' needs a single-use ticket from the invocation broker; not allowed from a pane. Errors are returned inside the result, not as RPC errors.
compat.herdr.call :: {method: string, params?: object, as_plugin?: {session: string, ticket: string}} => {result: any} | {error: {code: string, message: string}}
# not available from a pane
compat.invocation.verify :: {ticket: string} => {plugin_id: string, digest: string, grant_id: string, log_id: string, session: string, sandboxed: bool}
compat.status :: {} => {baseline: {herdr: string, commit: string}, support: string, listener: {enabled: bool, path: string, live: bool}, inventory: {implemented: int, partial: int, missing: int}, brokers: int, herdr_root: string, registry: string}
compat.ui.state :: {} => {window_title?: string, popup?: {pane: string, plugin_id: string, entrypoint_id: string}, contributions?: [object], agent_views: [{plugin_id: string, run: string, text: string, detail?: string, tone?: string, updated_ms: int}]}
plugin.action.list :: {plugin?: string} => {actions: [{plugin_id: string, action_id: string, qualified_id: string, title: string, description?: string, contexts: any, available: bool, status: string}], keybindings: [{plugin_id: string, key: string, action: string, description?: string, installed: bool, reason?: string, conflicts_with?: any}]}
# source only honoured as palette|keybinding, otherwise cli; context ids (pane_id/pane, tab_id/tab, workspace_id/workspace) may be given at top level or under context
plugin.action.run :: {plugin: string, action: string, source?: palette|keybinding, context?: {pane_id?: Target, pane?: Target, tab_id?: Target, tab?: Target, workspace_id?: Target, workspace?: Target}, pane_id?: Target, tab_id?: Target, workspace_id?: Target}
  | {action: string, source?: palette|keybinding, context?: object}
  => {log: {log_id: string, plugin_id: string, action_id?: string, event?: string, entrypoint_id?: string, source: string, command: any, long_lived: bool, isolation: sandbox|host, status: string, started_unix_ms: int, finished_unix_ms?: int, started_at: int, finished_at?: int, exit_code?: int, stdout: string, stderr: string, context: object}}
plugin.link.open :: {plugin: string, handler: string, url: string, context?: {pane_id?: Target, tab_id?: Target, workspace_id?: Target}, pane_id?: Target, tab_id?: Target, workspace_id?: Target} => {log: object}
plugin.link_handler.list :: {} => {handlers: [{plugin_id: string, handler_id: string, title?: string, pattern: string, action_id?: string, available: bool, status: string}]}
plugin.log.list :: {plugin?: string, limit?: int} => {logs: [object]}
# not available from a pane
plugin.registry.notify :: {} => {ok: bool}
# not available from a pane; idempotent: closed is null when the pane is already gone
plugin.surface.close :: {pane: Target} => {closed: Target | null}
# alias form: action=reset (needs a user client) behaves as preview.profile.reset; default/list action lists
preview.profile :: {action?: list|reset = list, profile?: string, machine?: string}
  => {profiles: [{name: string, path: string, running: bool, pid: int|null, machine: string|null, route: string|null, bytes: int}], root: string} | {profile: string, removed: bool}
preview.profile.list :: {} => {profiles: [{name: string, path: string, running: bool, pid: int|null, machine: string|null, route: string|null, bytes: int}], root: string}
# not allowed from a pane
preview.profile.reset :: {profile?: string, machine?: string} => {profile: string, removed: bool}
# not allowed from a pane; global = every contained task, persisted (13 §7 "always")
sandbox.allow :: {task?: Target, host: string, global?: bool = false} => {task: string, allowed: string} | {allowed: string, scope: 'global'}
# not allowed from a pane; without task (or with global) removes a global entry
sandbox.disallow :: {host: string, task?: Target, global?: bool = false} => {host: string, scope: 'global'|'task', removed: bool, task?: string|null}
sandbox.list :: {} => {sandboxes: [{sandbox: string, task?: string, checkout?: string, level: string, provider?: string, network: any, yolo: bool, proxy_port?: int, task_allow?: any, credentials: any, container?: {container: string, state: string, image: any, code: any, workdir: any, clone?: {branch: string, base: string}, devcontainer: any, warnings: any, in_box_vibeke: any}, usage?: any, forwards?: [{box_port: int, host_port: int}], idle_suspended?: bool, slot?: string|null}], global_allow?: [string]}
# not allowed from a pane; the argv of an interactive debugging shell in the box (no credentials), run by the CLI
sandbox.shell :: {task: Target} => {sandbox: string, level: container|sandbox, argv: [string], env: [[string]], cwd: any}
# not allowed from a pane; container output (tail lines), setup log and the box's recent sandbox.* events
sandbox.logs :: {task: Target, tail?: int = 200} => {sandbox: string, task: string|null, level: string, container: string|null, setup: string|null, events: [{seq: int, ts: int, type: string, data: any}]}
# not allowed from a pane; removes this session's boxes without a context, orphan box dirs and stale broker dirs
sandbox.prune :: {dry_run?: bool = false} => {dry_run: bool, containers: [{container: string, key: string, state: string, removed: bool}], dirs: [{dir: string, removed: bool}], warnings: [string]}
# not allowed from a pane; a fresh box for a task whose box was lost (runner_lost), then resumes the lost runs
sandbox.recover :: {task: Target} => {task: string, created: bool, synced: any, resumed: [any], skipped: [{run: string, reason: string}]}
# not allowed from a pane; restart a host run from its native session inside an isolation level (13 §3 nudge)
sandbox.relaunch :: {run: Target, isolate?: sandbox|container, network?: none|harness-apis|package-registries|dev|open} => {run: AgentRun, from: string, level: string}
# a contained pane asks for a boundary action; answered through an approval Interaction on that pane
sandbox.request :: {kind: push|copy_out, remote?: string = origin, path?: string, pane?: Target} => {interaction: string, kind: string, status: 'pending'}
# not allowed from a pane; push the task branch from the host (container clone pulled first, hooks off)
sandbox.push :: {task: Target, remote?: string = origin} => {kind: 'push', outcome: 'applied', detail: {remote: string, branch: string, synced: any, output: string}}
# not allowed from a pane; copy one regular file of the checkout/clone into <state>/outbox/<box>/
sandbox.copy_out :: {task: Target, path: string} => {kind: 'copy_out', outcome: 'applied', detail: {path: string}}
# not allowed from a pane; stores the `claude setup-token` output (0600) for projection as CLAUDE_CODE_OAUTH_TOKEN
sandbox.setup_token :: {token: string} => {stored: bool, path: string, env: string}
# not allowed from a pane; container boxes only
sandbox.remove :: {task: Target, force?: bool = false} => {container: string, action: string, sync: any, leftovers: any, error?: string, unsynced_kept: bool}
# not allowed from a pane; container boxes only
sandbox.start :: {task: Target} => {state: string, created: bool}
sandbox.status :: {} => {levels: [{level: string, available: bool, detail?: any, hint?: string}], config: object, container: {runtime?: string|null, docker_sandboxes?: {plugin: any, note: string}|null, vibeke_linux: any, vibeke_linux_hint: string}}
# not allowed from a pane; container boxes only
sandbox.stop :: {task: Target} => {state: string}
"##;

/// Batch 2A API surface (07 §2.1, §2.2, §2.6, §2.10, §2.14, §2.15): `session_api`, `config_api`,
/// `blob_api`, `pane_api` and `task_park`.
const BATCH_2A_SHAPES: &str = r##"
# --- session.* (runtime-dir scan; create spawns a server; create/stop/rename full scope only) ---
session.list :: {} => {sessions: [SessionEntry]}
session.create :: {name: string} => {session: SessionEntry}
session.stop :: {name: string, kill_panes?: bool = false} => {session: object, stopped: bool}
# only a stopped session without live holders can be renamed (conflict session_running | holders_live | name_taken)
session.rename :: {name: string, new_name: string} => {session: SessionEntry, previous_name: string}
# exec the server binary in place (holders keep running, the pid stays); full scope only
server.restart :: {binary?: string} => {new_pid: int, binary: string}

# --- config.* (layers, lowest first: default < user < repo < runtime < cli) ---
# repo | cwd | pane: layer that repository's .vibeke/config.toml in (only while trusted)
config.get :: {key?: string, repo?: string, cwd?: string, pane?: Target}
  => {key?: string, value: any, source: default|user|repo|runtime|cli, path: string, overrides: [string], cli_overrides: [string], layers: [ConfigLayer], repo: {root: string, file: string|null, trusted: bool, applied: bool, warnings?: [string], error?: string}|null, errors: [ConfigDiagnostic]}
# runtime override unless persist (written to config.toml atomically, comments kept); value null resets the key; full scope only
config.set :: {key: string, value: any, persist?: bool = false}
  => {key: string, value: any, persisted: bool, path: string, changed: [string]}
config.validate :: {path?: string} => {path: string, valid: bool, errors: [ConfigDiagnostic], warnings: [ConfigWarning]}
# same as server.reload_config; full scope only
config.reload :: {} => {changed: [string], errors: [ConfigDiagnostic], warnings?: [ConfigWarning]}

# --- blob.* (blake3 hashes; session blob store and pane inbox) ---
blob.get :: {hash: string, range?: {offset?: int, length?: int}}
  => {hash: string, mime: string, size: int, offset: int, length: int, data_b64: string}
blob.stat :: {hash: string} => {hash: string, mime: string, size: int, created_at: int, refs: int, path: string}

# --- pane.move / scroll / screenshot ---
pane.move :: {pane?: Target, to: {tab?: Target, workspace?: Target, new_tab_in?: Target}, direction?: right|down|left|up = right, anchor?: Target, position?: {pane?: Target, direction?: string}, focus?: bool = false}
  => {pane: Pane, tab: Tab, previous_pane_handle: string, source_tab_closed: bool}
# asks attached clients to scroll their view (event pane.scroll_requested); offset = rows above the live screen
pane.scroll :: {pane?: Target, to?: bottom|top|line, line?: int, delta?: int, client?: string}
  => {scroll: {offset: int, total: int, at_bottom: bool}}
# svg: text spans on a cell grid; png: rasterized with an embedded 8x16 bitmap font (inline png comes as data_b64)
pane.screenshot :: {pane?: Target, format?: text|ansi|html|svg|png = ansi, source?: visible|recent = visible, lines?: int = 200, include_cursor?: bool, inline?: bool}
  => {blob: {hash: string, size: int, mime: string, path: string}, format: string, source: string, cols: int, rows: int, lines: int, revision: int, width?: int, height?: int, cursor?: {row: int, col: int, visible: bool}, data?: string, data_b64?: string}

# --- task.park / resume (full scope only) ---
task.park :: {task: Target} => {task: Task, stopped: [{run: string, handle: string, pane: string, harness: string, name: string|null, pane_closed: bool, resumable: bool}], note?: string}
task.resume :: {task: Target} => {task: Task, resumed: [AgentRun], skipped: [{run: string, reason: string}]}
"##;

/// The v1 server/API remainder: `sync_input`, `task_lifecycle`.
const V1_REMAINDER_SHAPES: &str = r##"
# --- pane.sync_input (full scope only; agents join only with include_agents) ---
# action defaults from enabled (true = start, false = stop), else status; start without panes = the tab (default: focused)
pane.sync_input :: {action?: start|stop|status|on|off, enabled?: bool, panes?: [Target]|string, tab?: Target, pane?: Target, group?: string, all?: bool, include_agents?: bool = false}
  => {group_id: string|null, group?: SyncGroup, excluded?: [{pane: string, handle: string, reason: string}], stopped?: [string], groups: [SyncGroup]}

# --- tab.renumber (08 §3; full scope only): numbers 1..n in tab order, handles follow ---
tab.renumber :: {workspace?: Target} => {workspace: string, tabs: [Tab], changed: int}

# --- task lifecycle (05 §1, §4, §6, §8) ---
# readable from a pane for tasks of its own workspace
task.setup_log :: {task: Target, max_bytes?: int = 262144}
  => {task: string, path: string, exists: bool, text: string, size: int, truncated: bool, setup_status: string|null}
# full scope only: stops agents (park record kept), deletes the worktree, keeps the branch; dirty needs force
task.archive :: {task: Target, force?: bool = false}
  => {task: Task, job?: any, stopped?: [any], branch_kept?: string|null, worktree_removed?: bool, archived?: bool}
# full scope only: records an existing git worktree (path, or a pane's cwd) as an owned task; moves nothing
task.adopt :: {path?: string, pane?: Target, title?: string, slug?: string, focus?: bool = false}
  => {task: Task, workspace: Workspace, created_workspace: bool, warnings: [string]}
# full scope only: a missing task's worktree back at its path, from its branch
task.recreate :: {task: Target} => {task: Task, path: string, branch: string, new_workspace: bool}
# full scope only: closes a missing or finished task's record (force for others); never touches files
task.forget :: {task: Target, force?: bool = false} => {task: Task, files_touched: bool}
task.ports :: {task: Target} => TaskPorts
# full scope only: another block of the same size; running panes keep the old env
task.ports.re_lease :: {task: Target}
  => {task: string, handle: string, lease: {start: int, end: int, count: int}|null, env: object, old_lease: {start: int, end: int}|null, note: string}
"##;

/// Adapter polish (04 §7.7, §10, §12.3, §13; `crate::agents::polish`).
const ADAPTER_POLISH_SHAPES: &str = r##"
# --- 2F adapter polish ---
# fingerprints approved at least min_count times with at most max_denials denials, as ready-to-paste
# rules (04 §7.7); `rule` is null (and `blocked` says why) for risky or compound commands
policy.suggest :: {min_count?: int = 3, max_denials?: int = 0, limit?: int = 50, harness?: string, include_covered?: bool = false}
  => {suggestions: [{fingerprint: string, harness: string, tool: string, subject: string, workspace: string, approvals: int, denials: int, last_at_ms: int, risk: low|medium|high|unknown, rule: object|null, toml: string|null, blocked: string|null, covered: bool}], min_count: int, max_denials: int, samples: int}
# compact per-turn records written by the transcript tailer (04 §10); cost_source: harness|price_table|subscription|none
agent.turn_usage :: {run?: Target, target?: Target, limit?: int = 200}
  => {run: string, turns: [{id: string, run: string, harness: string, n: int, native_id: string, model: string|null, input: int, output: int, cache_read: int, cache_write: int, cost_usd: number|null, cost_source: string, stop: string|null, ended_at_ms: int|null, source: string}], turn_count: int, totals: {input: int, output: int, cache_read: int, cache_write: int, cost_usd: number|null}, usage: any}
# the "limits" status segment's data: the latest rate-limit observation per harness
agent.limits :: {} => {limits: [{harness: string, scope: string|null, limited: bool, used_percent: number|null, resets_at_ms: int|null, message: string|null, observed_at_ms: int}]}
# drift telemetry (04 §12.3), local only
agent.drift :: {} => {versions: [{harness: string, version: string, observations: int, disagreements: int, unknown_resolutions: int, answer_failures: int, rate: number, drifting: bool}]}
# poll the signed manifest channel now (04 §13); refused unless the index verifies
agent.manifests_check :: {url?: string} => {serial: int, applied: [string], skipped?: [string], unsigned?: bool, warnings?: [string], announced?: int, unchanged?: bool, index_serial?: int}
# freeze a cached remote manifest at its version, or release the pin
agent.manifest_pin :: {id: string, version?: string, unpin?: bool = false} => {id: string, version?: string, pinned_at_ms?: int, unpinned?: bool}
"##;

/// Server security (09, `crate::security`): policy, auth, audit, integration integrity.
const SECURITY_SHAPES: &str = r##"
# --- policy.* (07 §2.9, 09 §4); full scope only ---
# merged view: config.toml rules, policy.add rules, then trusted repositories' .vibeke/policy.toml
# (every trusted repository without a scope; with one, the repository found from it)
policy.list :: {scope?: PolicyScope} => {rules: [PolicyRuleInfo], repos: [PolicyRepo]}
# a rule needs at least one matcher; given as `rule` or as top-level fields
policy.add :: {rule?: {match?: PolicyMatch, tool?: string, command_regex?: string, path_glob?: string, url_glob?: string, effect: allow|deny|ask, scope?: string, note?: string}, tool?: string, command_regex?: string, path_glob?: string, url_glob?: string, effect?: allow|deny|ask, scope?: string, note?: string}
  => {rule: PolicyRuleInfo}
# only rules added with policy.add (`p-…`); config and repository rules live in their files
policy.remove :: {rule_id: string} => {removed: bool, rule: PolicyRuleInfo}
# dry run: what an approval of this action would get
policy.test :: {action: {tool: string, command?: string, paths?: [string], url?: string}, scope?: PolicyScope}
  => {effect: allow|deny|ask, rule: PolicyRuleInfo|null, user_rule: PolicyRuleInfo|null, repo_rule: PolicyRuleInfo|null, repo: string|null, reason: string}
# trust a repository's .vibeke/ at its current digest (09 §4); allow_policy_grants lets its allow rules apply
policy.trust :: {path?: string = ".", digest?: string, devcontainer_digest?: string, allow_policy_grants?: bool = false}
  => {repo: string, digest: string|null, setup_script: string|null, task_file: any, devcontainer: any, harness_manifests: [any], allow_policy_grants: bool, policy_rules: [PolicyRuleInfo], policy_errors?: [string]}
# --- auth.* (09 §3.2) ---
# full scope only: the pane's agent keeps running without API access until the pane restarts
auth.revoke_token :: {pane: Target} => {pane: string, revoked: true, tokens_removed: int, elevations_removed: int}
# from inside a pane: waits for the user's decision outside the pane (auth.elevate.decide); `request` resumes waiting, `wait: false` returns the pending request
auth.elevate :: {reason?: string, timeout_ms?: int = 120000, request?: string, wait?: bool = true}
  => {request: string, token: string, expires_at_ms: int, ttl_s: int, env: string} | ElevationRequest
# full scope only, never from a pane or an elevated connection
auth.elevate.decide :: {request: string, decision: approve|deny} => {request: string, pane: string, decision: approved|denied, expires_at_ms: int|null}
auth.list :: {} => {pending: [ElevationRequest], elevated: [{pane: string, request: string, expires_at_ms: int}], revoked: [{pane: string}]}
# --- audit.* (09 §11); full scope only ---
audit.tail :: {limit?: int = 50, types?: [string]|string} => {entries: [AuditEntry], path: string}
audit.search :: {query?: string, types?: [string]|string, since_ms?: int, limit?: int = 200} => {entries: [AuditEntry], path: string}
# verifies the rotated segments and the active log as one chain
audit.verify :: {} => {ok: bool, exists: bool, entries: int, last_seq: int, last_hash: string, problems: [string], discontinuities: [int], head_seq: int|null, path: string, segments: [string]}
# --- integration.* (09 §5.3); full scope only ---
integration.doctor :: {harness?: claude|codex|pi|omp|opencode|gemini}
  => {checks: [{name: string, harness: string, ok: bool, status: ok|changed|removed|unrecorded|not_installed, detail: string, file: string, fingerprint: string|null, recorded: string|null}]}
"##;

/// `type :: subject => data` for every event type the server emits.
pub const EVENT_SHAPES: &str = r##"
# type :: subject => data. Subjects name the objects an event is about (the `subjects` filter of
# events.subscribe matches these keys); data is the type-specific payload. Unlisted fields may
# appear (clients ignore unknown fields); high-frequency signals are not events.
session.server_restarted :: {} => {recovered_panes?: int, pid?: int, prev_pid?: int}
session.config_reloaded :: {} => {changed_keys: [string], new_panes_only: [string], source: watch|api|set|set_persist}
session.config_rejected :: {} => {errors: [ConfigDiagnostic], source: watch}
security.rate_limited :: {pane: string} => {method: string, limit: requests|spawn|depth|descendants}
pane.scroll_requested :: {pane: string, tab?: string, workspace?: string} => {offset: int, total: int, client: string|null}
task.parked :: {task: string} => {runs: int, attached?: bool}
tab.renumbered :: {workspace: string} => {tabs: [{tab: string, from: int, to: int}]}
pane.sync_input_changed :: {group: string} => {enabled: bool, panes: [string], tab: string|null, reason: started|stopped|superseded}
task.archived :: {task: string} => {path: string|null, branch: string|null, job: any, runs: int}
task.adopted :: {task: string, workspace: string} => {path: string, branch: string|null, via: path|pane, main_worktree: bool, created_workspace: bool}
task.recreated :: {task: string} => {path: string, branch: string, new_workspace: bool}
task.forgotten :: {task: string} => {path: string|null, branch: string|null, previous_status: string}
task.ports_changed :: {task: string} => {old: [int]|null, new: [int]}
task.cleanup_suggested :: {task: string} => {reason: pr_merged, pr: int, url: string, hint: string}
task.resumed :: {task: string} => {runs: int, skipped: int}
group.created :: {group: string} => {name: string, parent: string|null}
group.renamed :: {group: string} => {name: string}
group.moved :: {group: string} => {parent: string|null, index: int}
group.collapsed :: {group: string} => {collapsed: bool}
group.closed :: {group: string} => {}
workspace.created :: {workspace: string} => {cwd?: string}
workspace.renamed :: {workspace: string} => {name: string|null}
workspace.moved :: {workspace: string} => {group?: string|null}
workspace.closed :: {workspace: string} => {}
tab.created :: {tab: string, workspace: string} => {number: int}
tab.renamed :: {tab: string} => {title: string|null}
tab.moved :: {tab: string, workspace: string} => {index: int}
tab.closed :: {tab: string, workspace: string} => {}
tab.layout_changed :: {tab: string, workspace?: string} => {floated?: string}
pane.created :: {pane: string, tab?: string, workspace?: string} => {}
pane.closed :: {pane: string, tab?: string, workspace?: string} => {reason?: string}
pane.exited :: {pane: string, tab?: string, workspace?: string} => {code?: int|null, signal?: any, reason?: string, respawn_error?: string}
pane.focused :: {pane: string, tab?: string, workspace?: string} => {client?: string}
pane.title_changed :: {pane: string, tab?: string, workspace?: string} => {title: string|null}
pane.cwd_changed :: {pane: string, tab?: string, workspace?: string} => {cwd: string|null}
pane.process_changed :: {pane: string, tab?: string, workspace?: string} => {fg_cmdline: [string]}
pane.marked_unread :: {pane: string, tab?: string, workspace?: string} => {marked: bool}
pane.seen :: {pane: string, tab?: string, workspace?: string} => {}
paste.translated :: {pane: string, tab?: string, workspace?: string} => {files: [{blob: string, bytes: int, local_name: string, dir: bool}], target_namespace: string}
pane.pinned :: {pane: string, tab?: string, workspace?: string} => {pinned: bool}
pane.recovered :: {pane: string, tab?: string, workspace?: string} => {method: string}
pane.moved :: {pane: string, tab?: string, workspace?: string} => {from_tab_id: string, to_tab_id: string}
pane.scroll_changed :: {pane: string, tab?: string, workspace?: string} => {offset: int, total: int, client?: string}
pane.output_matched :: {pane: string, tab?: string, workspace?: string} => {matched: any, revision: any}
pane.isolation_changed :: {pane: string} => {level: string, scope: string, network: string}
pane.input_unconfirmed :: {pane: string, run?: string} => {input_id: string, reason?: string, preview_chars?: int}
adapter.health_changed :: {run: string} => {to: string, from?: string, transport?: string, reason?: string}
adapter.disagreement :: {run: string} => {facet: string, structured: string, other: string, source?: string}
agent.detected :: {run: string, pane: string} => {harness: string, via: string, argv0?: string|null}
agent.started :: {run: string, pane: string} => {harness: string, via?: string}
agent.identified :: {run: string, pane: string} => {harness_session_id?: any, transcript_path?: any}
agent.state_changed :: {run: string, pane: string} => {facet: string, from?: string, to: string, source?: string, confidence?: number, inferred?: bool}
agent.named :: {run: string} => {name: string|null}
agent.turn_started :: {run: string, pane: string} => any
agent.turn_completed :: {run: string, pane: string} => {stop_reason?: any, usage?: any}
agent.usage :: {run: string, pane: string} => {input: int, output: int, cache_read: int, cache_write: int, cost_usd: number|null, source: string}
agent.file_changed :: {run: string, pane: string} => {path: string, op: string}
agent.rate_limited :: {run: string, pane: string} => {resets_at_ms: int|null, message: string|null}
agent.resume_handle :: {run: string} => {argv: [string]}
agent.session_ended :: {run: string, pane: string} => {reason: string}
agent.harness_version_unvalidated :: {run: string, pane: string} => {harness: string, version: any}
agent.turn_usage :: {run: string, pane: string} => {turn: int, native_id: string, model: string|null, input: int, output: int, cache_read: int, cache_write: int, cost_usd: number|null, cost_source: string, source: string}
agent.cwd_changed :: {run: string, pane: string} => {cwd: string, old_cwd: string|null}
agent.tool_blocked :: {run: string, pane: string} => {tool: string, effect: string, rule: string|null, command: string|null}
agent.drift_detected :: {run: string, pane: string} => {harness: string, version: string, observations: int, disagreements: int, unknown_resolutions: int, answer_failures: int, rate: number}
harness.manifest_loaded :: {manifest: string} => {id: string, version: string, source: string, serial: int, verified: string}
agent.exited :: {run: string, pane: string} => {reason: string, harness: string}
interaction.opened :: {interaction?: string, pane?: string, run?: string} => any
interaction.updated :: {interaction: string, pane: string} => {gate?: bool, reason?: string}
interaction.decided :: {interaction: string, pane: string, run: string} => {rev: int, by: string, decision: string|null, channel?: string}
interaction.delivery_unknown :: {interaction: string, pane: string} => {reason: string}
interaction.cancelled :: {interaction: string, pane: string} => {reason: string}
interaction.expired :: {interaction: string, pane?: string} => any
policy.rule_matched :: {interaction: string} => {effect: string|null}
policy.repo_trusted :: {} => {repo: string, digest: string, devcontainer_digest?: string|null}
task.created :: {task: string, workspace: string} => {title: string, branch: string|null, path: string|null}
task.status_changed :: {task: string} => {status: string}
task.files_materialized :: {task: string} => {files: [MaterializedFile], deps: any}
task.setup_started :: {task: string} => {commands: [{source: string, command: string}], pane: string|null}
task.setup_finished :: {task: string} => {status: string, exit_code?: int|null, duration_ms?: int, log?: string, pane?: string|null}
task.setup_failed :: {task: string} => {status: string, exit_code?: int|null, duration_ms?: int, log?: string, pane?: string|null}
task.setup_untrusted :: {task: string} => {repo: string, digest: string, script: string, hint: string, commands?: [{source: string, command: string}]}
task.agents_withheld :: {task: string} => {reason: string, hint: string}
task.missing :: {task: string} => {path: string, reason: string, hint: string}
task.recovered :: {task: string} => {path: string|null}
task.branch_changed :: {task: string} => {path: string, expected: string|null, actual: string|null}
worktree.orphan_found :: {repo: string} => {path: string, kind: string, branch: string|null, hint: string}
task.tracked :: {task: string, run: string} => {intent_revision: int, binding: string, start_turn: any}
task.intent_updated :: {task: string} => {revision: int, previous: any}
task.binding_changed :: {task: string, run: string, binding: string} => {state: string, reason?: string, offers?: [string]}
task.message_prepared :: {task: string, message: string} => {run: string}
task.message_sending :: {task: string, message: string} => {}
task.message_delivery_unknown :: {task: string, message: string} => {detail: any}
task.dependency_changed :: {task: string, depends_on: string, edge: string} => {action: added|removed, kind: string}
task.finished :: {task: string} => {status: string, attached: bool, reviewed: any}
task.updated :: {task: string} => {priority?: any, effort?: any, effort_source?: string}
check.authorized :: {task: string, check: string} => {subject: string, definition_digest: string, trust: any}
check.queued :: {task: string, check: string, check_run: string} => {subject: string, head: string}
check.started :: {task: string, check: string, check_run: string} => {}
review.candidate_created :: {task: string} => {subject: string, head: string, source: string}
review.end_candidate_pinned :: {task: string, binding: string, run: string} => {subject: string, head: string, note: any}
review.accepted :: {task: string} => any
review.invalidated :: {task: string, acceptance: string} => {subject: string, reasons: any}
review.label_changed :: {task: string} => {from: any, to: any}
review.note_classified :: {task: string, note: string} => any
review.notes_recorded :: {task: string, run: string, binding: string} => {turn: any, count: int, subject: any, open_concerns: any}
review.reviewer_requested :: {task: string, request: string} => any
review.reviewer_started :: {task: string, request: string, run: string} => {subject: string, harness: string, binding: string, prompt_digest: string}
review.reviewer_unknown :: {task: string, request: string} => {reason: string, subject: string}
review.snapshot_created :: {task: string, subject: string} => {head: string, content: string, ref: string, attempts: any}
review.snapshot_refs_removed :: {repo: string, task: any} => any
review.human_reviewed :: {task: string, criterion: string, subject: string} => {verdict: string, screenshots: int, intent_revision: int}
review.purged :: {scope: any} => ReviewPurge
review.reviewer_checkout_removed :: {task: string, request: string} => {run: string | null}
worktree.created :: {workspace?: string} => any
worktree.opened :: {workspace: string} => {path: string, branch: string|null, repo_root: string, created_workspace?: any}
worktree.removed :: {path: string, task?: string} => {job?: string, state: string}
preview.opened :: {machine?: string, preview?: string, preview_handle?: string, pane?: string|null, task?: string|null} => {url: string, opened_in: pane|window|proxy|string, profile?: string, browser?: string, tls_origin?: bool, pane?: string|null}
preview.discovered :: {preview: string, preview_handle: string, pane: string|null, task: string|null, machine: string} => {preview: Preview}
preview.declared :: {preview: string, preview_handle: string, pane: string|null, task: string|null, machine: string} => {preview: Preview}
preview.up :: {preview: string, preview_handle: string, pane: string|null, task: string|null, machine: string} => {preview: Preview}
preview.down :: {preview: string, preview_handle: string, pane: string|null, task: string|null, machine: string} => {preview: Preview}
preview.gone :: {preview: string, preview_handle: string, pane: string|null, task: string|null, machine: string} => {preview: Preview}
preview.mirrored :: {machine: string, preview: string, preview_handle: string} => {local_port: int, url: string, warning: string}
preview.unmirrored :: {machine: string, preview: string, preview_handle: string} => {local_port: int}
preview.console_error :: {preview: string, preview_id: string, pane: string|null, task: string|null, machine: string} => {count: int, source: string, text: string, url: string|null, line: int|null, session: string|null}
browser.navigated :: {pane: string, tab?: string, workspace?: string} => {url: string}
browser.viewport_changed :: {pane: string, tab?: string, workspace?: string} => {device: string|null, viewport: any}
notification.created :: {pane: string|null} => {id: string, kind: string, title: string, body: string, urgency: string}
notes.updated :: {workspace: string} => {rev: int, bytes: int}
draft.created :: {draft?: string} => any
draft.updated :: {draft?: string} => any
draft.deleted :: {draft?: string} => {rev: int}
draft.reordered :: {scope: string, scope_id: string, workspace: any} => {count: int}
draft.sending :: {draft?: string} => {send: string, run: string, include_notes: bool, bytes: int}
draft.delivery_unknown :: {draft?: string} => any
desk.forgotten :: {scope: any} => {rows: int, sessions: int}
scrollback.forgotten :: {scope: any} => {panes: int, segments: int, bytes: int, fts_rows: int, panes_dropped: int}
state.forgotten :: {scope: any} => {counts: object}
security.encryption_migrated :: {} => {to: sealed|plain, files: int, failed: int}
tombstone :: {} => {}
layout.applied :: {workspace: string} => {name: string|null, tabs: int, panes: int, new_workspace: any}
attention.preference_changed :: {key: {kind: string, id: string}} => {seen: bool, snoozed_until_ms: int|null, pinned: bool}
assistant.consent_granted :: {workspace?: string} => any
assistant.consent_revoked :: {workspace?: string, remote_workspace?: string} => {grants: int, cancelled: int}
assistant.purged :: {} => {count: int, reason: string}
assistant.request_created :: {assistant_request: string} => any
assistant.request_started :: {assistant_request: string} => any
assistant.request_finished :: {assistant_request: string} => any
assistant.models_refreshed :: {assistant_connection: string} => {adapter: string, endpoint_host: string, models: int}
assistant.test_finished :: {assistant_connection: string} => any
assistant.stall_notice :: {assistant_request: string, run?: any} => {request: string}
# transient (seq 0, tier "transient"): streamed text of a running request; not outbox history, never replayed, full-scope subscribers only
assistant.delta :: {assistant_request: string} => {request: string, seq: int, text: string}
sandbox.created :: {task: string, sandbox: string} => {level: string, provider: string, network: string, yolo: bool, proxy_port?: int|null, credentials?: any}
sandbox.boundary_action :: {task?: string|null, sandbox: string} => {kind: push|copy_out|credential_use, interaction: string|null, outcome: applied|failed|denied|expired|cancelled, detail?: any, credentials?: [string], harnesses?: [string]}
sandbox.runner_lost :: {task: string, sandbox: string} => {state: string, runs: [string], resumable: [string]}
sandbox.recovered :: {task: string, sandbox: string} => {created: bool, resumed: int, skipped: int}
sandbox.resource_pressure :: {task?: string|null, sandbox: string} => {resource: memory|pids|cpus, value: number, limit: number, share: number}
sandbox.suggested :: {run: string, pane: string} => {harness: string, level: string, hint: string}
sandbox.relaunched :: {run: string, pane: string} => {from: string, level: string}
sandbox.host_yolo_confirmed :: {workspace: string} => {}
sandbox.credentials_notice :: {task?: string|null, sandbox: string} => {harness: string, level: string, message: string}
sandbox.port_forwarded :: {task?: string|null, sandbox: string} => {box_port: int, host_port: int, source: declared|discovered}
sandbox.template_created :: {task?: string|null, sandbox: string} => {template: string, image: string}
sandbox.warm_ready :: {sandbox: string} => {pool: string, index: int}
sandbox.warm_claimed :: {task?: string|null, sandbox: string} => {slot: string}
sandbox.warm_recycled :: {sandbox: string} => {pool: string}
sandbox.pruned :: {} => {containers: int, dirs: int}
sandbox.egress_revoked :: {task?: string|null, sandbox?: string} => {host: string, scope: global|task}
screenshot.captured :: {machine?: string} => any
screenshot.deleted :: {machine: string} => {ids: [string]}
client.action :: {method: string, pane: any, target: any} => any
client.confirm_requested :: {confirm: string} => {title: string, body: any, options: any, timeout_ms: int, from: string}
client.confirm_resolved :: {confirm: string} => {choice: any, timed_out: bool}
client.devices_changed :: {client: string} => {devices: int}
client.window_title_changed :: {} => {title: string|null}
theme.changed :: {} => any
client.attached :: {client: string} => {kind: string, remote: bool}
client.detached :: {client: string} => {kind: string, remote: bool}
audit.recorded :: {} => {seq: int, type: string, hash: string}
auth.token_revoked :: {pane: string, pane_handle: string, tab: string, workspace: string} => {tokens_removed: int, elevations_removed: int}
auth.elevate_requested :: {pane: string, request: string} => {reason: string}
auth.elevate_granted :: {pane: string, request: string} => {expires_at_ms: int|null}
auth.elevate_denied :: {pane: string, request: string} => {expires_at_ms: int|null}
policy.rule_added :: {rule: string} => {rule: PolicyRuleInfo}
policy.rule_removed :: {rule: string} => {rule: PolicyRuleInfo}
integration.tampered :: {run: string|null} => {harness: string, reason: changed|removed|changed_during_run, file: string}
"##;

/// JSON-RPC notifications the server pushes on a connection.
pub const NOTIFICATION_SHAPES: &str = r##"
events.event :: {subscription_id: string, event: Event} => {}
events.overflow :: {subscription_id: string, resume_from: Cursor} => {}
events.closed :: {subscription_id: string, reason: string} => {}
"##;

/// Config and store hardening (02; 3D): Machine/Session entities, Turn/Item stream, storage
/// status, blob store maintenance.
const BATCH_3D_DEFS: &str = r##"
MachineKind = local|ssh|quic
MachineStatus = connected|connecting|degraded|offline
Machine = {id: string, label: string, kind: MachineKind, address: string|null, os: string, arch: string, vibeke_version: string, status: MachineStatus, last_seen_ms: int}
SessionInfo = {id: string, name: string, machine_id: string, created_at_ms: int, server_pid: int, server_version: string}
TurnUsage = {input_tokens: int, output_tokens: int, cache_read: int, cache_write: int, cost_usd: number|null}
Turn = {id: string, run_id: string, seq: int, started_at_ms: int, ended_at_ms: int|null, input_summary: string, status: running|completed|interrupted|failed, usage: TurnUsage|null, usage_baseline: TurnUsage|null, item_count: int}
ItemKind = user_message|assistant_message|reasoning|tool_call|tool_result|file_change|command|plan|subagent|error
FileChange = {path: string, op: create|modify|delete|rename, lines_added: int|null, lines_removed: int|null}
Item = {id: string, turn_id: string, run_id: string, seq: int, kind: ItemKind, started_at_ms: int, ended_at_ms: int|null, summary: string, payload_ref: string|null, file_change: FileChange|null, native_id: string|null}
"##;

const BATCH_3D_SHAPES: &str = r##"
# --- session.info, machine.* (02 §1.1; remote machines are reported by clients) ---
session.info :: {} => {session: SessionInfo, machine: Machine|null, cursor: Cursor}
machine.list :: {} => {machines: [Machine], local: string}
machine.get :: {machine: Target} => {machine: Machine}
# registers or updates a remote machine (kind ssh|quic) and emits machine.added/connected/disconnected/degraded; this machine is registered by the server; full scope only
machine.upsert :: {label: string, kind?: ssh|quic = ssh, id?: string, address?: string, os?: string, arch?: string, vibeke_version?: string, status?: connected|connecting|degraded|offline, reason?: string}
  => {machine: Machine, created: bool, cursor: Cursor}
machine.remove :: {machine: Target} => {removed: string, cursor: Cursor}

# --- storage.* (retention, backups, degraded mode; full scope only) ---
storage.status :: {}
  => {degraded: string|null, ephemeral: int, archive_rows_skipped: int, db: {path: string, bytes: int}, events: {count: int, first_seq: int, last_seq: int, retention: {sync_days: int, history_days: int, max_rows: int, blob_days: int}}, backups: [{name: string, schema_version: int, created_at: int, bytes: int}], keep_backups: int, blobs: {count: int, bytes: int}, cursor: {machine_uuid: string, session_uuid: string, log_epoch: string}}
# runs the retention sweep now (events past retention and over the row cap, old turns, unreferenced old blobs); refused while degraded
storage.prune :: {} => {events_aged: int, events_capped: int, events_remaining: int, stream_removed: int, blobs_removed: int, blob_bytes: int}

# --- blob.stats / blob.gc (the unified blob store; full scope only) ---
blob.stats :: {} => {count: int, bytes: int, by_source: object, path: string}
# removes unreferenced upload and payload blobs older than older_than_days; screenshots follow their own retention
blob.gc :: {dry_run?: bool = false, older_than_days?: int = 30}
  => {dry_run: bool, older_than_days: int, removed: int, bytes: int, kept_referenced: int, kept_young: int, kept_uncollectable: int, hashes: [string]}

# --- agent.turns / agent.items (Turn/Item stream; full scope only) ---
agent.turns :: {run: Target, after_seq?: int, limit?: int} => {run: string, turns: [Turn], next_after_seq: int|null}
agent.items :: {turn?: string, run?: Target, kind?: ItemKind, after_seq?: int, limit?: int} => {items: [Item], next_after_seq: int|null}
"##;

const BATCH_3D_EVENT_SHAPES: &str = r##"
session.started :: {} => {pid: int, version: string, machine: string, name: string, prev_pid: int|null, fresh: bool}
session.stopped :: {} => {pid: int, reason: string}
machine.added :: {machine: string} => {label: string, kind: string, address: string|null}
machine.connected :: {machine: string} => {label: string}
machine.disconnected :: {machine: string} => {label: string, reason: string|null}
machine.degraded :: {machine: string} => {label: string, reason: string|null}
machine.removed :: {machine: string} => {label: string}
agent.item :: {run: string, pane: string} => {kind: ItemKind, summary: string, item: string, turn: string, seq: int, payload_ref: string|null}
agent.subagent_started :: {run: string, pane: string} => {agent_id: string|null, agent_type: string}
agent.subagent_finished :: {run: string, pane: string} => {agent_id: string|null, agent_type: string}
"##;

/// Native plugin runtime (07 §7.1–7.6, lane 3B; `crate::plugin_native`).
const BATCH_3B_SHAPES: &str = r##"
# --- registry over the API (full scope only; 07 §2.16) ---
# native: refused with permission_denied capabilities_not_accepted (details.requested_capabilities) until accept_capabilities covers every requested item; Herdr manifests need trust herdr_legacy
plugin.install :: {source: string, ref?: string, accept_capabilities?: [string], trust?: scoped|herdr_legacy = scoped}
  => {plugin: string, kind: native|herdr, requested_capabilities: [object], entrypoints?: [string], status: string}
plugin.link :: {path: string} => {plugin: string, kind: native|herdr, requested_capabilities?: [object]}
plugin.enable :: {plugin: string} => {plugin: string, enabled: bool}
plugin.disable :: {plugin: string} => {plugin: string, enabled: bool}
plugin.remove :: {plugin: string, purge_data?: bool = false} => {plugin: string, removed: bool, kind: native|herdr}
# re-consent after a widening update; runs [[build]] when the managed checkout needs it
plugin.consent :: {plugin: string, accept_capabilities: [string]} => {plugin: string, consent_id: string, capabilities: object}
plugin.restart :: {plugin: string} => {plugin: string, restarted: bool}
# waits for the action (07 §7.2): last 4 KiB of stdout; an action without a command is sent to the process
plugin.action :: {plugin?: string, action: string, context?: {workspace?: Target, pane?: Target, tab?: Target}, source?: string}
  => {exit_code: int|null, stdout_tail: string|null, status: string, log: object}
# --- plugin-only (a plugin token) ---
plugin.kv.get :: {key: string} => {key: string, found: bool, value?: any, value_b64?: string}
plugin.kv.set :: {key: string, value?: any, value_b64?: string} => {key: string, bytes: int, used: int, quota: int}
plugin.kv.delete :: {key: string} => {key: string, deleted: bool}
plugin.kv.list :: {prefix?: string, after?: string, limit?: int = 100} => {keys: [{key: string, bytes: int}], next: string|null}
ui.contribute :: {contributions?: [object], replace?: bool = false, remove?: [{kind: string, id: string}]} => {plugin: string, contributions: int}
# --- clients ---
ui.contributions :: {} => {contributions: [object]}
ui.pane.open :: {plugin: string, pane: string, target?: Target, direction?: right|down|left|up = right} => {pane: Pane}
"##;

const BATCH_3B_EVENT_SHAPES: &str = r##"
plugin.installed :: {plugin: string} => {kind: string, update?: bool}
plugin.linked :: {plugin: string} => {kind: string, update?: bool}
plugin.unlinked :: {plugin: string} => {kind: string}
plugin.uninstalled :: {plugin: string} => {kind: string}
plugin.enabled :: {plugin: string} => {kind: string}
plugin.disabled :: {plugin: string} => {kind: string}
plugin.trust_changed :: {plugin: string} => {kind: string, trusted: bool}
plugin.registry_observed :: {} => {generation: int, plugins: int}
plugin.reloaded :: {plugin: string} => {reason: string}
plugin.crashed :: {plugin: string} => {exit_code: int|null, reason: string, crashes_in_window: int, restart: bool, restart_in_ms: int|null, disabled: bool, stderr_tail: string}
plugin.process_started :: {plugin: string} => {pid: int, sandbox: bool, restarts: int}
plugin.process_stopped :: {plugin: string} => {exit_code: int|null, signal: int|null, requested: bool}
plugin.action_invoked :: {plugin: string} => {action: string, log: string, source: string, via: argv|process}
plugin.command_finished :: {plugin: string} => {log: string, what: string, status: completed|failed, exit_code: int|null}
plugin.capability_violation :: {plugin: string} => {method: string, reason: string, token?: string}
plugin.api_call :: {plugin: string} => {method: string, token?: string, ok?: bool, error_code?: string|null}
plugin.launch_failed :: {plugin: string} => {what: string, error: string}
plugin.pane_opened :: {plugin: string, pane?: string} => {entrypoint?: string}
ui.contributions_changed :: {plugin: string} => {contributions: int}
"##;

/// Evidence and PR integration (15 §6.3, §6.4; 3F).
const BATCH_3F_DEFS: &str = r##"
PrObservation = {id: string, task: string, requested?: string, lookup: object, observed_at_ms: int, authorization_scope: string, provider: string, observed_by: object, criterion_ids: [string], current?: bool, assessment?: object}
PrClaim = {id: string, task: string, url: string, identity?: object, source: pasted_url|agent_statement, claimed_by: object, claimed_at_ms: int, text?: string, label?: string, confirmed?: bool}
"##;

const BATCH_3F_SHAPES: &str = r##"
# --- task.pr.* (PR evidence; 15 §6.4) ---
# an explicit lookup through the user's authenticated gh CLI; a failed or offline lookup is recorded as such (outcome unknown); not allowed from a pane scope
task.pr.observe :: {task: Target, pr?: string, criteria?: [string], idempotency_key?: string}
  => {observation: PrObservation, label: string, note: string}
# a pasted URL or an agent statement: shown with the review, never confirmation
task.pr.claim :: {task: Target, url: string, text?: string, idempotency_key?: string}
  => {claim: PrClaim, label: string, note: string}
# with subject, each observation also carries its assessment for that subject (binding, outcome, summary)
task.pr.list :: {task: Target, subject?: string}
  => {task: string, observations: [PrObservation], claims: [PrClaim], provider: string, max_age_secs: int}

# --- execution-interval binding of observed commands (15 §6.3) ---
task.review.intervals :: {task: Target, limit?: int}
  => {task: string, intervals: [object]}
task.review.interval_status :: {}
  => {enabled: bool, watcher: string, settle_ms: int, arm_delay_ms: int, harnesses: [string], checkouts: [object], note: string}
"##;

const BATCH_3F_EVENT_SHAPES: &str = r##"
review.pr_observed :: {task: string} => {observation: string, lookup: observed|no_pr|failed, pr: string|null, head: string|null, state: open|closed|merged|null, draft: bool|null, checks: none|pending|passing|failing|null, reason: string|null}
review.pr_head_changed :: {task: string} => {pr: string|null, from: string|null, to: string|null, observation: string}
review.pr_claimed :: {task: string} => {claim: string, url: string, pr: string|null, source: pasted_url|agent_statement}
"##;

/// Collision tracker and claims (05 §10; 3A).
const BATCH_3A_DEFS: &str = r##"
CollisionRunInfo = {run: string, handle: string|null, name: string|null, harness: string|null, pane: string|null, task: string|null, state: string|null, alive: bool}
CollisionPath = {path: string, severity: low|medium|high, reason: object, runs: [string], ambiguous: bool, first_ms: int, last_ms: int}
Collision = {id: string, root: string, severity: low|medium|high, status: open|cleared|ignored, runs: [string], run_info: [CollisionRunInfo], paths: [CollisionPath], ambiguous: bool, first_ms: int, last_ms: int, headline: string, cleared_ms: int|null, cleared_reason: string|null, timeline?: [object]}
CollisionClaim = {id: string, run: string, run_handle: string|null, root: string, glob: string, created_ms: int, created_at_ms: int, note: string|null, task: string|null, kind: run|task}
CollisionIgnore = {id: string, root: string, path: string, created_ms: int, expires_ms: int|null, by: string}
"##;

const BATCH_3A_SHAPES: &str = r##"
# --- collision tracker (05 §10): advisory; it warns and never blocks, reverts or reassigns ---
# an agent (pane scope) sees the collisions of its own run only
collision.list :: {task?: Target, run?: Target, status?: open|cleared|ignored|all = open, limit?: int} => {collisions: [Collision], enabled: bool}
collision.get :: {collision: string} => {collision: Collision, claims: [CollisionClaim], steer: [{run: string, channel: string|null, reason?: string}]}
# full scope only
collision.status :: {} => {enabled: bool, fs_attribution: string, watcher: string, window_ms: int, read_window_ms: int, poll_interval_ms: int, enforce_claims: bool, roots: [object], claims: int, ignores: int, pending_context: int, note: string}
collision.ignores :: {root?: string} => {ignores: [CollisionIgnore]}
# "Ignore for this path": drops the path from open collisions and stops tracking it (optionally for a while)
collision.ignore :: {collision?: string, root?: string, path: string, for_secs?: int} => {ignore: CollisionIgnore, collision: Collision|null}
collision.unignore :: {ignore: string} => {removed: bool}
# the adapter's own interrupt of one run of the collision
collision.pause :: {collision: string, run: Target} => {run: AgentRun|null, paused: bool}
# native steer only (headless steer, Claude hook context on the next prompt or tool result); never typed into a TUI
collision.tell :: {collision: string, runs?: [Target], text?: string} => {results: [{run: string, status: delivered|queued|unsupported|failed, channel: string|null, reason?: string}], text: string}
# a task from the shared checkout's HEAD with a new run and a hand-off prompt; nothing is moved
collision.start_task :: {collision: string, run?: Target, title?: string, harness?: string, prompt?: string, dry_run?: bool = false} => {created: bool, base: string, title: string, harness: string, prompt: string, source_run: string|null, result: any}

# --- advisory run claims (a task's claims are `task.claim`, merge orchestration; the tracker reads both) ---
# a pane-scoped agent claims for its own run only; `task.claim {run}` of a run without a task is the same call
collision.claim :: {glob: string, run?: Target, task?: Target, root?: string, note?: string} => {claim: CollisionClaim, conflicts: [CollisionClaim], label: string}
# run claims and the task claims that bind each checkout
collision.claims :: {task?: Target, run?: Target, root?: string} => {claims: [CollisionClaim]}
collision.claim_release :: {claim?: string, run?: Target, glob?: string} => {released: [string]}
"##;

const BATCH_3A_EVENT_SHAPES: &str = r##"
task.collision_detected :: {collision: string} => {repo: string, severity: low|medium|high, reason: same_file|same_dir|read_then_edited|claim, paths: [string], new_paths: [string], runs: [string], new_runs: [string], ambiguous: bool, created: bool, raised: bool}
task.collision_cleared :: {collision: string} => {repo: string, reason: quiet|runs_ended|ignored|restart, severity: low|medium|high, runs: [string], paths: [string]}
task.collision_action :: {collision?: string|null} => {action: ignore|pause|tell|tell_delivered|start_task|claim_denied, path?: string, repo?: string, ignore?: string, run?: string, by?: string, results?: [object], text?: string, via?: string, messages?: int, base?: string, task?: any, source_run?: string|null, claim?: string, owner?: string}
task.collision_claim_added :: {claim: string, run: string} => {glob: string, repo: string, note: string|null, conflicts: [string], by: string}
task.collision_claim_released :: {claim: string, run: string} => {glob: string, repo: string, reason: released|run_ended}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_well_formed() {
        let p = problems();
        assert!(
            p.is_empty(),
            "api_schema registry problems:\n{}",
            p.join("\n")
        );
    }

    #[test]
    fn every_method_has_a_shape_and_no_shape_is_orphaned() {
        let reg = registry();
        let mut names: Vec<&str> = vec![];
        for (_, rows) in method_tables() {
            names.extend(rows.iter().map(|(n, _)| *n));
        }
        let missing: Vec<&&str> = names
            .iter()
            .filter(|n| !reg.methods.contains_key(**n))
            .collect();
        assert!(missing.is_empty(), "methods without a shape: {missing:?}");
        let orphans: Vec<&String> = reg
            .methods
            .keys()
            .filter(|k| !names.contains(&k.as_str()))
            .collect();
        assert!(
            orphans.is_empty(),
            "shapes for unknown methods: {orphans:?}"
        );
    }

    /// Every event type the workspace's source emits through `.event("…")` / `.event_by("…")`
    /// (the transactional outbox) is in the registry.
    #[test]
    fn every_emitted_event_type_is_listed() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = vec![];
        for c in std::fs::read_dir(&crates).unwrap().flatten() {
            let src = c.path().join("src");
            if src.is_dir() {
                walk(&src, &mut files);
            }
        }
        // Also the preview lifecycle names handed to `commit_previews` (`Some("preview.up")`).
        let re = regex::Regex::new(
            r#"\.event(?:_by)?\(\s*"([a-z_]+\.[a-z_.]+)"|Some\("(preview\.[a-z_]+)"\)"#,
        )
        .unwrap();
        let inline_tests = regex::Regex::new(r"(?m)^#\[cfg\(test\)\]\s*mod [a-z_]+\s*\{").unwrap();
        let reg = registry();
        let mut missing = std::collections::BTreeSet::new();
        for f in &files {
            // Test modules and files emit throwaway event names.
            let name = f.file_name().unwrap().to_string_lossy();
            if name.contains("tests") {
                continue;
            }
            let mut text = std::fs::read_to_string(f).unwrap();
            // Inline `#[cfg(test)] mod … { … }` blocks emit throwaway names too: cut each one
            // out, to its matching brace.
            while let Some(m) = inline_tests.find(&text) {
                let (start, body) = (m.start(), m.end());
                let mut depth = 1usize;
                let mut end = text.len();
                for (i, ch) in text[body..].char_indices() {
                    match ch {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = body + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                text.replace_range(start..end, "");
            }
            for c in re.captures_iter(&text) {
                let t = c.get(1).or_else(|| c.get(2)).unwrap().as_str();
                if !reg.events.contains_key(t) {
                    missing.insert(format!("{t} ({name})"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "event types emitted but not in EVENT_SHAPES: {missing:?}"
        );
    }

    #[test]
    fn bundle_is_json_schema_shaped() {
        let b = bundle();
        assert_eq!(b["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert!(b["x-methods"]["pane.send_text"]["params"]["required"].is_array());
        assert_eq!(b["x-errors"].as_array().unwrap().len(), 18);
        assert!(b["x-events"]["agent.state_changed"].is_object());
        // Mutating results carry the cursor.
        assert!(b["x-methods"]["workspace.create"]["result"]["properties"]["cursor"].is_object());
        assert!(b["x-methods"]["pane.list"]["result"]["properties"]["cursor"].is_null());
        // api.schema narrows to one method.
        let one = api_schema(Some("server.status")).unwrap();
        assert!(one["x-method"]["result"]["properties"]["pid"].is_object());
        assert!(api_schema(Some("no.such")).is_none());
    }
}
