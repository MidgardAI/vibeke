//! M6 API clients: the generated parts of the TypeScript (`clients/typescript/`) and Python
//! (`clients/python/`) clients, derived from the JSON Schema bundle of the schema registry
//! (`vk_server::api_schema`, the same source as `vibeke debug api-schema` and the API docs), a
//! drift check like the docs freeze, and an end-to-end test of both clients against a real
//! isolated server.
//!
//! Regenerate with `VIBEKE_UPDATE_CLIENTS=1 cargo test -p vibeke --test api_clients`; without
//! the variable a stale checked-in file fails the test. The hand-written runtimes
//! (`client.ts`, `client.py`) are checked in as they are.

use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn pascal(s: &str) -> String {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn entries(v: &Value) -> Vec<(&String, &Value)> {
    v.as_object()
        .map(|o| o.iter().collect())
        .unwrap_or_default()
}

fn is_free(s: &Value) -> bool {
    s.as_object().is_some_and(|o| o.is_empty()) || s == &Value::Bool(true)
}

fn arms(s: &Value) -> Option<&Vec<Value>> {
    s.get("anyOf").and_then(Value::as_array)
}

fn required(s: &Value) -> Vec<&str> {
    s.get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn ref_name(s: &Value) -> Option<&str> {
    s.get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
}

// ----------------------------------------------------------------------------------------------
// TypeScript
// ----------------------------------------------------------------------------------------------

fn ts_def_name(n: &str) -> String {
    match n {
        "Event" => "VibekeEvent".into(),
        n => n.into(),
    }
}

fn ts_key(k: &str) -> String {
    let ok = k
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ok {
        k.to_string()
    } else {
        serde_json::to_string(k).unwrap()
    }
}

/// The TypeScript type of a schema. Optional (`?`) means "may be absent"; `null` is part of a
/// type only where the schema says so (`anyOf` with `{type: null}`), exactly like the JSON Schema.
fn ts(s: &Value, ind: usize) -> String {
    if is_free(s) {
        return "unknown".into();
    }
    if let Some(n) = ref_name(s) {
        return ts_def_name(n);
    }
    if let Some(c) = s.get("const") {
        return c.to_string();
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        return e
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(a) = arms(s) {
        let mut parts: Vec<String> = vec![];
        for x in a {
            let t = ts(x, ind);
            if !parts.contains(&t) {
                parts.push(t);
            }
        }
        return parts.join(" | ");
    }
    match s.get("type").and_then(Value::as_str) {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => {
            let items = s.get("items").cloned().unwrap_or(Value::Bool(true));
            let inner = ts(&items, ind);
            let multi = arms(&items).is_some_and(|a| a.len() > 1)
                || items
                    .get("enum")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.len() > 1);
            if multi {
                format!("({inner})[]")
            } else {
                format!("{inner}[]")
            }
        }
        Some("object") => {
            if let Some(props) = s.get("properties") {
                let req = required(s);
                let pad = "  ".repeat(ind + 1);
                let mut out = String::from("{\n");
                for (k, v) in entries(props) {
                    let opt = !req.contains(&k.as_str());
                    let t = ts(v, ind + 1);
                    let _ = writeln!(
                        out,
                        "{pad}{}{}: {t};",
                        ts_key(k),
                        if opt { "?" } else { "" }
                    );
                }
                let _ = write!(out, "{}}}", "  ".repeat(ind));
                out
            } else if let Some(ap) = s.get("additionalProperties") {
                format!("{{ [key: string]: {} }}", ts(ap, ind))
            } else {
                "Record<string, unknown>".into()
            }
        }
        _ => "unknown".into(),
    }
}

const GENERATED: &str = "Generated by `VIBEKE_UPDATE_CLIENTS=1 cargo test -p vibeke --test api_clients` from the schema registry (crates/vk-server/src/api_schema.rs). Do not edit.";

fn gen_ts(b: &Value) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "// {GENERATED}\n");
    let _ = writeln!(o, "export const API_VERSION = {};\n", b["x-api"]);
    // Errors.
    let errors = b["x-errors"].as_array().unwrap();
    let kinds: Vec<String> = errors.iter().map(|e| e["kind"].to_string()).collect();
    let _ = writeln!(o, "export type ErrorKind = {};\n", kinds.join(" | "));
    let _ = writeln!(
        o,
        "export const ERROR_KINDS: Record<ErrorKind, {{ code: number; retryable: boolean }}> = {{"
    );
    for e in errors {
        let _ = writeln!(
            o,
            "  {}: {{ code: {}, retryable: {} }},",
            e["kind"], e["code"], e["retryable"]
        );
    }
    let _ = writeln!(o, "}};\n");
    // Shared types.
    let _ = writeln!(o, "// ---- shared types ----\n");
    for (n, d) in entries(&b["$defs"]) {
        let _ = writeln!(o, "export type {} = {};\n", ts_def_name(n), ts(d, 0));
    }
    // Methods.
    let _ = writeln!(o, "// ---- methods ----\n");
    let mut names = BTreeSet::new();
    let mut map = String::new();
    let mut info = String::new();
    for (m, e) in entries(&b["x-methods"]) {
        let p = pascal(m);
        assert!(names.insert(p.clone()), "type name clash for method {m}");
        let _ = writeln!(o, "export type {p}Params = {};\n", ts(&e["params"], 0));
        let _ = writeln!(o, "export type {p}Result = {};\n", ts(&e["result"], 0));
        let _ = writeln!(
            map,
            "  {}: {{ params: {p}Params; result: {p}Result }};",
            serde_json::to_string(m).unwrap()
        );
        let _ = writeln!(
            info,
            "  {}: {{ mutating: {}, scope: {}, paneScope: {} }},",
            serde_json::to_string(m).unwrap(),
            e["mutating"],
            e["scope"],
            e["pane_scope"]
        );
    }
    let _ = writeln!(
        o,
        "/** Params and result of every method. */\nexport interface Methods {{\n{map}}}\n"
    );
    let _ = writeln!(o, "export type MethodName = keyof Methods;\n");
    let _ = writeln!(
        o,
        "/** Mutating flag and scope of every method (`full`: not callable with a pane token). */\nexport const METHOD_INFO: Record<MethodName, {{ mutating: boolean; scope: \"full\" | \"pane\"; paneScope: \"forbidden\" | \"own_target\" | \"open\" }}> = {{\n{info}}};\n"
    );
    // Events.
    let _ = writeln!(o, "// ---- events ----\n");
    let mut emap = String::new();
    for (t, e) in entries(&b["x-events"]) {
        let p = pascal(t);
        assert!(
            names.insert(format!("{p}Subject")),
            "type name clash for event {t}"
        );
        let _ = writeln!(o, "export type {p}Subject = {};\n", ts(&e["subject"], 0));
        let _ = writeln!(o, "export type {p}Data = {};\n", ts(&e["data"], 0));
        let _ = writeln!(
            emap,
            "  {}: {{ subject: {p}Subject; data: {p}Data }};",
            serde_json::to_string(t).unwrap()
        );
    }
    let _ = writeln!(
        o,
        "/** Subject and data of every known event type; clients ignore unknown types. */\nexport interface EventMap {{\n{emap}}}\n"
    );
    let _ = writeln!(o, "export type EventType = keyof EventMap;\n");
    let _ = writeln!(
        o,
        "export type TypedEvent<T extends EventType> = Omit<VibekeEvent, \"type\" | \"subject\" | \"data\"> & {{\n  type: T;\n  subject: EventMap[T][\"subject\"];\n  data: EventMap[T][\"data\"];\n}};\n"
    );
    let _ = writeln!(
        o,
        "/** Narrow an event to a known type: `if (isEvent(e, \"pane.created\")) e.data`. */\nexport function isEvent<T extends EventType>(e: VibekeEvent, type: T): e is TypedEvent<T> {{\n  return e.type === type;\n}}"
    );
    o
}

// ----------------------------------------------------------------------------------------------
// Python
// ----------------------------------------------------------------------------------------------

struct Py {
    /// (name, code) of generated aliases and TypedDicts, in generation order.
    out: Vec<(String, String)>,
    names: BTreeSet<String>,
}

/// A JSON literal as Python source (`true` → `True`).
fn py_literal(v: &Value) -> String {
    match v {
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        v => v.to_string(),
    }
}

fn py_def_name(n: &str) -> String {
    match n {
        "Event" => "EventRecord".into(),
        n => n.into(),
    }
}

impl Py {
    fn unique(&mut self, want: &str) -> String {
        let mut n = want.to_string();
        let mut i = 2;
        while !self.names.insert(n.clone()) {
            n = format!("{want}{i}");
            i += 1;
        }
        n
    }

    /// The annotation for a schema; objects become named TypedDicts (hint names them).
    fn ty(&mut self, s: &Value, hint: &str, results: bool) -> String {
        if is_free(s) {
            return "Any".into();
        }
        if let Some(n) = ref_name(s) {
            return format!("\"{}\"", py_def_name(n));
        }
        if let Some(c) = s.get("const") {
            return format!("Literal[{}]", py_literal(c));
        }
        if let Some(e) = s.get("enum").and_then(Value::as_array) {
            let v: Vec<String> = e.iter().map(Value::to_string).collect();
            return format!("Literal[{}]", v.join(", "));
        }
        if let Some(a) = arms(s) {
            let mut parts: Vec<String> = vec![];
            let mut nullable = false;
            for (i, x) in a.iter().enumerate() {
                if x.get("type").and_then(Value::as_str) == Some("null") {
                    nullable = true;
                    continue;
                }
                let t = self.ty(x, &format!("{hint}V{i}"), results);
                if !parts.contains(&t) {
                    parts.push(t);
                }
            }
            let u = if parts.len() == 1 {
                parts.remove(0)
            } else {
                format!("Union[{}]", parts.join(", "))
            };
            return if nullable {
                format!("Optional[{u}]")
            } else {
                u
            };
        }
        match s.get("type").and_then(Value::as_str) {
            Some("string") => "str".into(),
            Some("integer") => "int".into(),
            Some("number") => "float".into(),
            Some("boolean") => "bool".into(),
            Some("null") => "None".into(),
            Some("array") => {
                let items = s.get("items").cloned().unwrap_or(Value::Bool(true));
                format!("List[{}]", self.ty(&items, &format!("{hint}Item"), results))
            }
            Some("object") => {
                if let Some(props) = s.get("properties") {
                    let name = self.unique(hint);
                    let code = self.typed_dict(&name, s, props, results);
                    self.out.push((name.clone(), code));
                    format!("\"{name}\"")
                } else if let Some(ap) = s.get("additionalProperties") {
                    format!(
                        "Dict[str, {}]",
                        self.ty(ap, &format!("{hint}Value"), results)
                    )
                } else {
                    "Dict[str, Any]".into()
                }
            }
            _ => "Any".into(),
        }
    }

    /// Optional fields are `NotRequired` (may be absent); `Optional[...]` (may be `None`) only
    /// where the schema includes `null`.
    fn typed_dict(&mut self, name: &str, s: &Value, props: &Value, results: bool) -> String {
        let req = required(s);
        let mut fields = vec![];
        for (k, v) in entries(props) {
            let mut t = self.ty(v, &format!("{name}{}", pascal(k)), results);
            if !req.contains(&k.as_str()) {
                t = format!("NotRequired[{t}]");
            }
            fields.push(format!("    {}: {t},", serde_json::to_string(k).unwrap()));
        }
        format!(
            "{name} = TypedDict(\"{name}\", {{\n{}\n}})",
            fields.join("\n")
        )
    }

    /// A named alias or TypedDict for a top-level schema.
    fn define(&mut self, name: &str, s: &Value, results: bool) {
        let n = self.unique(name);
        if s.get("type").and_then(Value::as_str) == Some("object") && s.get("properties").is_some()
        {
            let code = self.typed_dict(&n, s, &s["properties"], results);
            self.out.push((n, code));
        } else {
            let t = self.ty(s, &format!("{n}X"), results);
            // An explicit `TypeAlias`: a quoted forward reference (`X = "Y"`) would otherwise
            // be a plain string variable to type checkers.
            self.out.push((n.clone(), format!("{n}: TypeAlias = {t}")));
        }
    }
}

fn gen_py(b: &Value) -> (String, String) {
    let mut py = Py {
        out: vec![],
        names: BTreeSet::new(),
    };
    // Reserve names the module defines itself.
    for n in [
        "API_VERSION",
        "ERROR_KINDS",
        "METHODS",
        "EVENT_TYPES",
        "Any",
        "Dict",
        "List",
        "Literal",
        "NotRequired",
        "Optional",
        "TypedDict",
        "TypeAlias",
        "Union",
    ] {
        py.names.insert(n.into());
    }
    for (n, _) in entries(&b["$defs"]) {
        py.names.insert(py_def_name(n));
    }
    let mut defs = vec![];
    for (n, d) in entries(&b["$defs"]) {
        let name = py_def_name(n);
        py.names.remove(&name);
        let before = py.out.len();
        py.define(&name, d, true);
        defs.extend(py.out.drain(before..));
    }
    let mut methods = vec![];
    let mut method_tbl = String::new();
    let mut api = String::new();
    for (m, e) in entries(&b["x-methods"]) {
        let p = pascal(m);
        let (pn, rn) = (format!("{p}Params"), format!("{p}Result"));
        let before = py.out.len();
        py.define(&pn, &e["params"], false);
        py.define(&rn, &e["result"], true);
        methods.extend(py.out.drain(before..));
        let _ = writeln!(
            method_tbl,
            "    {}: {{\"mutating\": {}, \"scope\": {}, \"pane_scope\": {}}},",
            serde_json::to_string(m).unwrap(),
            if e["mutating"] == true {
                "True"
            } else {
                "False"
            },
            e["scope"],
            e["pane_scope"]
        );
        let all_optional = e["params"].get("type").and_then(Value::as_str) == Some("object")
            && required(&e["params"]).is_empty();
        let (sig, call) = if all_optional {
            (
                format!("params: \"Optional[t.{pn}]\" = None"),
                "params or {}".to_string(),
            )
        } else {
            (format!("params: \"t.{pn}\""), "params".to_string())
        };
        let _ = writeln!(
            api,
            "    async def {}(self, {sig}) -> \"t.{rn}\":\n        return await self.call({}, {call})  # type: ignore[arg-type, return-value]\n",
            m.replace('.', "_"),
            serde_json::to_string(m).unwrap()
        );
    }
    let mut events = vec![];
    let mut event_names = vec![];
    for (t, e) in entries(&b["x-events"]) {
        let p = pascal(t);
        let before = py.out.len();
        py.define(&format!("{p}Subject"), &e["subject"], true);
        py.define(&format!("{p}Data"), &e["data"], true);
        events.extend(py.out.drain(before..));
        event_names.push(serde_json::to_string(t).unwrap());
    }

    let mut o = String::new();
    let _ = writeln!(o, "# {GENERATED}");
    let _ = writeln!(
        o,
        "from __future__ import annotations\n\nfrom typing import Any, Dict, List, Literal, NotRequired, Optional, TypeAlias, TypedDict, Union\n"
    );
    let _ = writeln!(o, "API_VERSION = {}\n", b["x-api"]);
    let _ = writeln!(o, "ERROR_KINDS: Dict[str, Dict[str, Any]] = {{");
    for e in b["x-errors"].as_array().unwrap() {
        let _ = writeln!(
            o,
            "    {}: {{\"code\": {}, \"retryable\": {}}},",
            e["kind"],
            e["code"],
            if e["retryable"] == true {
                "True"
            } else {
                "False"
            }
        );
    }
    let _ = writeln!(o, "}}\n");
    let _ = writeln!(o, "# ---- shared types ----\n");
    for (_, c) in &defs {
        let _ = writeln!(o, "{c}\n");
    }
    let _ = writeln!(o, "# ---- methods ----\n");
    for (_, c) in &methods {
        let _ = writeln!(o, "{c}\n");
    }
    let _ = writeln!(
        o,
        "# Mutating flag and scope of every method (`full`: not callable with a pane token).\nMETHODS: Dict[str, Dict[str, Any]] = {{\n{method_tbl}}}\n"
    );
    let _ = writeln!(o, "# ---- events ----\n");
    for (_, c) in &events {
        let _ = writeln!(o, "{c}\n");
    }
    let _ = writeln!(
        o,
        "# Event types this client knows (clients ignore unknown types).\nEVENT_TYPES = (\n    {},\n)",
        event_names.join(",\n    ")
    );

    let mut a = String::new();
    let _ = writeln!(a, "# {GENERATED}");
    let _ = writeln!(
        a,
        "from __future__ import annotations\n\nfrom typing import Any, Dict, Optional\n\nfrom . import types_gen as t\n\n\nclass Api:\n    \"\"\"One typed coroutine per control-API method (`pane.split` -> `pane_split`).\"\"\"\n\n    async def call(\n        self, method: str, params: Optional[Dict[str, Any]] = None, *, timeout: Optional[float] = None\n    ) -> Any:\n        raise NotImplementedError\n"
    );
    a.push_str(&api);
    (o, a)
}

/// Every generated file: path (relative to the repo root) and contents.
fn generated() -> Vec<(&'static str, String)> {
    let b = vk_server::api_schema::bundle();
    let (types, api) = gen_py(&b);
    vec![
        ("clients/typescript/src/types.gen.ts", gen_ts(&b)),
        ("clients/python/vibeke_client/types_gen.py", types),
        ("clients/python/vibeke_client/api_gen.py", api),
    ]
}

#[test]
fn generated_clients_are_up_to_date() {
    let update = std::env::var_os("VIBEKE_UPDATE_CLIENTS").is_some();
    let mut stale = vec![];
    for (rel, want) in generated() {
        let path = root().join(rel);
        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(want.as_str()) {
            stale.push(rel);
        }
    }
    assert!(
        stale.is_empty(),
        "generated client files are stale: {stale:?}; run VIBEKE_UPDATE_CLIENTS=1 cargo test -p vibeke --test api_clients"
    );
}

#[test]
fn client_packages_are_complete_and_versioned_with_the_api() {
    for rel in [
        "clients/typescript/package.json",
        "clients/typescript/tsconfig.json",
        "clients/typescript/src/client.ts",
        "clients/typescript/src/index.ts",
        "clients/typescript/test/client.test.ts",
        "clients/typescript/examples/status.ts",
        "clients/typescript/examples/preview_tls.ts",
        "clients/python/pyproject.toml",
        "clients/python/vibeke_client/__init__.py",
        "clients/python/vibeke_client/client.py",
        "clients/python/tests/test_client.py",
        "clients/python/examples/status.py",
        "clients/python/examples/preview_tls.py",
    ] {
        assert!(root().join(rel).is_file(), "{rel} missing");
    }
    let pkg: Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("clients/typescript/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pkg["name"], "@vibeke/client");
    // The generated constants carry the API string the server speaks.
    let ts = std::fs::read_to_string(root().join("clients/typescript/src/types.gen.ts")).unwrap();
    assert!(ts.contains(&format!("API_VERSION = \"{}\"", vk_proto::API_VERSION)));
}

// ----------------------------------------------------------------------------------------------
// End to end
// ----------------------------------------------------------------------------------------------

fn have(cmd: &str, arg: &str) -> Option<String> {
    let out = Command::new(cmd).arg(arg).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// How to run a `.ts` file: Node >= 22.18 strips types natively; otherwise Bun.
fn ts_runner() -> Option<Vec<&'static str>> {
    if let Some(v) = have("node", "--version") {
        let n: Vec<u32> = v
            .trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect();
        if n.len() >= 2
            && (n[0] > 23 || (n[0] == 23 && n[1] >= 6) || (n[0] == 22 && n[1] >= 18) || n[0] > 22)
        {
            return Some(vec!["node"]);
        }
    }
    have("bun", "--version").map(|_| vec!["bun", "run"])
}

fn python() -> Option<&'static str> {
    // The client needs 3.11+ (typing.NotRequired).
    let v = have("python3", "--version")?;
    let minor: u32 = v.split('.').nth(1)?.parse().ok()?;
    (minor >= 11 && v.starts_with("Python 3.")).then_some("python3")
}

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkcli")
            .tempdir_in("/tmp")
            .unwrap();
        // An ephemeral proxy port (never the machine-wide default); the local CA of the TLS
        // example lives in this session's state dir.
        std::fs::write(
            dir.path().join("config.toml"),
            "[preview]\nproxy_port = 0\n",
        )
        .unwrap();
        Session { dir }
    }
    fn apply(&self, c: &mut Command) {
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_NO_OPEN", "1")
            .env("VIBEKE_SESSION", "default")
            .env("PYTHONDONTWRITEBYTECODE", "1");
        for k in ["VIBEKE", "VIBEKE_SOCKET", "VIBEKE_PANE_TOKEN"] {
            c.env_remove(k);
        }
    }
    fn vibeke(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        self.apply(&mut c);
        c.args(args);
        c
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self
            .vibeke(&["--json", "server", "stop", "--kill-panes"])
            .output();
    }
}

fn run_with_timeout(mut c: Command, secs: u64) -> std::process::Output {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(secs) {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "timed out after {secs}s\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn check_example_output(who: &str, out: std::process::Output) {
    assert!(
        out.status.success(),
        "{who} example failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or_default()
        .to_string();
    let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{who}: {e}: {line}"));
    assert_eq!(v["session"], "default", "{who}: server.status");
    assert!(v["pid"].as_u64().unwrap() > 0, "{who}: server.status pid");
    assert!(v["workspaces_before"].is_u64(), "{who}: workspace.list");
    // The workspace this client created arrived as a typed event on its own subscription.
    assert_eq!(v["event"]["type"], "workspace.created", "{who}: {v}");
    assert_eq!(
        v["event"]["subject"]["workspace"], v["created_workspace"],
        "{who}: event subject names the created workspace"
    );
}

/// The TLS example's output: a proxy open over https with the CA to trust, and `proxy_url`
/// null before the origin existed.
fn check_tls_example_output(who: &str, out: std::process::Output, state: &Path) {
    assert!(
        out.status.success(),
        "{who} TLS example failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or_default()
        .to_string();
    let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{who}: {e}: {line}"));
    assert_eq!(v["opened_in"], "proxy", "{who}: {v}");
    assert_eq!(v["tls_origin"], true, "{who}: {v}");
    assert_eq!(v["https"], true, "{who}: {v}");
    assert_eq!(v["has_open_url"], true, "{who}: {v}");
    assert_eq!(v["proxy_url_before"], Value::Null, "{who}: {v}");
    assert!(v["session_ttl_s"].as_u64().unwrap() > 0, "{who}: {v}");
    assert_eq!(
        v["ca_sha256"].as_str().unwrap().split(':').count(),
        32,
        "{who}: {v}"
    );
    assert!(
        Path::new(v["ca_path"].as_str().unwrap()).starts_with(state),
        "{who}: the CA lives in the session's state dir: {v}"
    );
}

#[test]
fn typescript_and_python_clients_talk_to_a_real_server() {
    let ts = ts_runner();
    let py = python();
    if ts.is_none() && py.is_none() {
        eprintln!("skipped: neither node (>= 22.18) / bun nor python3 (>= 3.11) is available");
        return;
    }
    let s = Session::new();
    let out = s.vibeke(&["--json", "server", "status"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if let Some(runner) = ts {
        let mut c = Command::new(runner[0]);
        c.args(&runner[1..])
            .arg("examples/status.ts")
            .current_dir(root().join("clients/typescript"));
        s.apply(&mut c);
        check_example_output("typescript", run_with_timeout(c, 60));
        let app = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = Command::new(runner[0]);
        c.args(&runner[1..])
            .arg("examples/preview_tls.ts")
            .arg(app.local_addr().unwrap().port().to_string())
            .current_dir(root().join("clients/typescript"));
        s.apply(&mut c);
        check_tls_example_output(
            "typescript",
            run_with_timeout(c, 60),
            &s.dir.path().join("state"),
        );
    } else {
        eprintln!("skipped typescript: need node >= 22.18 or bun");
    }
    if let Some(py) = py {
        let mut c = Command::new(py);
        c.arg("examples/status.py")
            .current_dir(root().join("clients/python"));
        s.apply(&mut c);
        check_example_output("python", run_with_timeout(c, 60));
        let app = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = Command::new(py);
        c.arg("examples/preview_tls.py")
            .arg(app.local_addr().unwrap().port().to_string())
            .current_dir(root().join("clients/python"));
        s.apply(&mut c);
        check_tls_example_output(
            "python",
            run_with_timeout(c, 60),
            &s.dir.path().join("state"),
        );
    } else {
        eprintln!("skipped python: need python3 >= 3.11");
    }
}

#[test]
fn typescript_client_unit_tests_pass() {
    let Some(runner) = ts_runner() else {
        eprintln!("skipped: need node >= 22.18 or bun");
        return;
    };
    let mut c = if runner[0] == "node" {
        let mut c = Command::new("node");
        c.args(["--test", "test/*.test.ts"]);
        c
    } else {
        let mut c = Command::new("bun");
        c.args(["test", "test"]);
        c
    };
    c.current_dir(root().join("clients/typescript"));
    let out = run_with_timeout(c, 120);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn python_client_unit_tests_pass() {
    let Some(py) = python() else {
        eprintln!("skipped: need python3 >= 3.11");
        return;
    };
    let mut c = Command::new(py);
    c.args(["-m", "unittest", "discover", "-s", "tests"])
        .current_dir(root().join("clients/python"))
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let out = run_with_timeout(c, 120);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `tsc` for the typecheck test: the package's own `node_modules/.bin/tsc` (after `npm install`
/// in `clients/typescript`), else one on `PATH`.
fn tsc() -> Option<PathBuf> {
    let local = root().join("clients/typescript/node_modules/.bin/tsc");
    if local.is_file() {
        return Some(local);
    }
    have("tsc", "--version").map(|_| PathBuf::from("tsc"))
}

/// The TypeScript client, its tests and examples typecheck (`tsc --noEmit`), including the
/// typed TLS `preview.open` and `task.finish` examples and their `@ts-expect-error` lines (a
/// mistake the generated types no longer reject fails the check). Skipped without `tsc`.
#[test]
fn typescript_client_and_examples_typecheck() {
    let Some(tsc) = tsc() else {
        eprintln!("skipped: no tsc (npm install in clients/typescript, or tsc on PATH)");
        return;
    };
    let mut c = Command::new(tsc);
    c.args(["--noEmit", "-p", "tsconfig.json"])
        .current_dir(root().join("clients/typescript"));
    let out = run_with_timeout(c, 180);
    assert!(
        out.status.success(),
        "tsc:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The Python client and its examples typecheck with mypy; in the TLS example every
/// deliberate mistake must still be an error (`warn_unused_ignores`). Skipped without mypy.
#[test]
fn python_client_and_examples_typecheck() {
    let Some(mypy) = have("mypy", "--version").map(|_| "mypy") else {
        eprintln!("skipped: mypy not on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("mypy.ini");
    std::fs::write(
        &ini,
        "[mypy]\n\n[mypy-preview_tls]\nwarn_unused_ignores = True\n",
    )
    .unwrap();
    let mut c = Command::new(mypy);
    c.arg("--config-file")
        .arg(&ini)
        .arg("--cache-dir")
        .arg(dir.path().join("cache"))
        .args(["vibeke_client", "examples"])
        .current_dir(root().join("clients/python"));
    let out = run_with_timeout(c, 180);
    assert!(
        out.status.success(),
        "mypy:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn debug_api_schema_prints_the_bundle() {
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let out = Command::new(bin)
        .args(["debug", "api-schema"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed, vk_server::api_schema::bundle());
    assert_eq!(
        printed["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    // The checked-in copy is what the binary prints.
    let file = std::fs::read_to_string(root().join("docs/api/vibeke-1.schema.json")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&file).unwrap(), printed);
    // --out writes the same text; --method narrows.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");
    let st = Command::new(bin)
        .args(["debug", "api-schema", "--out"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(st.success());
    assert_eq!(std::fs::read(&path).unwrap(), out.stdout);
    let one = Command::new(bin)
        .args(["debug", "api-schema", "--method", "pane.split"])
        .output()
        .unwrap();
    let one: Value = serde_json::from_slice(&one.stdout).unwrap();
    assert_eq!(one["method"], "pane.split");
    assert!(one["x-method"]["params"]["properties"]["direction"].is_object());
    let bad = Command::new(bin)
        .args(["debug", "api-schema", "--method", "no.such"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
}
