// Read the ABI version from the built module, without creating a browser TUI or requiring a DOM.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
const directory = process.argv[2];
if (!directory) throw new Error('Pass the wasm-bindgen output directory');
const module = await import(pathToFileURL(resolve(directory, 'vk_tui.js')).href);
module.initSync({ module: readFileSync(resolve(directory, 'vk_tui_bg.wasm')) });
const api = module.browser_api();
if (!Number.isSafeInteger(api) || api < 1) throw new Error('Invalid browser interface version');
console.log(api);
