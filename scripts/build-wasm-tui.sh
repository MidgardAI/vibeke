#!/bin/sh
# Build the shared Rust TUI as a browser module. wasm-bindgen must match Cargo.lock.
set -eu
cd "$(dirname "$0")/.."
wasm_bindgen_cli=${WASM_BINDGEN:-wasm-bindgen}
if ! command -v "$wasm_bindgen_cli" >/dev/null 2>&1; then
    echo 'Install wasm-bindgen-cli 0.2.129, or set WASM_BINDGEN to its executable.' >&2
    exit 1
fi
if [ "$("$wasm_bindgen_cli" --version)" != 'wasm-bindgen 0.2.129' ]; then
    echo 'The browser build needs wasm-bindgen-cli 0.2.129.' >&2
    exit 1
fi
cargo rustc --locked -p vk-tui --lib --crate-type cdylib --target wasm32-unknown-unknown --profile wasm
out=web/apps/pwa/public/tui
staging=$(mktemp -d "${TMPDIR:-/tmp}/vibeke-wasm.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
"$wasm_bindgen_cli" --target web --out-dir "$staging" --out-name vk_tui \
    "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/wasm/vk_tui.wasm"
# Give glue and WASM one content identity, so browser caches cannot mix builds.
digest=$(cat "$staging/vk_tui.js" "$staging/vk_tui_bg.wasm" | shasum -a 256 | cut -c 1-16)
# This directory contains only ignored, generated browser assets.
rm -rf "$out"
mkdir -p "$out/$digest"
cp "$staging"/* "$out/$digest/"
printf '{"moduleUrl":"/tui/%s/vk_tui.js"}\n' "$digest" > "$out/manifest.json"
printf 'Browser TUI: %s/%s\n' "$out" "$digest"
