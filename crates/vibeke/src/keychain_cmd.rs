//! `vibeke security keychain set|delete|check <ref>` (09 §9.1, 14 §6): store an assistant
//! credential in the keychain `[security] keychain` selects, without a server. The secret is
//! read from stdin (one line), never from the command line; nothing prints it back.

use vk_cli::{EXIT_OK, EXIT_USAGE};
use vk_store::keychain::{Keychain, parse_ref};

pub const USAGE: &str = "vibeke security keychain set|delete|check <account | service/account>\n  set reads the secret from stdin (one line); reference it in config as\n  [assistant.connections.<name>] credential = { keychain = \"<account>\" }\n  The backend is [security] keychain (\"os\": macOS Keychain or Secret Service; \"file:<path>\").";

pub fn run(args: &[String]) -> i32 {
    let kc = match vk_server::privacy::Settings::load().keychain {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[security] {e}");
            return EXIT_USAGE;
        }
    };
    let input = std::io::stdin();
    run_with(&kc, args, &mut input.lock())
}

pub fn run_with(kc: &Keychain, args: &[String], input: &mut dyn std::io::BufRead) -> i32 {
    let (Some(verb), Some(r)) = (args.first(), args.get(1)) else {
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    };
    let (service, account) = match parse_ref(r) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    match verb.as_str() {
        "set" => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                eprint!("secret for {service}/{account} (input is shown; paste and press Enter): ");
            }
            let mut line = String::new();
            if input.read_line(&mut line).is_err() || line.trim().is_empty() {
                eprintln!("no secret on stdin");
                return EXIT_USAGE;
            }
            match kc.set(&service, &account, line.trim()) {
                Ok(()) => {
                    eprintln!("stored {service}/{account} in {}", kc.describe());
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "delete" => match kc.delete(&service, &account) {
            Ok(true) => {
                eprintln!("deleted {service}/{account}");
                EXIT_OK
            }
            Ok(false) => {
                eprintln!("{service}/{account} is not in {}", kc.describe());
                EXIT_OK
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        "check" => match kc.get(&service, &account) {
            Ok(Some(_)) => {
                println!("{service}/{account}: present in {}", kc.describe());
                EXIT_OK
            }
            Ok(None) => {
                println!("{service}/{account}: missing from {}", kc.describe());
                1
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        _ => {
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn set_check_delete_with_the_file_backend() {
        let d = tempfile::tempdir().unwrap();
        let kc = Keychain::File(d.path().join("kc.json"));
        let mut empty: &[u8] = b"";
        assert_eq!(run_with(&kc, &a(&["check", "openai"]), &mut empty), 1);
        let mut secret: &[u8] = b"sk-test-xyz\n";
        assert_eq!(run_with(&kc, &a(&["set", "openai"]), &mut secret), EXIT_OK);
        assert_eq!(
            kc.get("vibeke", "openai").unwrap().as_deref(),
            Some("sk-test-xyz")
        );
        assert_eq!(run_with(&kc, &a(&["check", "openai"]), &mut empty), EXIT_OK);
        assert_eq!(
            run_with(&kc, &a(&["delete", "openai"]), &mut empty),
            EXIT_OK
        );
        assert_eq!(kc.get("vibeke", "openai").unwrap(), None);
        let mut none: &[u8] = b"\n";
        assert_eq!(run_with(&kc, &a(&["set", "x"]), &mut none), EXIT_USAGE);
        assert_eq!(run_with(&kc, &a(&["set"]), &mut none), EXIT_USAGE);
        assert_eq!(run_with(&kc, &a(&["frob", "x"]), &mut none), EXIT_USAGE);
        assert_eq!(
            run_with(&kc, &a(&["set", "bad ref"]), &mut none),
            EXIT_USAGE
        );
    }
}
