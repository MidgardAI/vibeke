# Finder drag-and-drop formats per terminal (06 A11 [verify M0])

Status: **not verified by automated drag** (no GUI drag available in CI or this sandbox). Entries
are tagged:

- **[docs]** stated in public documentation or release notes
- **[source]** recalled from reading the project's source; moderate confidence, re-check against the
  version in use
- **[assumed]** inference or hearsay; must be confirmed by a manual drag test during M0

`paste.rs` is deliberately format-agnostic so every variant below is handled: backslash escapes,
single quotes, double quotes and `file://` URLs, separated by spaces or newlines, with or without
a trailing space, delivered bracketed or not (the client sees the paste event either way; if a
terminal delivers a drop as plain typed keystrokes, there is no paste event to intercept and
translation cannot apply there).

| Terminal | Quoting of a single path | Multiple files | Delivery | Confidence |
|---|---|---|---|---|
| iTerm2 | backslash-escapes shell-special characters, including spaces: `/Users/x/Screenshot\ 2026.png` | space-separated, trailing space after each | handled as a paste of text; bracketed when the app enabled mode 2004 | quoting [docs] (iTerm2 "Drag and drop" inserts the shell-escaped path); bracketed delivery [assumed] |
| Ghostty (macOS) | backslash-escapes (own shell-escape helper escapes space, parens, brackets, quotes, `$`, `&`, `;`, `|`, `*`, `?`, `!`, `#`, backtick, `<`, `>`, tab, backslash) | space-joined | text send via the surface; bracketed paste when the app enabled 2004 is likely, not confirmed | escaping [source]; bracketed [assumed] |
| Ghostty (Linux/GTK) | GTK delivers a file list or `text/uri-list`; paths are shell-escaped before insertion | space-joined | as above | [assumed] |
| kitty | no native Finder drop on macOS in older releases; Linux drops of `text/uri-list` are inserted as paths (local `file://` URLs converted to paths) | whitespace/newline-separated | bracketed paste if enabled | [assumed]; treat as unverified |
| WezTerm | `DroppedFile` window event; paths are quoted with a shell-quoting helper (single quotes when special characters are present, bare otherwise), joined with a space | space-joined | pasted to the pane (bracketed if the app enabled it) | [source], moderate confidence |
| Terminal.app | backslash-escapes (spaces become `\ `) | space-separated | inserted as a paste, bracketed when the app enabled 2004 | [assumed]; classic behavior, unverified for current macOS |

## Observed variations to expect (all [assumed] until tested)

- Trailing space after the last path (iTerm2, Ghostty): parsed as whitespace and preserved by
  `rewrite`, so the agent still sees the same trailing space.
- macOS screenshot names contain U+202F (narrow no-break space) before AM/PM. Terminals do not
  escape it; the tokenizer treats only ASCII whitespace as a separator, so it stays inside the
  word. If a terminal does backslash-escape it, that is also handled.
- `file://` URLs appear when the terminal passes the pasteboard URL through unconverted (some
  Linux drops, some browsers' dragged links). `file://localhost/...` is accepted; any other host
  is rejected.
- Image drags from a browser or Preview may deliver a URL or no text at all; only local paths that
  exist are rewritten, everything else is pasted as-is.
- Paths with a newline in the name cannot be represented safely unquoted; `rewrite` falls back to
  single quotes for replacement paths containing control characters.

## M0 manual test checklist

For each terminal, drag (a) one file with spaces and non-ASCII in the name, (b) two files,
(c) a directory, into a shell running `cat -v` with bracketed paste on and off, and record the exact
bytes (look for `^[[200~`, backslashes, quote characters, `file://`, separators, trailing space).
Update the table, upgrading entries from [source]/[assumed] to [verified].
