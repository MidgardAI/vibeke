//! `vibeke shell-integration zsh|bash|fish` (03 §8): shell snippets that emit OSC 133 prompt
//! and command marks (A prompt start, B command start, C output start, D;exit command end)
//! and OSC 7 working-directory reports, for shells that do not send them themselves.
//!
//! Load with `eval "$(vibeke shell-integration zsh)"` (bash the same) or
//! `vibeke shell-integration fish | source`. The snippets do nothing outside a terminal, never
//! run twice in one shell, keep existing prompt hooks (they add to `precmd`/`preexec`,
//! `PROMPT_COMMAND` and fish events), and encode the cwd as a `file://host/path` URL with
//! percent-escapes. With them, Vibeke gets prompt jumps (`[`/`]` in copy mode), "select last
//! command output", the exit-code badge, `pane.read --source last-command` and an exact cwd.

use crate::{EXIT_OK, EXIT_USAGE};

pub const SHELLS: &[&str] = &["zsh", "bash", "fish"];

const ZSH: &str = r#"# Vibeke shell integration for zsh (OSC 133 marks, OSC 7 cwd).
# eval "$(vibeke shell-integration zsh)" in ~/.zshrc
if [[ -o interactive && -t 1 && -z "${__vibeke_si:-}" ]]; then
  __vibeke_si=1
  __vibeke_osc7() {
    local p="" c i
    for (( i = 1; i <= ${#PWD}; i++ )); do
      c="${PWD[i]}"
      case "$c" in
        [-/._~A-Za-z0-9]) p+="$c" ;;
        *) p+=$(printf '%%%02X' "'$c") ;;
      esac
    done
    printf '\e]7;file://%s%s\e\\' "${HOST:-$(hostname)}" "$p"
  }
  __vibeke_precmd() {
    local ret=$?
    if [[ -n "${__vibeke_running:-}" ]]; then
      printf '\e]133;D;%s\e\\' "$ret"
      unset __vibeke_running
    fi
    __vibeke_osc7
    printf '\e]133;A\e\\'
  }
  __vibeke_preexec() {
    printf '\e]133;C\e\\'
    __vibeke_running=1
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __vibeke_precmd
  add-zsh-hook preexec __vibeke_preexec
  # B (command start) at the end of the prompt.
  [[ "$PS1" == *$'\e]133;B'* ]] || PS1="$PS1%{"$'\e]133;B\e\\'"%}"
fi
"#;

const BASH: &str = r#"# Vibeke shell integration for bash (OSC 133 marks, OSC 7 cwd).
# eval "$(vibeke shell-integration bash)" in ~/.bashrc
if [[ $- == *i* && -t 1 && -z "${__vibeke_si:-}" ]]; then
  __vibeke_si=1
  __vibeke_osc7() {
    local p="" c i
    for (( i = 0; i < ${#PWD}; i++ )); do
      c="${PWD:i:1}"
      case "$c" in
        [-/._~A-Za-z0-9]) p+="$c" ;;
        *) printf -v c '%%%02X' "'$c"; p+="$c" ;;
      esac
    done
    printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-$(hostname)}" "$p"
  }
  __vibeke_prompt() {
    local ret=$?
    if [[ -n "${__vibeke_running:-}" ]]; then
      printf '\e]133;D;%s\e\\' "$ret"
    fi
    __vibeke_running=""
    __vibeke_at_prompt=1
    __vibeke_osc7
    printf '\e]133;A\e\\'
    return $ret
  }
  # Output start: the first command run after a prompt (DEBUG fires for every simple command).
  __vibeke_debug() {
    [[ -n "${__vibeke_at_prompt:-}" && "$BASH_COMMAND" != __vibeke_prompt* ]] || return 0
    __vibeke_at_prompt=""
    __vibeke_running=1
    printf '\e]133;C\e\\'
  }
  if [[ -z "${PROMPT_COMMAND:-}" ]]; then
    PROMPT_COMMAND="__vibeke_prompt"
  elif [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
    PROMPT_COMMAND=(__vibeke_prompt "${PROMPT_COMMAND[@]}")
  else
    PROMPT_COMMAND="__vibeke_prompt;${PROMPT_COMMAND}"
  fi
  trap '__vibeke_debug' DEBUG
  [[ "$PS1" == *$'\e]133;B'* ]] || PS1="$PS1"'\[\e]133;B\e\\\]'
fi
"#;

const FISH: &str = r#"# Vibeke shell integration for fish (OSC 133 marks, OSC 7 cwd).
# vibeke shell-integration fish | source   (in ~/.config/fish/config.fish)
if status is-interactive; and isatty stdout; and not set -q __vibeke_si
    set -g __vibeke_si 1
    function __vibeke_osc7 --on-variable PWD
        printf '\e]7;file://%s%s\e\\' (hostname) (string escape --style=url -- $PWD | string replace -a '%2F' '/')
    end
    function __vibeke_prompt_start --on-event fish_prompt
        if set -q __vibeke_running
            printf '\e]133;D;%s\e\\' $__vibeke_status
            set -e __vibeke_running
        end
        printf '\e]133;A\e\\'
    end
    function __vibeke_preexec --on-event fish_preexec
        printf '\e]133;C\e\\'
        set -g __vibeke_running 1
    end
    function __vibeke_postexec --on-event fish_postexec
        set -g __vibeke_status $status
    end
    # B (command start) after the prompt: wrap fish_prompt once.
    if functions -q fish_prompt; and not functions -q __vibeke_orig_prompt
        functions -c fish_prompt __vibeke_orig_prompt
        function fish_prompt
            __vibeke_orig_prompt
            printf '\e]133;B\e\\'
        end
    end
    __vibeke_osc7
end
"#;

/// The snippet for `shell`, or `None` for an unsupported one.
pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(ZSH),
        "bash" => Some(BASH),
        "fish" => Some(FISH),
        _ => None,
    }
}

/// `vibeke shell-integration <shell>`; with no shell, the one named by `$SHELL`.
pub fn run(args: &[String]) -> i32 {
    let shell = args.first().cloned().or_else(|| {
        std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_string))
    });
    match shell.as_deref().and_then(script) {
        Some(s) => {
            print!("{s}");
            EXIT_OK
        }
        None => {
            eprintln!(
                "vibeke shell-integration <{}>\n  eval \"$(vibeke shell-integration zsh)\"   # or bash\n  vibeke shell-integration fish | source",
                SHELLS.join("|")
            );
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_snippet_emits_all_marks_and_osc7() {
        for sh in SHELLS {
            let s = script(sh).unwrap();
            for mark in ["133;A", "133;B", "133;C", "133;D;", "]7;file://"] {
                assert!(s.contains(mark), "{sh} lacks {mark}");
            }
            assert!(
                s.contains("__vibeke_si"),
                "{sh}: guarded against double loading"
            );
        }
        assert!(script("tcsh").is_none());
    }
}
