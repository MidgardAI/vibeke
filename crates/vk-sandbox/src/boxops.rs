//! Container box operations beyond the create/exec/stop lifecycle in `container.rs` (13 §9,
//! §11): pause/unpause for idle suspend, resource sampling for `sandbox.resource_pressure`,
//! template commits, listing Vibeke's boxes for `sandbox prune`, the debugging shell, logs and
//! listening-port discovery for box previews. All of it is docker-CLI shaped (Docker, OrbStack,
//! Podman); Apple `container` lacks pause and commit, which are reported as unsupported.

use crate::container::{
    BoxSpec, BoxState, CmdOut, ContainerBox, ExecOpts, Limits, Provider, run_cmd,
};
use crate::runner::RunnerError;
use std::time::Duration;

impl BoxSpec {
    /// Pause/unpause and commit exist on the docker-compatible runtimes only.
    pub fn supports_pause(&self) -> bool {
        !self.provider.apple()
    }
    pub fn pause_argv(&self) -> Vec<String> {
        vec![self.provider.s(), "pause".into(), self.name.clone()]
    }
    pub fn unpause_argv(&self) -> Vec<String> {
        vec![self.provider.s(), "unpause".into(), self.name.clone()]
    }
    pub fn stats_argv(&self) -> Vec<String> {
        vec![
            self.provider.s(),
            "stats".into(),
            "--no-stream".into(),
            "--format".into(),
            "{{json .}}".into(),
            self.name.clone(),
        ]
    }
    /// `commit` into a template image (13 §9), labelled so `sandbox prune` can find it.
    pub fn commit_argv(&self, tag: &str) -> Vec<String> {
        vec![
            self.provider.s(),
            "commit".into(),
            "--change".into(),
            "LABEL vibeke.template=1".into(),
            self.name.clone(),
            tag.into(),
        ]
    }
    pub fn image_exists_argv(&self, tag: &str) -> Vec<String> {
        vec![
            self.provider.s(),
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            tag.into(),
        ]
    }
    pub fn rename_argv(&self, new: &str) -> Vec<String> {
        vec![
            self.provider.s(),
            "rename".into(),
            self.name.clone(),
            new.into(),
        ]
    }
    /// `vibeke sandbox shell`: an interactive login shell in the box's workdir. No secrets are
    /// passed (a debugging shell is not an agent).
    pub fn shell_argv(&self, shell: &str, env: &[(String, String)]) -> Vec<String> {
        self.exec_argv(
            &ExecOpts {
                tty: true,
                interactive: true,
                workdir: Some(self.workdir.clone()),
                env: env.to_vec(),
                ..Default::default()
            },
            &[shell.to_string(), "-l".into()],
        )
    }
    /// Listening TCP sockets inside the box (`/proc/net/tcp*` of its network namespace).
    pub fn ports_argv(&self) -> Vec<String> {
        self.exec_argv(
            &ExecOpts::default(),
            &[
                "/bin/sh".into(),
                "-c".into(),
                "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null".into(),
            ],
        )
    }
}

/// `<runtime> ps -a` restricted to Vibeke's boxes, one tab-separated line per box.
pub fn list_argv(provider: &Provider) -> Vec<String> {
    vec![
        provider.s(),
        "ps".into(),
        "--all".into(),
        "--filter".into(),
        "label=vibeke.box=1".into(),
        "--format".into(),
        "{{.Names}}\t{{.Label \"vibeke.key\"}}\t{{.Label \"vibeke.session\"}}\t{{.State}}".into(),
    ]
}

/// One box as `<runtime> ps` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedBox {
    pub name: String,
    pub key: String,
    pub session: String,
    pub state: String,
}

pub fn parse_list(out: &str) -> Vec<ListedBox> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let name = f.next()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(ListedBox {
                name,
                key: f.next().unwrap_or("").trim().to_string(),
                session: f.next().unwrap_or("").trim().to_string(),
                state: f.next().unwrap_or("").trim().to_string(),
            })
        })
        .collect()
}

/// Every Vibeke box the runtime knows (blocking).
pub fn list_boxes(
    provider: &Provider,
    cli_env: &[(String, String)],
) -> std::io::Result<Vec<ListedBox>> {
    if provider.apple() {
        return Err(std::io::Error::other(
            "listing boxes is not supported for Apple container",
        ));
    }
    let o = run_cmd(&list_argv(provider), cli_env, None, Duration::from_secs(30))?;
    if !o.ok {
        return Err(std::io::Error::other(o.stderr.trim().to_string()));
    }
    Ok(parse_list(&o.stdout))
}

/// One resource sample of a box (`<runtime> stats --no-stream`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxStats {
    pub cpu_percent: f64,
    pub mem_bytes: u64,
    /// The runtime's memory limit (the VM/host size when the box has none).
    pub mem_limit: u64,
    pub pids: u64,
}

/// `12.5MiB`, `1.2GB`, `512kB`, `100B` → bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "k" => 1e3,
        "mb" | "m" => 1e6,
        "gb" | "g" => 1e9,
        "tb" | "t" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// One `{{json .}}` line of `docker stats` (also Podman's docker-compatible output).
pub fn parse_stats(line: &str) -> Option<BoxStats> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let cpu = get("CPUPerc")
        .trim_end_matches('%')
        .trim()
        .parse()
        .unwrap_or(0.0);
    let (used, limit) = get("MemUsage").split_once('/').unwrap_or(("", ""));
    let pids = match v.get("PIDs") {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    Some(BoxStats {
        cpu_percent: cpu,
        mem_bytes: parse_size(used).unwrap_or(0),
        mem_limit: parse_size(limit).unwrap_or(0),
        pids,
    })
}

/// A resource above the pressure threshold (13 §11).
#[derive(Debug, Clone, PartialEq)]
pub struct Pressure {
    pub resource: &'static str,
    pub value: f64,
    pub limit: f64,
    pub share: f64,
}

/// Resources of `s` at or above `threshold` (share of the box's configured limit). Only limits
/// the box actually has count: memory and cpus when configured, pids always (default 1024).
pub fn pressure(s: &BoxStats, limits: &Limits, threshold: f64) -> Vec<Pressure> {
    let mut out = Vec::new();
    let mut push = |resource, value: f64, limit: f64| {
        if limit > 0.0 {
            let share = value / limit;
            if share >= threshold {
                out.push(Pressure {
                    resource,
                    value,
                    limit,
                    share,
                });
            }
        }
    };
    if let Some(m) = limits.memory.as_deref().and_then(parse_size) {
        let lim = if s.mem_limit > 0 {
            s.mem_limit.min(m)
        } else {
            m
        };
        push("memory", s.mem_bytes as f64, lim as f64);
    }
    push("pids", s.pids as f64, limits.pids.unwrap_or(1024) as f64);
    if let Some(c) = limits.cpus.as_deref().and_then(|c| c.parse::<f64>().ok()) {
        push("cpus", s.cpu_percent / 100.0, c);
    }
    out
}

/// Listening ports (state `0A`) on a wildcard or loopback address, from `/proc/net/tcp` and
/// `/proc/net/tcp6` text. Sorted, deduplicated.
pub fn parse_proc_net_tcp(text: &str) -> Vec<u16> {
    let mut v: Vec<u16> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 || f[3] != "0A" {
                return None;
            }
            let (addr, port) = f[1].rsplit_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let local = matches!(
                addr,
                // 0.0.0.0, 127.0.0.1 (little-endian hex), ::, ::1, ::ffff:127.0.0.1
                "00000000"
                    | "0100007F"
                    | "00000000000000000000000000000000"
                    | "00000000000000000000000001000000"
                    | "0000000000000000FFFF00000100007F"
            );
            local.then_some(port)
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Content key of a template (13 §9): image, provider and the setup steps that would run.
pub fn template_key(provider: &str, image: &str, steps: &[(String, String)]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(provider.as_bytes());
    h.update(b"\0");
    h.update(image.as_bytes());
    for (n, s) in steps {
        h.update(b"\0");
        h.update(n.as_bytes());
        h.update(b"\x01");
        h.update(s.as_bytes());
    }
    h.finalize().to_hex()[..12].to_string()
}

/// The image tag of a template.
pub fn template_tag(key: &str) -> String {
    format!("vibeke-template:{key}")
}

impl ContainerBox {
    fn run(&self, argv: &[String], secs: u64) -> std::io::Result<CmdOut> {
        run_cmd(argv, &self.cli_env, None, Duration::from_secs(secs))
    }
    /// Freeze a running box (idle suspend). No-op unless it runs.
    pub fn pause(&self) -> Result<(), RunnerError> {
        if !self.spec.supports_pause() {
            return Err(RunnerError::Unsupported(
                "pause is not supported for Apple container".into(),
            ));
        }
        if self.state() == BoxState::Running {
            self.cli(&self.spec.pause_argv(), Duration::from_secs(60))?;
        }
        Ok(())
    }
    /// Thaw a paused box. No-op unless it is paused.
    pub fn unpause(&self) -> Result<(), RunnerError> {
        if self.state() == BoxState::Paused {
            self.cli(&self.spec.unpause_argv(), Duration::from_secs(60))?;
        }
        Ok(())
    }
    pub fn stats(&self) -> Option<BoxStats> {
        let o = self.run(&self.spec.stats_argv(), 20).ok()?;
        if !o.ok {
            return None;
        }
        o.stdout.lines().find_map(parse_stats)
    }
    pub fn commit(&self, tag: &str) -> Result<(), RunnerError> {
        if !self.spec.supports_pause() {
            return Err(RunnerError::Unsupported(
                "template commits are not supported for Apple container".into(),
            ));
        }
        self.cli(&self.spec.commit_argv(tag), Duration::from_secs(600))?;
        Ok(())
    }
    pub fn image_exists(&self, tag: &str) -> bool {
        self.run(&self.spec.image_exists_argv(tag), 20)
            .is_ok_and(|o| o.ok && !o.stdout.trim().is_empty())
    }
    pub fn listening_ports(&self) -> Vec<u16> {
        match self.run(&self.spec.ports_argv(), 15) {
            Ok(o) if o.ok => parse_proc_net_tcp(&o.stdout),
            _ => vec![],
        }
    }
    /// The last `tail` lines of the box's own output (PID 1).
    pub fn logs(&self, tail: u32) -> String {
        match self.run(&self.spec.logs_argv(tail), 20) {
            Ok(o) => format!("{}{}", o.stdout, o.stderr),
            Err(e) => format!("(logs unavailable: {e})"),
        }
    }
    pub fn rename(&self, new: &str) -> Result<(), RunnerError> {
        self.cli(&self.spec.rename_argv(new), Duration::from_secs(30))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{BOX_WORKSPACE, BoxNet};
    use std::path::PathBuf;

    fn spec(p: Provider) -> BoxSpec {
        BoxSpec {
            provider: p,
            name: "vk-abc".into(),
            image: "alpine:3.20".into(),
            labels: vec![],
            net: BoxNet::None,
            workdir: BOX_WORKSPACE.into(),
            mounts: vec![],
            env: vec![],
            user: None,
            limits: Limits::default(),
            in_box_vibeke: false,
            cap_add: vec![],
        }
    }

    #[test]
    fn argv_shapes() {
        let s = spec(Provider::Docker(PathBuf::from("/usr/bin/docker")));
        assert_eq!(s.pause_argv(), ["/usr/bin/docker", "pause", "vk-abc"]);
        assert_eq!(s.unpause_argv(), ["/usr/bin/docker", "unpause", "vk-abc"]);
        assert_eq!(
            s.commit_argv("vibeke-template:abc")[1..],
            [
                "commit",
                "--change",
                "LABEL vibeke.template=1",
                "vk-abc",
                "vibeke-template:abc"
            ]
        );
        let sh = s.shell_argv("/bin/sh", &[("TERM".into(), "xterm".into())]);
        assert!(sh.contains(&"--tty".to_string()) && sh.contains(&"--interactive".to_string()));
        assert_eq!(&sh[sh.len() - 3..], ["vk-abc", "/bin/sh", "-l"]);
        assert!(
            sh.windows(2)
                .any(|w| w[0] == "--env" && w[1] == "TERM=xterm")
        );
        let l = list_argv(&Provider::Podman(PathBuf::from("/usr/bin/podman")));
        assert!(l.contains(&"label=vibeke.box=1".to_string()));
        assert!(s.supports_pause());
        assert!(
            !spec(Provider::AppleContainer(PathBuf::from(
                "/usr/local/bin/container"
            )))
            .supports_pause()
        );
    }

    #[test]
    fn list_and_stats_parsing() {
        let out = "vk-1\tT1\tmain\trunning\nvk-2\tpane:P\tother\texited\n\n";
        let l = parse_list(out);
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].key, "pane:P");
        assert_eq!(l[1].session, "other");
        let st = parse_stats(
            r#"{"CPUPerc":"183.20%","MemUsage":"900MiB / 1GiB","PIDs":"1000","Name":"vk-1"}"#,
        )
        .unwrap();
        assert!((st.cpu_percent - 183.2).abs() < 1e-9);
        assert_eq!(st.mem_bytes, 900 * 1024 * 1024);
        assert_eq!(st.mem_limit, 1024 * 1024 * 1024);
        assert_eq!(st.pids, 1000);
        assert_eq!(parse_size("1.5kB"), Some(1500));
        assert_eq!(parse_size("12x"), None);
        assert!(parse_stats("not json").is_none());
    }

    #[test]
    fn pressure_uses_configured_limits() {
        let st = BoxStats {
            cpu_percent: 190.0,
            mem_bytes: 950 * 1024 * 1024,
            mem_limit: 8 * 1024 * 1024 * 1024,
            pids: 10,
        };
        // No memory/cpu limits configured: only pids count, and 10/1024 is fine.
        assert!(pressure(&st, &Limits::default(), 0.9).is_empty());
        let lim = Limits {
            cpus: Some("2".into()),
            memory: Some("1GiB".into()),
            pids: Some(11),
        };
        let p = pressure(&st, &lim, 0.9);
        let names: Vec<&str> = p.iter().map(|x| x.resource).collect();
        assert_eq!(names, ["memory", "pids", "cpus"]);
        assert!(p[0].share > 0.9 && p[0].share < 1.0);
    }

    #[test]
    fn proc_net_tcp_listeners() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2 1 0000000000000000 100 0 0 10 0
   2: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 3 1 0000000000000000 100 0 0 10 0
   3: 0200000A:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4 1 0000000000000000 100 0 0 10 0
   0: 00000000000000000000000000000000:1389 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5 1 0000000000000000 100 0 0 10 0
";
        // 3000 and 8080 (v4 wildcard / loopback) and 5001 (v6 wildcard); an established
        // connection and a listener on another interface are not previews.
        assert_eq!(parse_proc_net_tcp(text), [3000, 5001, 8080]);
    }

    #[test]
    fn template_keys_follow_inputs() {
        let a = template_key("docker", "node:22", &[("setup".into(), "npm ci".into())]);
        let b = template_key("docker", "node:22", &[("setup".into(), "npm i".into())]);
        assert_ne!(a, b);
        assert_eq!(a.len(), 12);
        assert_eq!(template_tag(&a), format!("vibeke-template:{a}"));
    }
}
