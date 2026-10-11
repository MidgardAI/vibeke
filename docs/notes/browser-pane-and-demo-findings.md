# Browser pane frame size, and other findings from the product video

These notes come from recording the product video in October 2026. They list one bug with a working fix, and some smaller problems that were found but not fixed. Pick them up as separate changes.

## 1. Browser pane images are squashed in tall panes (fix ready, not landed)

### Symptom

A browser pane in the terminal interface shows the page in a short strip at the top of the pane. The rest of the pane stays empty. It happens when the pane is taller than about 300 CSS pixels, so it shows in most split layouts.

### Cause

The server runs Chromium headless and sets the page size with `Emulation.setDeviceMetricsOverride` (`crates/vk-browser/src/cdp.rs`, `Page::set_viewport_for_cells`). Headless screencast frames have the size of the browser **window**, not of the emulated viewport. The window keeps Chromium's default size.

Example from a pane with a 502×759 CSS viewport at DPR 2:

- `browser.pane.status` reported `css: [502, 759]` and `frame: [1000, 464]`.
- The terminal interface scales the whole frame into the pane. A wide, short frame fills the pane's width and only part of its height.

Launching Chromium with a large `--window-size` makes it worse: the frames become the size of the large window, and the page fills only one corner of them.

There is a second, smaller problem. The CSS size is rounded with `round()`. For an odd number of device pixels at DPR 2, the frame comes out one pixel larger than the pane, for example 1004 pixels for a 1003-pixel pane. The tiler (`TileDiffer` in `crates/vk-browser/src/frame.rs`) then adds a column and a row of one-pixel tiles. The terminal stretches each of them to a whole cell, so a bright strip and a dark seam appear at the right and bottom edges.

### Fix that was tested

Size the headless window to the viewport whenever the viewport changes, and round the CSS size down:

```diff
diff --git a/crates/vk-browser/src/cdp.rs b/crates/vk-browser/src/cdp.rs
index f4802360..0da4d3c8 100644
--- a/crates/vk-browser/src/cdp.rs
+++ b/crates/vk-browser/src/cdp.rs
@@ -634,8 +634,27 @@ impl Page {
         dpr: f64,
     ) -> Result<(u32, u32)> {
         let (dev_w, dev_h) = (cols * cell_w, rows * cell_h);
-        let css_w = (dev_w as f64 / dpr).round() as u32;
-        let css_h = (dev_h as f64 / dpr).round() as u32;
+        // Round down: a frame even one pixel larger than the pane adds a row and a column of
+        // one-pixel tiles, which the terminal stretches to whole cells.
+        let css_w = (dev_w as f64 / dpr).floor() as u32;
+        let css_h = (dev_h as f64 / dpr).floor() as u32;
+        // Headless screencast frames have the window's size, not the emulated viewport's: size
+        // the window so its content area matches the pane (best effort; tests may lack it).
+        if let Ok(w) = self.call("Browser.getWindowForTarget", json!({}))
+            && let Some(id) = w["windowId"].as_i64()
+        {
+            // The window's own frame, measured without the emulated viewport.
+            let _ = self.call("Emulation.clearDeviceMetricsOverride", json!({}));
+            let chrome = self
+                .eval("[Math.max(0, outerWidth - innerWidth), Math.max(0, outerHeight - innerHeight)]")
+                .ok()
+                .and_then(|v| Some((v[0].as_u64()?, v[1].as_u64()?)))
+                .unwrap_or((0, 0));
+            let _ = self.call(
+                "Browser.setWindowBounds",
+                json!({ "windowId": id, "bounds": { "width": css_w as u64 + chrome.0, "height": css_h as u64 + chrome.1, "windowState": "normal" } }),
+            );
+        }
         self.call(
             "Emulation.setDeviceMetricsOverride",
             json!({ "width": css_w, "height": css_h, "deviceScaleFactor": dpr, "mobile": false }),
```

With this change, the same pane reported `css: [501, 758]` and `frame: [1002, 1516]`, and the page filled the pane.

### Before landing

- **Edge seam:** a faint one-pixel line is still visible at the right and bottom edges. The frame is now a pixel smaller than the pane, so the last tile column and row are narrower than their cells. Pad the frame to whole cells, or let the last tiles cover exactly their pixels.
- **Measuring the window frame:** the patch clears the device metrics override for a moment on every resize to measure the window's frame (`outerHeight - innerHeight`). This can flicker. Measure the frame once per browser, or derive it from `Browser.getWindowBounds` and a known content size.
- **Other paths:** check the pinned-device path (`set_viewport` with `pin` in `crates/vk-server/src/browser_page.rs`) and `chrome-headless-shell`, which may not support `Browser.setWindowBounds`. The patch ignores errors from those calls.
- **Tests:** `vibeke debug fake-chromium` does not model window-sized frames. Add a test that fails when the frame size differs from the viewport size, for example in `crates/vibeke/tests/it/browser_pane.rs` with a real Chromium (`VIBEKE_BROWSER_TESTS=1`).

## 2. A scripted focus command for the terminal interface (optional)

The video build adds `VIBEKE_TUI_DEMO_CONTROL=<fifo>`. Each line written to the FIFO runs one command in a running TUI:

- `focus <machine> <workspace>` calls `nav::focus_workspace`.
- `pane <machine> <pane>` calls `App::focus_pane`.

The sidebar selection moves the same way it does when a user selects the row. An outside `workspace.focus` call cannot do this, because focus is tracked per client. The hook could also help `web/apps/site/scripts/capture-clients.ts`, documentation screenshots and TUI end-to-end tests. Decide whether to ship it as a test hook or keep it out of releases.

## 3. Problems found but not fixed

- **Handoff picker offers a peer the server does not know yet.** Right after the TUI starts, `prefix+alt+h` lists a peer from the gateway, but `handoff.send` fails with "no peer <id>; `vibeke handoff peers` lists them". It works a few seconds later, once the server has synced its peers from the gateway. Either wait for the sync before listing the peer, or let the send wait for it.
- **The update badge ignores `[update] version_check = false`.** The TUI's own release check reads `update-checks.json` next to the configuration file (`crates/vk-tui/src/updates.rs`) and shows "↑ Update · v…" even when the configuration turns version checks off.
- **`vibeke browser pane list` opens a pane.** `browser pane <url>` takes the next word as the URL, so `list` opens a browser pane at `http://list/`. Treat `list` (and other verbs) as reserved, or reject URLs without a scheme or a dot.
- **Where the preview manager opens a browser pane.** `enter` in the preview manager (`prefix+shift+o`) opens the browser pane next to the pane that owns the port. When the dev server runs in its own small pane, the browser opens there instead of next to the agent the user is looking at. Consider opening it next to the focused pane when that pane is on the same machine.
