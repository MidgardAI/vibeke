//! `vibeke doctor`: assistance settings (14 §9), diagnosed **without a paid request** and
//! without contacting any provider. The checks themselves live in `vk_assist::diagnose`
//! (pure over the parsed config, the consent grants and an environment lookup); this adds
//! the report plumbing.

use super::*;
use vk_assist::diagnose::{self, Level as AssistLevel};

const ASSISTANT: &str = "assistant";

fn level(l: AssistLevel) -> Level {
    match l {
        AssistLevel::Pass => Level::Pass,
        AssistLevel::Info => Level::Info,
        AssistLevel::Warn => Level::Warn,
        AssistLevel::Fail => Level::Fail,
    }
}

pub(super) fn check(r: &mut Report) {
    let config = vk_server::assist::load_config().map(|(c, _)| c);
    let grants = vk_assist::consent::load(&vk_server::assist::consent_path());
    let machine = crate::commands::hostname();
    let env = |k: &str| std::env::var(k).ok();
    for f in diagnose::diagnose(&diagnose::Input {
        config,
        grants: &grants,
        machine: &machine,
        env: &env,
    }) {
        match f.hint {
            Some(h) => r.add_hint(ASSISTANT, level(f.level), f.message, h),
            None => r.add(ASSISTANT, level(f.level), f.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_become_report_checks_in_the_assistant_section() {
        // Exercised through the pure checker (the real config file is never read in tests).
        let env = |_: &str| None;
        let mut r = Report::default();
        for f in diagnose::diagnose(&diagnose::Input {
            config: Err("[assistant]: unknown key `enabeld`".into()),
            grants: &[],
            machine: "laptop",
            env: &env,
        }) {
            match f.hint {
                Some(h) => r.add_hint(ASSISTANT, level(f.level), f.message, h),
                None => r.add(ASSISTANT, level(f.level), f.message),
            }
        }
        assert!(r.failed());
        let text = r.render_text();
        assert!(text.contains("assistant"), "{text}");
        assert!(text.contains("unknown key `enabeld`"), "{text}");
        assert!(text.contains("fix:"), "{text}");
    }

    #[test]
    fn levels_map_one_to_one() {
        assert_eq!(level(AssistLevel::Fail), Level::Fail);
        assert_eq!(level(AssistLevel::Pass), Level::Pass);
        assert_eq!(level(AssistLevel::Warn), Level::Warn);
        assert_eq!(level(AssistLevel::Info), Level::Info);
    }
}
