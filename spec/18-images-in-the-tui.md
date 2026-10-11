# 18 — Images in the TUI

**Status: planned (branch `tui-images`).** This spec makes screenshots and attached images easy to find and view in the terminal interface. It also makes the TUI's own image views work in iTerm2 as well as in kitty-compatible terminals. It builds on the gallery and 📷 counters of [06](06-remote-and-preview.md) B7/B8 and the `screenshot.*` API of [07](07-api-cli-plugins.md).

## 1. Problem

- **The gallery shows no image in iTerm2.** `gallery.rs` draws images only with kitty unicode placeholders. `browser::decide_gfx` turns kitty off for iTerm2, because iTerm2 accepts kitty transmissions but not placeholder placement. The gallery then falls back to "This terminal shows no graphics" (`gallery.rs`, `draw_image_or_hint`), and `v` refuses to fetch the image. Browser panes already work in iTerm2: they send a whole frame as an OSC 1337 inline image (`browser::inline_image`, `browser::after_write`). The WASM TUI in the browser app also takes the iTerm2 path (`web/mod.rs` sets `iterm2_images`), so it has the same gap.
- **Images are hard to find.** A new image shows a toast that disappears, and a small `📷N` badge on the agent row and on unfocused pane corners. The gallery opens only from `:screenshots` or the agent peek. No key opens it, and nothing stays in the sidebar.
- **Acting on one image takes many steps.** Dismissing, opening on this machine, or removing an image means opening the gallery, finding the row and confirming a dialog.

## 2. Goals

1. The TUI's own image views (gallery, diff, viewer, pop-up, preview thumbnails) work with kitty graphics and with iTerm2 inline images.
2. An **Images** section in the sidebar lists recent images. New ones stand out until you look at them.
3. A **viewer** shows one image with direct actions: dismiss, open on this machine, remove, older and newer, and open the gallery.
4. When an agent attaches an image to the focused pane, a **pop-up** shows it without taking the keyboard. A setting turns the pop-up off.
5. One additive API change: `screenshot.captured` gains `data.taken_by` (§6). Everything else is in `vk-tui` and `vk-config`.

Non-goals:
- images in the desktop and phone apps (they have a Screenshots tab already);
- syncing the "seen" state between clients (§10);
- images inside tmux (§3.5);
- iTerm2 support for images that programs place inside terminal panes (`pane_images.rs`). On iTerm2 they keep their `[image W×H]` label. That is a separate follow-up, because it needs placement that follows scrolling and pane clipping.

## 3. One drawing layer for both terminals

### 3.1 Module

Add `crates/vk-tui/src/image_view.rs`. Every TUI-owned image view registers **placements** with it while drawing. A placement has:

- an identity: the view and a slot, such as `viewer`, `gallery`, `diff:a` or `thumb:<preview id>`;
- the image key `(machine, screenshot id)` and its PNG bytes (or a pending fetch);
- the cell rectangle.

`image_view` picks the protocol with `browser::gfx(app)`:

| `gfx` | How placements are drawn |
| --- | --- |
| `Gfx::Kitty` | Each placement gets its own kitty image id from a reserved range, so a viewer and several thumbnails can be on screen at once. The image is transmitted once per id and size, and the cells are drawn as unicode placeholders (today's gallery code, moved here). Ids that are no longer placed are deleted. |
| `Gfx::Iterm` | Blank cells in the grid. After the frame is written, the PNG is sent with OSC 1337 over the rectangle (`preserveAspectRatio=1`, `doNotMoveCursor=1`). See §3.2–§3.3. |
| `Gfx::None` | The text hint. |

The text hint is also used when a capable terminal still cannot show the image: the image is too large, its file is gone (retention), or the transfer failed. The hint names the reason and the action that still works (§5.3).

### 3.2 iTerm2: when an image is sent

An inline image is not part of the grid. It stays on screen until the terminal writes those cells again, and it covers anything drawn there earlier. So:

- **Occlusion.** A placement is shown only when nothing in the composed frame overlaps it: no toast, menu, popup, plugin surface or notice. While something overlaps it, the placement is hidden (its cells are repainted, §3.3). It is sent again when it is uncovered. Toasts are short, so this is rare and simple.
- **Sending.** Sent in `after_write`, after the grid diff and after browser pane frames. It is sent again only when its key, rectangle or visibility changed, or after a full repaint or resize.
- **Browser panes.** Browser frames are already skipped while a popup is open (`after_write` allows `Mode::Normal` and an armed prefix). When the viewer closes, the browser panes it covered are marked `canvas_dirty` and sent again, even without a new media frame.

### 3.3 iTerm2: repainting cells under an image

`screen::diff` compares the previous and next grids and skips equal cells. Blank cells under a removed image are equal, so a normal redraw leaves the old image on screen. Add a forced repaint region:

- `App::invalidate(rect)` marks the cells of `rect` in the previous grid (`App.prev`, which `screen::diff` compares against) as unknown, so the next diff rewrites them.
- `image_view` calls it when a placement is hidden, moved, resized or removed, and when the view closes.
- Tests cover image → blank, a moved image, hiding the sidebar, and a browser canvas restored without a new frame.

### 3.4 Sizes and fetching

- **Local images** are read from disk. Images over the gallery's `MAX_IMAGE` (16 MiB) show the hint.
- **Remote images** are fetched with `screenshot.get {inline: true, thumb: <px>}`. The thumbnail width is the placement's pixel width (cells × cell size), so the transfer stays small. This also avoids the server's 8 MiB limit on inline originals.
- **`o` on a remote image** uses `screenshot.open`, which carries the original and is limited to 8 MiB. Over that limit, the action line says the image is too large to transfer and shows the path on the host. It does not offer `o`.
- **WASM TUI.** The machine is never local, so every image is fetched with `thumb`. `o` downloads the image in the browser, so its label there is `o download`.

### 3.5 tmux

OSC 1337 inside tmux needs passthrough, and the graphics probe does not detect tmux today. For this plan, image views inside tmux show the hint. A follow-up can add tmux passthrough, like the clipboard code does in `copyout.rs`.

### 3.6 Views moved onto `image_view`

- the gallery list and full view, and the diff view (`gallery.rs`: `placement`, `ensure_local_image`, the `v` key, `draw_image_or_hint`);
- the viewer and the pop-up (§5, §6);
- sidebar preview thumbnails (`ui.sidebar.preview_thumbnails`, `preview_ui.rs`). Today they are kitty-only and local-only. With `image_view` they also work on iTerm2, and on remote machines and the WASM TUI through `thumb` fetches.

## 4. Sidebar Images section

### 4.1 Where it goes

The section goes **near the top**, right after the attention section (`ui.sidebar.attention_section`), not at the end. The sidebar does not scroll and cuts rows at the screen height (`draw.rs`), so a section at the bottom would often be invisible. It is built before the navigate-mode `/` filter runs, so the filter can match it.

### 4.2 Data

The sidebar and the unseen state are kept apart:

- **Recent cache.** Per machine, the newest `ui.images.recent` records from `screenshot.list {limit}` (metadata only), plus the server's `total`. Fetched on connect and reconnect. After `screenshot.captured` and `screenshot.deleted`, the list is fetched again. A reply older than the last one applied for that machine is dropped.
- **Unseen set.** `(machine, screenshot id)` of images captured **after this client connected** to that machine, from `screenshot.captured` events. Images that existed before the connection count as seen. After a reconnect, the set keeps its entries, and new events add to it.
- **Deletions.** `screenshot.deleted` carries the deleted ids in `data.ids`, both for user deletions and for retention. They are removed from the unseen set, the recent cache, cached pixels, the viewer's position and any pending delete confirmation. This also covers deletions made by another client.

`GalleryState.counts` (the `📷N` badges) is replaced by a count of unseen images per `(machine, pane)`, taken from the unseen set. Badges then clear when you look at an image, not when the gallery opens.

### 4.3 Rows

- One row per image, newest first: `📷 s5 login page · claude · 2m`. The caption is used when there is one, else the page title or URL, else the environment label. The agent is the run or pane that attached or captured it. When that run or pane is gone, the row shows the label only. Images from a remote machine also show the machine label.
- Unseen rows are bright in the accent colour. Seen rows are dimmed. The header shows the count: `─ images · 2 new ─`.
- At most `ui.images.recent` rows in total, and at most a third of the sidebar height. Unseen images come first.
- Overflow rows are per machine: `+12 more on devbox` opens that machine's gallery. The count comes from the server's `total`.
- The section is hidden when there are no images, or when `ui.images.sidebar = false`.

### 4.4 Actions

- `SideRow` gets an `image: Option<(usize, String)>` target, next to `preview`. Navigate mode and mouse handlers check image targets **before** any pane-only action (`enter`, peek, `x`), so an image row never acts on a pane.
- `enter` or a click opens the viewer on that image. `x` on a selected row removes it, with the viewer's confirm rules (§5.2).
- A click on an agent row's `📷N` badge opens the viewer on that agent's newest unseen image.
- After a removal, the selection moves to the next image row, or leaves the section when it is empty.

## 5. Viewer

### 5.1 Layout

The viewer is a new popup mode, `Popup::ImageViewer`. It reuses the gallery's full-view state and drawing, with a shorter action line. It covers the pane area:

```
s5 · login page · attached by claude · 2m ago
                         <image>
esc dismiss · o open on this machine · x remove · ←/→ older/newer · g gallery · y copy path
```

### 5.2 Keys

| Key | Action |
| --- | --- |
| `esc` | Dismiss: mark the image as seen and close the viewer. The image stays in the gallery. |
| `o` | Open the image on this machine (`open_locally`; a download in the WASM TUI). See §3.4 for large remote images. |
| `x` | Remove the image with `screenshot.delete`. An attached image (`environment = agent`) asks for a second `x`, shown in the action line, with no dialog. A captured screenshot keeps today's confirm step. One that is part of an accepted review still needs `!`. After a removal, the viewer shows the next image, or closes when none is left. |
| `←` / `→` | Older or newer image. The order is all images in the recent caches, by time, across machines. When the viewer was opened for unseen images, it steps through the unseen ones first. Every step keeps the `(machine, id)` pair. |
| `g` | Open the gallery for that image's machine, on that image. |
| `y` | Copy the handle and path (as in the gallery). |

### 5.3 Ways to open it

- A new key action, `open_images`, bound to `prefix+alt+i` (unbound today). It is added to `DEFAULT_KEYMAP` in `keys.rs`, which defines the known actions, and to `default_config.toml`. `check_keys` must pass. The key opens the newest unseen image. If the pop-up (§6) is showing, the key opens that image in the viewer. When no image is new, it opens the gallery.
- A palette entry, **View new images**.
- The sidebar rows and badges (§4.4).
- The toast for a new image names the key: `📷 s5 attached by claude · prefix+alt+i to view`.

Opening the viewer counts as asking for the image, so a remote image is fetched (§3.4) without a separate `v`. The gallery list keeps its explicit `v`, so scrolling a remote gallery transfers nothing.

## 6. Pop-up when an agent attaches an image

### 6.1 It does not take the keyboard

A modal pop-up would take keys that you meant for the agent. Ordinary typing contains `o`, `x`, `g` and arrows, and `x x` would remove the image. So the pop-up is **non-modal**:

- It is a card over part of the focused pane: the image, its caption, and one line, `prefix+alt+i open · closes when you type`. The card takes at most half of the pane, in the corner away from the cursor.
- Every key, paste and mouse event goes to the pane as usual. The first key or paste that the pane receives closes the card. Key releases and repeats do not close it. Modifier-only presses do not close it.
- `prefix+alt+i` (or a click on the card) opens the image in the viewer (§5), which is modal.
- The image is marked as seen only when it is opened in the viewer or dismissed there. A card that closes because you typed leaves the image unseen, so it stays bright in the sidebar.
- The card is an `image_view` placement, so the iTerm2 occlusion and repaint rules apply.

### 6.2 When it appears

A client shows the card when all of these are true:

- `ui.images.popup` is `true`;
- the image was attached by an agent: `data.environment = agent` **and** `data.taken_by = agent`. The event gets a new, additive `taken_by` field (`agent | user | plugin`, from `ScreenshotMeta.taken_by.kind`). A person running `vibeke screenshot add` outside a pane is a `user`, and gets no card.
- `subject.pane` is this client's focused pane, on the machine this client is showing;
- the client is in `Mode::Normal`, with no plugin surface open and no prefix armed;
- the host terminal has focus (`App.host_focused`), when the terminal reports focus events.

The conditions are checked again when the image bytes arrive, because the fetch is asynchronous. If focus moved, the card is not shown.

Several images attached at once show one card, for the newest image, with a count: `3 new images`. Opening it in the viewer steps through them.

### 6.3 Several clients

Each client decides on its own. Two clients that show the same focused pane both show the card. Other clients get the toast, the badge and the sidebar row.

## 7. Configuration

New table in `crates/vk-config/src/default_config.toml` and `types.rs`:

```toml
[ui.images]
sidebar = true      # show the Images section in the sidebar
recent  = 5         # images listed there (1–20)
popup   = true      # show a card when an agent attaches an image to the focused pane
```

New key: `open_images = "prefix+alt+i"`.

`ui.sidebar.preview_thumbnails` keeps its name. Its comment changes from "(kitty graphics)" to "(kitty graphics or iTerm2)".

## 8. API change

`screenshot.captured` gains `data.taken_by` (`agent | user | plugin`). This is additive (`vibeke/1` rules): register it in `api_schema.rs`, then regenerate the API docs and clients. Clients that do not know the field ignore it.

## 9. Phases

Each phase is one commit on `tui-images`. The main agent builds and tests once at the end (see AGENTS.md).

1. **`image_view` and the gallery on iTerm2.** The module with per-placement kitty ids, the iTerm2 path with occlusion, `App::invalidate`, `thumb` fetches for remote images, and the gallery and diff views moved onto it. This fixes the reported bug on its own.
2. **Seen state and the Images section.** The recent cache and unseen set, deletion handling, unseen badges, the sidebar rows and the `SideRow.image` target.
3. **Viewer.** `Popup::ImageViewer` on the gallery's full-view state, its keys, `open_images`, the palette entry and the toast text.
4. **Pop-up.** The `taken_by` event field, the non-modal card and the rules of §6.
5. **Preview thumbnails on `image_view`**, including remote machines and the WASM TUI.
6. **Docs.** Update `docs/site/src/concepts/previews.md`: the sidebar section, the viewer, the keys, the pop-up setting, iTerm2 support and the tmux limitation. Update `terminal.md` if it lists the key. Regenerate the CLI, configuration and API references (`VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test it api_docs::`, then the clients). Update 06 B8 and 08 to point here.

## 10. Open questions

- **Seen state across clients.** Seen is kept per TUI session, like today's badges. A later `screenshot.seen` method could sync it to the desktop and phone apps. That would be an additive API change.
- **Pop-up for captured screenshots.** This plan shows the card only for attached images. A later value such as `popup = "attached" | "all" | "off"` could widen it. The boolean form would stay valid.

## 11. Tests

### 11.1 Unit tests

Unit tests go in the sibling `*_tests.rs` files. Each image test runs twice, once with `caps.kitty_graphics = true` and once with `caps.iterm2_images = true`.

- **`image_view`**
  - kitty: separate ids for a viewer and two thumbnails, including the same image at two sizes; unused ids are deleted.
  - iTerm2: one OSC 1337 sequence per placement in `after_write`, at the right cell and size, and nothing again when nothing changed.
  - iTerm2: the image is hidden while a toast overlaps it, then sent again; it is sent again after a resize.
  - Cells are repainted after hide, move and close (`App::invalidate`).
  - Hints for too large, file gone and transfer failed.
- **Gallery:** the full view and the diff view on iTerm2; `v` fetches with `thumb`; the 8 MiB `o` limit message.
- **Sidebar**
  - placement after the attention section; the height cap; the order;
  - unseen and seen styles; the header count; per-machine `+N more` using `total`;
  - `ui.images.sidebar = false`; the `/` filter;
  - image targets win over pane actions; the selection after a deletion.
- **Unseen state:** images from before the connection are seen; reconnect; replies arriving out of order; deletions from another client and from retention (`data.ids`).
- **Viewer**
  - every key in §5.2: two-step `x` for attached images, the confirm step for captured ones, and `!` for review evidence;
  - stepping across machines, and the unseen-first order;
  - closing after the last removal; WASM label `o download`.
- **Pop-up**
  - It appears for an agent-attached image in the focused pane.
  - It does not appear for: another pane, another machine, `taken_by = user`, a captured screenshot, a plugin surface, an armed prefix, an unfocused host terminal, `ui.images.popup = false`, or focus that moved before the bytes arrived.
  - Typing `x`, `o`, `g`, arrows and a paste reaches the pane and closes the card.
  - `prefix+alt+i` opens the viewer. Closing the card by typing leaves the image unseen.
- **Config:** defaults, the `recent` range, and `check_keys` with `open_images` in `DEFAULT_KEYMAP`.
- **Server:** `screenshot.captured` carries `taken_by` for `screenshot.add` from a pane and from the CLI.

### 11.2 Manual checks before merging

- Real iTerm2, real kitty (or Ghostty) and the WASM TUI in the browser app.
- On each one: the gallery, the viewer, the pop-up over a running agent, a toast over an image, and closing the viewer over a browser pane.
- tmux shows the hint.
