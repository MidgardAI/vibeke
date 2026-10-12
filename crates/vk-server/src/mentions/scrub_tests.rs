use super::*;
use serde_json::json;

const ID: &str = "m_01k7abcdefghjkmnpqrstvwxyz";

#[test]
fn one_line_envelope_loses_preview_and_nonce() {
    let line = format!("❯ ↪ vibeke message {ID} from @coder · the secret plan [ABCDEFGHJKMN]");
    assert_eq!(
        scrub(&line),
        format!("❯ ↪ vibeke message {ID} from @coder · … […]")
    );
    // The same as the hook payload sanitizer.
    let bare = format!("↪ vibeke message {ID} from @coder · the secret plan [ABCDEFGHJKMN]");
    assert_eq!(
        scrub(&bare).into_owned(),
        super::super::hooks::sanitize_envelope(&bare).unwrap()
    );
}

#[test]
fn a_wrapped_envelope_is_scrubbed_across_rows() {
    // The preview wraps, and the nonce bracket continues on the next row.
    let text = format!(
        "before\n> ↪ vibeke message {ID} from the user (in @lead) · review the\n  migration plan for the billing tables\n  [ABCDEFGH\n  JKMN]\nafter"
    );
    let out = scrub(&text);
    assert_eq!(
        out,
        format!("before\n> ↪ vibeke message {ID} from the user (in @lead) · … […]\nafter")
    );
    assert!(!out.contains("ABCDEFGH"));
    assert!(!out.contains("billing"));
    // A box edge in front of the continuation row.
    let text = format!(
        "│ ↪ vibeke message {ID} from @coder · short │\n│ [ABCDEFGHJKMN]                           │"
    );
    let out = scrub(&text);
    assert!(!out.contains("ABCDEFGHJKMN"), "{out}");
    assert!(!out.contains("short"), "{out}");
    // The words of the head may wrap too, also inside a word or the id.
    let text = format!("↪ vibeke\nmessage {ID} from @coder · x [ABCDEFGHJKMN]");
    assert!(!scrub(&text).contains("ABCDEFGHJKMN"));
    let text = format!("> ↪ vibeke mess\nage {ID} from @coder · x [ABCDEFGHJKMN]");
    assert!(!scrub(&text).contains("ABCDEFGHJKMN"));
    let (a, b) = ID.split_at(10);
    let text = format!("> ↪ vibeke message {a}\n{b} from @coder · x [ABCDEFGHJKMN]");
    assert!(!scrub(&text).contains("ABCDEFGHJKMN"));
}

#[test]
fn other_text_is_left_alone() {
    for t in [
        "nothing here",
        "vibeke message without an id [ABCDEFGHJKMN]",
        // Already scrubbed.
        "↪ vibeke message m_01abc from @coder · … […]",
        // Not a nonce: lower case, too short.
        "↪ vibeke message m_01abc from @coder · x [abcdefghjkmn]",
        "↪ vibeke message m_01abc from @coder · x [ABCDEF]",
    ] {
        assert!(matches!(scrub(t), Cow::Borrowed(_)), "{t}");
    }
    // Two envelopes in one text.
    let t = format!(
        "↪ vibeke message {ID} from @a · one [ABCDEFGHJKMN]\n↪ vibeke message {ID} from @b · two [NMKJHGFEDCBA]"
    );
    let out = scrub(&t);
    assert_eq!(out.matches("· … […]").count(), 2, "{out}");
}

#[test]
fn json_strings_are_scrubbed_everywhere() {
    let mut v = json!({
        "turns": [{"items": [{"kind": "user", "text": format!("↪ vibeke message {ID} from @a · secret [ABCDEFGHJKMN]")}]}],
        "n": 3,
    });
    scrub_value(&mut v);
    let t = v["turns"][0]["items"][0]["text"].as_str().unwrap();
    assert_eq!(t, format!("↪ vibeke message {ID} from @a · … […]"));
    assert_eq!(v["n"], 3);
}
