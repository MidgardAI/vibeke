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
profile=${WASM_PROFILE:-wasm-release}
source_revision=$(git rev-parse HEAD)
source_inputs=$(bun web/scripts/tui-source.ts inputs)
source_digest=$(bun web/scripts/tui-source.ts "$source_inputs")
cargo rustc --locked -p vk-tui --lib --crate-type cdylib --target wasm32-unknown-unknown --profile "$profile"
out=web/apps/pwa/public/tui
staging=$(mktemp -d "${TMPDIR:-/tmp}/vibeke-wasm.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
"$wasm_bindgen_cli" --target web --out-dir "$staging" --out-name vk_tui \
    "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/$profile/vk_tui.wasm"
if [ "$source_digest" != "$(bun web/scripts/tui-source.ts "$source_inputs")" ]; then
    echo 'Rust sources changed during the WASM build. Build again before packaging.' >&2
    exit 1
fi
browser_api=$(bun web/scripts/tui-module-api.ts "$staging")
# Give glue and WASM one content identity, so browser caches cannot mix builds.
digest=$(cat "$staging/vk_tui.js" "$staging/vk_tui_bg.wasm" | shasum -a 256 | cut -c 1-16)
# This directory contains only ignored, generated browser assets.
rm -rf "$out"
mkdir -p "$out/$digest"
cp "$staging"/* "$out/$digest/"
printf '{"api":%s,"moduleUrl":"/tui/%s/vk_tui.js","sourceRevision":"%s","sourceDigest":"%s","sourceInputs":%s}\n' "$browser_api" "$digest" "$source_revision" "$source_digest" "$source_inputs" > "$out/manifest.json"
printf 'Browser TUI: %s/%s\n' "$out" "$digest"
