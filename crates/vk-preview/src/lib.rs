//! Preview fabric primitives (06 Part B): listener discovery via socket ownership, output URL
//! and banner scanning, HTTP probes, the preview lifecycle, the SOCKS5 listener protocol with
//! pluggable peer checks and routing, and the managed browser (profiles, discovery, launch).
//!
//! The server-side wiring (state, API, routing over bridge links) lives in
//! `vk-server::preview`.

pub mod browser;
pub mod lifecycle;
pub mod probe;
pub mod scan;
pub mod sockets;
pub mod socks;

/// Shells: a pane whose foreground process is one of these gets no listener scans (06 B2).
pub fn is_shell(argv0: &str) -> bool {
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let base = base.trim_start_matches('-');
    matches!(
        base,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "nu"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "login"
    )
}

/// Agent harness processes (06 B2): their own loopback listeners (Codex app-server, the Claude
/// Code IDE bridge websocket, …) are not dev servers and are never suggested as previews. Only
/// the harness process itself matches — a dev server it starts (`pnpm dev`) is a child with
/// its own argv and is still discovered.
pub fn is_harness_process(argv: &[String]) -> bool {
    let Some(a0) = argv.first() else {
        return false;
    };
    let base = |s: &str| {
        s.rsplit('/')
            .next()
            .unwrap_or(s)
            .trim_start_matches('-')
            .to_ascii_lowercase()
    };
    const HARNESSES: &[&str] = &[
        "claude",
        "claude-code",
        "codex",
        "codex-app-server",
        "pi",
        "omp",
        "opencode",
        "gemini",
        "cursor-agent",
        "amp",
    ];
    let b0 = base(a0);
    if HARNESSES.contains(&b0.as_str()) {
        return true;
    }
    // `node …/claude-code/cli.js`, `bun …/@openai/codex/…`, `node …/bin/codex`.
    if matches!(b0.as_str(), "node" | "bun" | "deno" | "nodejs") {
        return argv.iter().skip(1).take(4).any(|a| {
            let l = a.to_ascii_lowercase();
            l.contains("claude-code")
                || l.contains("@anthropic-ai/claude")
                || l.contains("@openai/codex")
                || l.contains("opencode")
                || l.contains("gemini-cli")
                || l.contains("@mariozechner/pi")
                || HARNESSES.contains(&base(&l).trim_end_matches(".js"))
        });
    }
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn harnesses() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for h in [
            v(&["claude"]),
            v(&["/opt/homebrew/bin/codex", "app-server"]),
            v(&[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            ]),
            v(&["bun", "/x/node_modules/@openai/codex/bin/codex.js"]),
            v(&["node", "/usr/local/bin/codex"]),
            v(&["opencode"]),
            v(&["gemini"]),
            v(&["omp"]),
        ] {
            assert!(super::is_harness_process(&h), "{h:?}");
        }
        for not in [
            v(&["node", "/app/node_modules/.bin/vite"]),
            v(&["pnpm", "dev"]),
            v(&["python3", "-m", "http.server"]),
            v(&[]),
        ] {
            assert!(!super::is_harness_process(&not), "{not:?}");
        }
    }

    #[test]
    fn shells() {
        assert!(super::is_shell("-zsh"));
        assert!(super::is_shell("/bin/bash"));
        assert!(!super::is_shell("node"));
        assert!(!super::is_shell("python3"));
    }
}
