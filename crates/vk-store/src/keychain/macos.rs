//! Select the user's keychains explicitly. In an SSH login, the implicit domain can
//! point at System.keychain even when the user has a default login keychain.

use super::{Command, KeychainError, Path, R, run, run_output};

const SECURITY: &str = "/usr/bin/security";

pub(super) fn os_get(service: &str, account: Option<&str>) -> R<Option<String>> {
    Backend(Path::new(SECURITY)).get(service, account)
}

pub(super) fn os_set(service: &str, account: &str, secret: &str, _pair: bool) -> R<()> {
    Backend(Path::new(SECURITY)).set(service, account, secret)
}

pub(super) fn os_delete(service: &str, account: Option<&str>) -> R<bool> {
    Backend(Path::new(SECURITY)).delete(service, account)
}

struct Backend<'a>(&'a Path);

impl Backend<'_> {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(self.0);
        c.args(args);
        c
    }

    fn keychains(&self, default: bool) -> R<Vec<String>> {
        let verb = if default {
            "default-keychain"
        } else {
            "list-keychains"
        };
        let (code, out) = run_output(self.command(&[verb, "-d", "user"]), None)?;
        if code != 0 {
            return Err(failure(code));
        }
        let paths: Vec<_> = out
            .lines()
            .filter_map(|line| {
                let path = line.trim().strip_prefix('"')?.strip_suffix('"')?;
                (!path.is_empty() && Path::new(path).is_absolute()).then(|| path.to_string())
            })
            .collect();
        if paths.is_empty() || (default && paths.len() != 1) {
            return Err(KeychainError::Unsupported(
                "no user keychain is configured on this host. Open Keychain Access on the host to create or select one".into(),
            ));
        }
        Ok(paths)
    }

    fn lookup(&self, verb: &str, service: &str, account: Option<&str>) -> R<Command> {
        let mut c = self.command(&[verb, "-s", service]);
        if let Some(a) = account {
            c.args(["-a", a]);
        }
        if verb == "find-generic-password" {
            c.arg("-w");
        }
        // Read the write destination first, even if it is absent from the search list.
        let mut paths = self.keychains(true)?;
        for path in self.keychains(false)? {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        c.args(paths);
        Ok(c)
    }

    fn get(&self, service: &str, account: Option<&str>) -> R<Option<String>> {
        match run(
            self.lookup("find-generic-password", service, account)?,
            None,
        )? {
            (0, out) => Ok(Some(out)),
            (44, _) => Ok(None), // errSecItemNotFound
            (code, _) => Err(failure(code)),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> R<()> {
        // stdin keeps the secret out of argv. The interactive parser is not a shell.
        if secret.contains(['"', '\\']) {
            return Err(KeychainError::Unsupported(
                "the macOS backend cannot store a secret containing quotes or backslashes".into(),
            ));
        }
        let path = self.keychains(true)?.remove(0);
        if path
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\'))
        {
            return Err(KeychainError::Failed(
                "the user keychain path cannot be passed to the macOS keychain tool".into(),
            ));
        }
        let line = format!(
            "add-generic-password -U -s \"{service}\" -a \"{account}\" -w \"{secret}\" \"{path}\"\n"
        );
        // security -i splits longer lines into commands. Refuse before it can store
        // a truncated token or interpret part of the token as another command.
        if line.len() >= 4096 {
            return Err(KeychainError::Unsupported(
                "the token is too long for the macOS keychain tool".into(),
            ));
        }
        match run(self.command(&["-i"]), Some(&line))? {
            (0, _) => Ok(()),
            (code, _) => Err(failure(code)),
        }
    }

    fn delete(&self, service: &str, account: Option<&str>) -> R<bool> {
        match run(
            self.lookup("delete-generic-password", service, account)?,
            None,
        )? {
            (0, _) => Ok(true),
            (44, _) => Ok(false),
            (code, _) => Err(failure(code)),
        }
    }
}

fn failure(code: i32) -> KeychainError {
    // security returns OSStatus modulo 256. Never copy stderr: its interactive
    // parser can echo input, including a secret, when a command fails.
    let message = match code {
        36 => {
            "the user keychain needs unlocking or approval on this host. Unlock it in Keychain Access on the host, then try again"
        }
        195 => {
            "the user keychain is not writable on this host. Check its owner and permissions in Keychain Access"
        }
        35 | 128 => {
            "the user keychain denied access. Allow access in Keychain Access on the host, then try again"
        }
        _ => {
            return KeychainError::Failed(format!(
                "the macOS Keychain operation failed (tool exit {code})"
            ));
        }
    };
    KeychainError::Failed(message.into())
}

#[cfg(test)]
#[path = "macos_tests.rs"]
mod tests;
