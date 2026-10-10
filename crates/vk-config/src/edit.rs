//! Editing config keys (07 §2.14 `config.set`): comment-preserving edits of `config.toml` with
//! `toml_edit`, atomic writes, and the process-wide runtime override layer that
//! [`Config::load`](crate::Config::load) applies on top of the user's file.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Runtime overrides (`config.set {persist: false}`): dotted key -> value (`None` = the key is
/// reset to its default for this process). Applied by `Config::load` of [`crate::config_path`].
fn overrides() -> &'static Mutex<BTreeMap<String, Option<toml::Value>>> {
    static O: OnceLock<Mutex<BTreeMap<String, Option<toml::Value>>>> = OnceLock::new();
    O.get_or_init(Mutex::default)
}

/// Set (`Some`) or reset to default (`None`) a runtime override for `key`.
pub fn set_runtime_override(key: &str, value: Option<toml::Value>) {
    overrides().lock().unwrap().insert(key.to_string(), value);
}

/// Drop the runtime override of `key` (the file's value applies again).
pub fn clear_runtime_override(key: &str) -> bool {
    overrides().lock().unwrap().remove(key).is_some()
}

/// The current runtime overrides.
pub fn runtime_overrides() -> BTreeMap<String, Option<toml::Value>> {
    overrides().lock().unwrap().clone()
}

/// Apply the runtime overrides to config text (used by `Config::load`). Unchanged when there
/// are none or when the text does not parse (the parse error is then reported as usual).
pub(crate) fn apply_runtime_overrides(src: &str) -> String {
    let o = runtime_overrides();
    if !crate::layers::has_overrides() {
        return src.to_string();
    }
    let Ok(mut doc) = src.parse::<toml_edit::DocumentMut>() else {
        return src.to_string();
    };
    for (k, v) in &o {
        let _ = set_in_doc(&mut doc, k, v.as_ref());
    }
    // The CLI layer (`--config-override`, `VIBEKE_CONFIG_OVERRIDE`) sits above runtime.
    crate::layers::apply_cli(&mut doc);
    doc.to_string()
}

/// Split a dotted key (`ui.sidebar.width`, `keys.bindings."ctrl+a"`) into its parts. Quoted
/// segments may contain dots.
pub fn split_key(key: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in key.chars() {
        match ch {
            '"' => quoted = !quoted,
            '.' if !quoted => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    if quoted || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("invalid key `{key}`"));
    }
    Ok(parts)
}

fn to_edit_value(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Datetime(d) => d.to_string().into(),
        toml::Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(to_edit_value(x));
            }
            toml_edit::Value::Array(arr)
        }
        toml::Value::Table(t) => {
            let mut it = toml_edit::InlineTable::new();
            for (k, x) in t {
                it.insert(k, to_edit_value(x));
            }
            toml_edit::Value::InlineTable(it)
        }
    }
}

/// Set (or with `None`, remove) `key` in a document, keeping comments and formatting of every
/// other line. Missing parent tables are created (as implicit tables, so `[ui.sidebar]` appears
/// only when needed). A table value replaces the key with an inline table.
pub fn set_in_doc(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: Option<&toml::Value>,
) -> Result<(), String> {
    let parts = split_key(key)?;
    let (last, parents) = parts.split_last().expect("split_key never returns empty");
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for p in parents {
        if table.get(p).is_none() {
            if value.is_none() {
                return Ok(());
            }
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            table.insert(p, toml_edit::Item::Table(t));
        }
        let item = table.get_mut(p).expect("inserted above");
        table = match item {
            toml_edit::Item::Table(t) => t,
            toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => t,
            _ => return Err(format!("`{p}` in `{key}` is not a table")),
        };
    }
    match value {
        None => {
            table.remove(last);
        }
        Some(v) => {
            let new = to_edit_value(v);
            match table.get_mut(last) {
                // Keep the existing line's decoration (trailing comment, spacing).
                Some(toml_edit::Item::Value(old)) => {
                    let decor = old.decor().clone();
                    *old = new;
                    *old.decor_mut() = decor;
                }
                Some(item @ toml_edit::Item::Table(_)) if !v.is_table() => {
                    return Err(format!(
                        "`{key}` is a table; set one of its keys instead ({})",
                        item.type_name()
                    ));
                }
                _ => {
                    table.insert(last, toml_edit::Item::Value(new));
                }
            }
        }
    }
    Ok(())
}

/// Text of `src` with `key` set to `value` (`None` removes it), comments preserved.
pub fn edit_text(src: &str, key: &str, value: Option<&toml::Value>) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = src.parse().map_err(|e| format!("{e}"))?;
    set_in_doc(&mut doc, key, value)?;
    Ok(doc.to_string())
}

/// Write `text` to `path` atomically: a private temp file in the same directory, fsync, then
/// rename over the target. The file mode of an existing target is kept (new files are 0600).
#[cfg(not(target_arch = "wasm32"))]
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Look up a dotted key in a TOML value tree.
pub fn lookup<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut cur = root;
    for p in split_key(key).ok()? {
        cur = cur.as_table()?.get(&p)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_keeps_comments_and_creates_tables() {
        let src = "# top comment\n[ui]\n# the theme\ntheme = \"dark\" # trailing\n\n[terminal]\nscrollback_lines = 100\n";
        let out = edit_text(src, "ui.theme", Some(&toml::Value::String("light".into()))).unwrap();
        assert!(out.contains("# top comment"));
        assert!(out.contains("# the theme"));
        assert!(out.contains("theme = \"light\" # trailing"), "{out}");
        let out = edit_text(&out, "ui.sidebar.width", Some(&toml::Value::Integer(30))).unwrap();
        assert!(out.contains("width = 30"), "{out}");
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            lookup(&v, "ui.sidebar.width"),
            Some(&toml::Value::Integer(30))
        );
        let out = edit_text(&out, "terminal.scrollback_lines", None).unwrap();
        assert!(!out.contains("scrollback_lines"));
        assert!(edit_text(&out, "ui", Some(&toml::Value::Integer(1))).is_err());
        assert_eq!(
            split_key("keys.bindings.\"a.b\"").unwrap(),
            vec!["keys", "bindings", "a.b"]
        );
        assert!(split_key("a..b").is_err());
    }

    #[test]
    fn atomic_write_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        write_atomic(&p, "a = 1\n").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&p, "a = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a = 2\n");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }
}

#[cfg(target_arch = "wasm32")]
pub fn write_atomic(_: &Path, _: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "Host configuration files are unavailable in the browser",
    ))
}
