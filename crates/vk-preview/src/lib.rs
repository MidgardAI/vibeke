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

#[cfg(test)]
mod tests {
    #[test]
    fn shells() {
        assert!(super::is_shell("-zsh"));
        assert!(super::is_shell("/bin/bash"));
        assert!(!super::is_shell("node"));
        assert!(!super::is_shell("python3"));
    }
}
