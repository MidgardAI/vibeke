//! Builds the vendored libghostty-vt (spec/03 §2.2) with the Zig toolchain pinned in mise.toml
//! and links it statically.
//!
//! The vendored source is copied into OUT_DIR before building: Zig 0.16 extracts fetched
//! packages into `<build root>/zig-pkg` and keeps its local cache next to the build root, so
//! building in place would write into the source tree and race between concurrent profiles.
//! Fetched Zig packages are hash-pinned by `build.zig.zon` and cached in the global Zig cache
//! (`ZIG_GLOBAL_CACHE_DIR`, default `~/.cache/zig`), so only the first build needs network.
//!
//! The built archive is also kept in a cache outside the target dir, keyed by a hash of the
//! vendored tree, this script, the Zig version, the target and the optimize mode. Cargo reruns
//! this script for every fresh target dir, profile wrapper (clippy) and CI run; the cache turns
//! those reruns from a multi-minute Zig build into a file copy.
//!
//! Environment overrides: `ZIG` (zig binary), `LIBGHOSTTY_VT_OPTIMIZE` (default ReleaseFast),
//! `VK_TERM_CACHE_DIR` (archive cache, default `$XDG_CACHE_HOME/vibeke/libghostty-vt` or
//! `~/.cache/vibeke/libghostty-vt`; `0` turns it off, as release and repro builds do).

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;

fn zig_target(target: &str) -> &'static str {
    match target {
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-apple-darwin" => "x86_64-macos",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        other => panic!("vk-term: unsupported target for libghostty-vt: {other}"),
    }
}

/// `"key": "value"` lookup in the small, flat vendor manifest (no JSON dependency).
fn json_str(json: &str, key: &str) -> String {
    let pat = format!("\"{key}\"");
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("vendor manifest lacks {key}"));
    let rest = &json[at + pat.len()..];
    let start = rest.find('"').expect("manifest value") + 1;
    let end = rest[start..].find('"').expect("manifest value end") + start;
    rest[start..end].to_string()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create dir");
    for entry in fs::read_dir(from).expect("read vendored dir") {
        let entry = entry.expect("dir entry");
        let ty = entry.file_type().expect("file type");
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &dst);
        } else {
            fs::copy(entry.path(), &dst).expect("copy vendored file");
        }
    }
}

/// Feeds every file under `dir` (relative path and contents, in sorted order) to `h`.
fn hash_tree(h: &mut DefaultHasher, root: &Path, dir: &Path) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read vendored dir")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            hash_tree(h, root, &path);
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy();
            let data = fs::read(&path).expect("read vendored file");
            h.write(rel.as_bytes());
            h.write_u64(data.len() as u64);
            h.write(&data);
        }
    }
}

fn cache_dir() -> Option<PathBuf> {
    match env::var_os("VK_TERM_CACHE_DIR") {
        Some(v) if v == "0" => None,
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .map(|c| c.join("vibeke/libghostty-vt")),
    }
}

/// `<cache>/<target>-<optimize>-<hash>.a` for these inputs, or `None` when caching is off or
/// Zig cannot report its version (the build below then reports the missing Zig).
fn cache_entry(
    zig: &str,
    src: &Path,
    inputs: &[&[u8]],
    target: &str,
    optimize: &str,
) -> Option<PathBuf> {
    let dir = cache_dir()?;
    let version = Command::new(zig).arg("version").output().ok()?;
    if !version.status.success() {
        return None;
    }
    let mut h = DefaultHasher::new();
    h.write(&version.stdout);
    for input in inputs {
        h.write_u64(input.len() as u64);
        h.write(input);
    }
    hash_tree(&mut h, src, src);
    Some(dir.join(format!("{target}-{optimize}-{:016x}.a", h.finish())))
}

/// Stores `archive` as `entry` (write then rename, so concurrent builds never see a partial
/// file) and drops older entries for the same target and optimize mode.
fn store(archive: &Path, entry: &Path) {
    let dir = entry.parent().unwrap();
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let name = entry.file_name().unwrap().to_string_lossy().into_owned();
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    if fs::copy(archive, &tmp).is_err() || fs::rename(&tmp, entry).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    let prefix = &name[..name.rfind('-').unwrap() + 1];
    for old in fs::read_dir(dir).into_iter().flatten().flatten() {
        let old_name = old.file_name().to_string_lossy().into_owned();
        if old_name.starts_with(prefix) && old_name.ends_with(".a") && old_name != name {
            let _ = fs::remove_file(old.path());
        }
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("../../vendor");
    let src = vendor.join("libghostty-vt");
    let manifest = vendor.join("libghostty-vt.vendor.json");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=VK_TERM_CACHE_DIR");

    let json = fs::read_to_string(&manifest).expect("read vendor/libghostty-vt.vendor.json");
    let commit = json_str(&json, "commit");
    let version = json_str(&json, "ghostty_version");
    println!("cargo:rustc-env=VK_GHOSTTY_COMMIT={commit}");
    println!("cargo:rustc-env=VK_GHOSTTY_SHORT_COMMIT={}", &commit[..7]);

    let target = env::var("TARGET").unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());

    // Link only the static archive: on Apple targets `-l` prefers a sibling dylib.
    let lib_dir = out.join("ghostty-lib");
    fs::create_dir_all(&lib_dir).unwrap();
    let archive = lib_dir.join("libghostty-vt.a");
    let script = fs::read(manifest_dir.join("build.rs")).expect("read build.rs");
    let inputs: [&[u8]; 2] = [&script, json.as_bytes()];
    let entry = cache_entry(&zig, &src, &inputs, &target, &optimize);
    let cached = entry
        .as_ref()
        .is_some_and(|e| fs::copy(e, &archive).is_ok());
    if !cached {
        build(&zig, &src, &out, &target, &optimize, &version, &commit);
        fs::copy(out.join("ghostty-out/lib/libghostty-vt.a"), &archive)
            .expect("libghostty-vt.a missing from zig output");
        if let Some(entry) = &entry {
            store(&archive, entry);
        }
    }
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // The archive needs only libc/libm symbols (Zig compiles its C++ deps without a C++
    // runtime dependency); std already links those on every supported target (musl's libm is
    // part of its libc.a).
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}

fn build(
    zig: &str,
    src: &Path,
    out: &Path,
    target: &str,
    optimize: &str,
    version: &str,
    commit: &str,
) {
    let build_root = out.join("ghostty-src");
    let _ = fs::remove_dir_all(&build_root);
    copy_tree(src, &build_root);

    let prefix = out.join("ghostty-out");
    let run_zig = || {
        Command::new(zig)
            .current_dir(&build_root)
            .arg("build")
            .arg("--prefix")
            .arg(&prefix)
            .arg("--cache-dir")
            .arg(out.join("zig-cache"))
            .arg("-Demit-lib-vt")
            .arg("-Demit-xcframework=false")
            .arg(format!("-Doptimize={optimize}"))
            .arg(format!("-Dtarget={}", zig_target(target)))
            // Explicit versions keep Ghostty's build from running `git describe` in our repo.
            .arg(format!("-Dversion-string={version}+{}", &commit[..7]))
            .status()
    };
    // On a cold cache Zig fetches Ghostty's dependencies over the network; retry once so a
    // transient connection error (seen on CI runners) doesn't fail the build.
    let status = match run_zig() {
        Ok(s) if !s.success() => run_zig(),
        other => other,
    }
    .unwrap_or_else(|e| {
        panic!(
            "vk-term: cannot run `{zig}` ({e}). libghostty-vt needs Zig 0.16.0: run \
                 `mise install` (pinned in mise.toml) or set ZIG"
        )
    });
    assert!(
        status.success(),
        "vk-term: `zig build -Demit-lib-vt` failed ({status}); check `zig version` is 0.16.0"
    );
}
