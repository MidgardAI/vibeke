//! `vibeke preview trust-ca` (06 B4 `tls_origin`): where the local preview CA lives and how the
//! user trusts it.
//!
//! Vibeke never changes a trust store by itself. The default is to print the CA path, its
//! SHA-256 fingerprint and exact per-OS instructions. `--install` (macOS only) runs
//! `security add-trusted-cert` for the user's login keychain, and only after the user typed the
//! first 8 hex digits of the fingerprint at an interactive prompt. The installer is injected so
//! tests never run it.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use vk_preview::ca::{LocalCa, trust_instructions};

pub const USAGE: &str = "usage: vibeke preview trust-ca [--install] [--path]\n  \
    (no flag)  print the CA file, its SHA-256 fingerprint and per-OS trust instructions\n  \
    --path     print only the CA file path (generating the CA on first use)\n  \
    --install  macOS: add the CA to your login keychain after a typed confirmation\n";

/// `<state>/tls`: `0700`, holds the CA certificate and its `0600` key.
pub fn ca_dir() -> PathBuf {
    crate::paths::state_root().join("tls")
}

/// The CA of this user, generated on first use.
pub fn load() -> Result<std::sync::Arc<LocalCa>, vk_preview::ca::CaError> {
    LocalCa::load_or_create(&ca_dir())
}

/// The CA as the long-running proxy holds it (reloaded when another process renews it).
pub fn store() -> Result<std::sync::Arc<vk_preview::ca::CaStore>, vk_preview::ca::CaError> {
    vk_preview::ca::CaStore::open(&ca_dir())
}

/// Adds the CA file to the login keychain; `Ok(true)` when the system accepted it.
pub fn macos_install(pem: &Path) -> std::io::Result<bool> {
    let keychain = crate::paths::home().join("Library/Keychains/login.keychain-db");
    Ok(std::process::Command::new("security")
        .args(["add-trusted-cert", "-r", "trustRoot", "-k"])
        .arg(keychain)
        .arg(pem)
        .status()?
        .success())
}

/// The command as run from the CLI (real terminal, real installer).
pub fn run(args: &[String]) -> i32 {
    use std::io::IsTerminal;
    let stdin = std::io::stdin();
    run_with(
        args,
        &ca_dir(),
        cfg!(target_os = "macos"),
        stdin.is_terminal(),
        &mut stdin.lock(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &mut macos_install,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_with(
    args: &[String],
    dir: &Path,
    macos: bool,
    interactive: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
    install: &mut dyn FnMut(&Path) -> std::io::Result<bool>,
) -> i32 {
    let (mut want_install, mut path_only) = (false, false);
    for a in args {
        match a.as_str() {
            "--install" => want_install = true,
            "--path" => path_only = true,
            _ => {
                let _ = write!(err, "unknown argument `{a}`\n{USAGE}");
                return 2;
            }
        }
    }
    let ca = match LocalCa::load_or_create(dir) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return 1;
        }
    };
    let fp = ca.fingerprint_sha256();
    if path_only {
        let _ = writeln!(out, "{}", ca.ca_path().display());
        return 0;
    }
    let _ = write!(out, "{}", trust_instructions(&ca.ca_path(), &fp));
    let _ = writeln!(
        out,
        "SPKI SHA-256 (base64, for Chromium's --ignore-certificate-errors-spki-list in a throwaway profile): {}",
        ca.spki_sha256_base64()
    );
    if !want_install {
        return 0;
    }
    if !macos {
        let _ = writeln!(
            err,
            "\n--install is only available on macOS; use the instructions above for this system."
        );
        return 2;
    }
    if !interactive {
        let _ = writeln!(
            err,
            "\n--install needs an interactive terminal (it asks you to type a confirmation)."
        );
        return 2;
    }
    let code: String = fp
        .split(':')
        .take(4)
        .collect::<String>()
        .to_ascii_lowercase();
    let _ = write!(
        out,
        "\nThis adds the CA to your login keychain as a trusted root (limited by its name\n\
         constraint to *.vibeke.localhost). macOS will ask you to authorise the change.\n\
         To continue, type the first 8 characters of the SHA-256 fingerprint ({code}); anything else cancels: "
    );
    let _ = out.flush();
    let mut line = String::new();
    let _ = input.read_line(&mut line);
    if !line.trim().eq_ignore_ascii_case(&code) {
        let _ = writeln!(err, "cancelled; nothing was changed.");
        return 1;
    }
    match install(&ca.ca_path()) {
        Ok(true) => {
            let _ = writeln!(
                out,
                "trusted. Remove it later with the command shown above."
            );
            0
        }
        Ok(false) => {
            let _ = writeln!(
                err,
                "the system refused to trust the CA; nothing was changed."
            );
            1
        }
        Err(e) => {
            let _ = writeln!(err, "could not run `security`: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run {
        code: i32,
        out: String,
        err: String,
        installs: Vec<PathBuf>,
    }

    fn go(dir: &Path, args: &[&str], macos: bool, tty: bool, typed: &str) -> Run {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut installs = Vec::new();
        let mut input = std::io::Cursor::new(typed.as_bytes().to_vec());
        let code = run_with(
            &args,
            dir,
            macos,
            tty,
            &mut input,
            &mut out,
            &mut err,
            &mut |p| {
                installs.push(p.to_path_buf());
                Ok(true)
            },
        );
        Run {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            installs,
        }
    }

    #[test]
    fn prints_instructions_and_never_installs_by_default() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("tls");
        let r = go(&dir, &[], true, true, "anything\n");
        assert_eq!(r.code, 0, "{}", r.err);
        assert!(r.installs.is_empty());
        let ca = LocalCa::load_or_create(&dir).unwrap();
        assert!(r.out.contains(&ca.fingerprint_sha256()), "{}", r.out);
        assert!(r.out.contains(&ca.ca_path().display().to_string()));
        for os in ["macOS", "Debian/Ubuntu", "Fedora/RHEL", "Firefox"] {
            assert!(r.out.contains(os), "{os}");
        }
        let r = go(&dir, &["--path"], false, false, "");
        assert_eq!(r.out.trim(), ca.ca_path().display().to_string());
        assert!(r.installs.is_empty());
        let r = go(&dir, &["--bogus"], true, true, "");
        assert_eq!(r.code, 2);
    }

    #[test]
    fn install_needs_macos_a_terminal_and_the_typed_confirmation() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("tls");
        let ca = LocalCa::load_or_create(&dir).unwrap();
        let code: String = ca
            .fingerprint_sha256()
            .split(':')
            .take(4)
            .collect::<String>()
            .to_ascii_lowercase();
        // Not macOS, or not interactive: refused, the installer is not reached.
        let r = go(&dir, &["--install"], false, true, &format!("{code}\n"));
        assert_eq!((r.code, r.installs.len()), (2, 0), "{}", r.err);
        let r = go(&dir, &["--install"], true, false, &format!("{code}\n"));
        assert_eq!((r.code, r.installs.len()), (2, 0), "{}", r.err);
        // A wrong or empty answer cancels.
        for typed in ["\n", "yes\n", "y\n", ""] {
            let r = go(&dir, &["--install"], true, true, typed);
            assert_eq!((r.code, r.installs.len()), (1, 0), "{typed:?}: {}", r.err);
            assert!(r.err.contains("cancelled"));
        }
        // The right confirmation runs the (injected) installer once on the CA file.
        let r = go(&dir, &["--install"], true, true, &format!("{code}\n"));
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(r.installs, vec![ca.ca_path()]);
    }

    #[test]
    fn this_module_is_the_only_place_that_spawns_a_trust_command() {
        // The server's proxy wiring never runs `security`: only `macos_install` does, and only
        // `run` (CLI) reaches it.
        for (name, src) in [
            ("preview_fabric.rs", include_str!("preview_fabric.rs")),
            ("preview.rs", include_str!("preview.rs")),
        ] {
            assert!(
                !src.contains("add-trusted-cert") && !src.contains("update-ca-certificates"),
                "{name}"
            );
        }
    }
}
