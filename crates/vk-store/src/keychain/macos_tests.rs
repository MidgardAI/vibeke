use super::*;
use std::os::unix::fs::PermissionsExt;

struct Tool {
    dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl Tool {
    fn new(exit: i32) -> Self {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join("security");
        // The implicit domain deliberately fails. Only explicit user-domain calls work.
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
case "$1" in
default-keychain|list-keychains)
  test "$2" = -d && test "$3" = user || exit 195
  printf '    "/tmp/User Keychains/login.keychain-db"\n'
  if test "$1" = list-keychains; then printf '    "/tmp/other.keychain-db"\n'; fi
  ;;
-i)
  printf '%s\n' "$@" > '{dir}/argv'
  cat > '{dir}/input'
  exit {exit}
  ;;
find-generic-password|delete-generic-password)
  printf '%s\n' "$@" > '{dir}/argv'
  printf 'test-secret\n'
  exit {exit}
  ;;
*) exit 195 ;;
esac
"#,
                dir = dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir, path }
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap()
    }
}

#[test]
fn user_domain_is_used_for_writes_reads_and_deletes() {
    let tool = Tool::new(0);
    let b = Backend(&tool.path);
    b.set("vibeke/cloud/sprites", "vibeke", "org/id/dummy")
        .unwrap();
    assert_eq!(tool.read("argv"), "-i\n");
    assert!(
        tool.read("input")
            .ends_with("\"/tmp/User Keychains/login.keychain-db\"\n")
    );
    assert!(tool.read("input").contains("-w \"org/id/dummy\""));
    assert_eq!(
        b.get("vibeke/cloud/sprites", None).unwrap().as_deref(),
        Some("test-secret")
    );
    let args = tool.read("argv");
    assert!(args.ends_with("/tmp/User Keychains/login.keychain-db\n/tmp/other.keychain-db\n"));
    assert!(!args.contains("-a\n"));
    assert!(b.delete("vibeke", Some("state-key")).unwrap());
    assert!(tool.read("argv").contains("-a\nstate-key\n"));
}

#[test]
fn interaction_error_explains_host_unlock_without_exposing_input() {
    let tool = Tool::new(36);
    let error = Backend(&tool.path)
        .set("vibeke/cloud/sprites", "vibeke", "SENTINEL")
        .unwrap_err()
        .to_string();
    assert!(error.contains("Unlock it in Keychain Access on the host"));
    assert!(!error.contains("SENTINEL"));
    let tool = Tool::new(195);
    assert!(
        Backend(&tool.path)
            .set("vibeke", "test", "dummy")
            .unwrap_err()
            .to_string()
            .contains("not writable")
    );
}

#[test]
fn missing_items_remain_distinct_from_access_failures() {
    let tool = Tool::new(44);
    let b = Backend(&tool.path);
    assert_eq!(b.get("vibeke", None).unwrap(), None);
    assert!(!b.delete("vibeke", None).unwrap());
    let tool = Tool::new(36);
    assert!(Backend(&tool.path).get("vibeke", None).is_err());
}

#[test]
fn long_tokens_are_refused_before_interactive_execution() {
    let tool = Tool::new(0);
    let error = Backend(&tool.path)
        .set("vibeke", "test", &"x".repeat(4096))
        .unwrap_err();
    assert!(error.to_string().contains("too long"));
    assert!(!tool.dir.path().join("input").exists());
}

#[test]
fn the_default_keychain_is_read_even_when_absent_from_the_search_list() {
    let tool = Tool::new(0);
    let script = std::fs::read_to_string(&tool.path).unwrap();
    let script = script.replace(
        "default-keychain|list-keychains)",
        "default-keychain)\n  printf '    \"/tmp/default.keychain-db\"\\n'\n  ;;\nlist-keychains)",
    );
    std::fs::write(&tool.path, script).unwrap();
    Backend(&tool.path).get("vibeke", None).unwrap();
    assert!(
        tool.read("argv")
            .contains("-w\n/tmp/default.keychain-db\n/tmp/User Keychains/login.keychain-db\n")
    );
}
