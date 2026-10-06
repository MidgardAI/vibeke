//! Builds the vendored libghostty-vt (spec/03 §2.2) with the Zig toolchain pinned in mise.toml
//! and links it statically.
//!
//! The vendored source is copied into OUT_DIR before building: Zig 0.16 extracts fetched
//! packages into `<build root>/zig-pkg` and keeps its local cache next to the build root, so
//! building in place would write into the source tree and race between concurrent profiles.
//! Fetched Zig packages are hash-pinned by `build.zig.zon` and cached in the global Zig cache
//! (`ZIG_GLOBAL_CACHE_DIR`, default `~/.cache/zig`), so only the first build needs network.
//!
//! Environment overrides: `ZIG` (zig binary), `LIBGHOSTTY_VT_OPTIMIZE` (default ReleaseFast).

use std::env;
use std::fs;
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

    let json = fs::read_to_string(&manifest).expect("read vendor/libghostty-vt.vendor.json");
    let commit = json_str(&json, "commit");
    let version = json_str(&json, "ghostty_version");
    println!("cargo:rustc-env=VK_GHOSTTY_COMMIT={commit}");
    println!("cargo:rustc-env=VK_GHOSTTY_SHORT_COMMIT={}", &commit[..7]);

    let target = env::var("TARGET").unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let build_root = out.join("ghostty-src");
    let _ = fs::remove_dir_all(&build_root);
    copy_tree(&src, &build_root);

    let prefix = out.join("ghostty-out");
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());
    let run_zig = || {
        Command::new(&zig)
            .current_dir(&build_root)
            .arg("build")
            .arg("--prefix")
            .arg(&prefix)
            .arg("--cache-dir")
            .arg(out.join("zig-cache"))
            .arg("-Demit-lib-vt")
            .arg("-Demit-xcframework=false")
            .arg(format!("-Doptimize={optimize}"))
            .arg(format!("-Dtarget={}", zig_target(&target)))
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

    // Link only the static archive: on Apple targets `-l` prefers a sibling dylib.
    let lib_dir = out.join("ghostty-lib");
    fs::create_dir_all(&lib_dir).unwrap();
    fs::copy(
        prefix.join("lib/libghostty-vt.a"),
        lib_dir.join("libghostty-vt.a"),
    )
    .expect("libghostty-vt.a missing from zig output");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // The archive needs only libc/libm symbols (Zig compiles its C++ deps without a C++
    // runtime dependency); std already links those on every supported target (musl's libm is
    // part of its libc.a).
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}
