# 18 — Images in the TUI

**Status: planned (branch `tui-images`).** This spec makes screenshots and attached images easy to find and view in the terminal interface. It also makes every image view work in iTerm2 as well as in kitty-compatible terminals. It builds on the gallery and 📷 counters of [06](06-remote-and-preview.md) B7/B8 and the `screenshot.*` API of [07](07-api-cli-plugins.md).

## 1. Problem

- **The gallery shows no image in iTerm2.** `gallery.rs` draws images only with kitty unicode placeholders. `browser::decide_gfx` turns kitty off for iTerm2, because iTerm2 accepts kitty transmissions but not placeholder placement. The gallery then falls back to "This terminal shows no graphics" (`gallery.rs`, `draw_image_or_hint`), and `v` refuses to fetch the image. Browser panes already work in iTerm2: they send a whole frame as an OSC 1337 inline image (`browser::inline_image`, `browser::after_write`). The WASM TUI in the browser app also takes the iTerm2 path (`web/mod.rs` sets `iterm2_images`), so it has the same gap.
- **Images are hard to find.** A new image shows a toast that disappears, and a small `📷N` badge on the agent row and on unfocused pane corners. The gallery opens only from `:screenshots` or the agent peek. No key opens it, and nothing stays in the sidebar.
- **Acting on one image takes many steps.** Dismissing, opening on this machine, or removing an image means opening the gallery, finding the row and confirming a dialog.

## 2. Goals

1. Every image view in the TUI works with kitty graphics and with iTerm2 inline images. The text hint is only for terminals with neither.
2. An **Images** section in the sidebar lists recent images. New ones stand out until you look at them.
3. A **quick viewer** shows one image with direct actions: dismiss, open on this machine, remove, next and previous, and open the gallery.
4. When an agent attaches an image to the focused pane, the viewer **pops up**. A setting turns this off.
5. No API change is needed. Everything here is in `vk-tui` and `vk-config`.

Non-goals: images in the desktop and phone apps (they have a Screenshots tab already), syncing the "seen" state between clients (see §9), and iTerm2 support for images that programs place inside terminal panes (§8, phase 5).

## 3. One drawing path for both terminals

Add a small module, `crates/vk-tui/src/image_view.rs`, that every image view uses. It takes an image key, its PNG bytes and a cell rectangle, and draws it in the best way the host supports:

| `browser::gfx(app)` | How the image is drawn |
| --- | --- |
| `Gfx::Kitty` | Today's path: transmit once per image and size, then draw unicode placeholder cells (moved here from `gallery.rs`). |
| `Gfx::Iterm` | Draw blank cells in the grid. After the frame is written, send the PNG unchanged with OSC 1337 over the rectangle (`preserveAspectRatio=1`, `doNotMoveCursor=1`). |
| `Gfx::None` | The text hint, with `o open on this machine`. |

iTerm2 details:

- **Emit after the grid.** Queue the image during drawing. Write it in `after_write`, next to the browser pane frames, so the grid diff does not paint over it.
- **Emit only when needed.** Remember the last `(key, rect)` that was sent. Send again when the key or rectangle changes, after a full repaint or resize, or when the grid diff wrote any cell inside the rectangle (for example, a toast or menu that crossed it).
- **Clear on close.** An inline image stays on screen until its cells are written again. When a view closes or moves, mark its old rectangle dirty so the next frame repaints those cells.
- **No overlap with browser panes.** `after_write` already skips browser frames outside `Mode::Normal`. The viewer is a popup mode, so the browser frames under it pause. When it closes, mark the browser panes it covered as dirty (`canvas_dirty`) so they are sent again.
- **Size.** Use the PNG as stored. Do not decode and re-encode it. Images over the gallery's `MAX_IMAGE` limit show the hint and `o`.
- **Remote hosts and SSH.** iTerm2 needs the bytes in the terminal stream. A local image is read from disk. A remote image is fetched with `screenshot.open` when the user asks for it (§5).

Callers moved onto `image_view`:

- the gallery list and full view, and the diff view (`gallery.rs`: `placement`, `ensure_local_image`, the `v` key, `draw_image_or_hint`);
- the new quick viewer (§5);
- sidebar preview thumbnails (`ui.sidebar.preview_thumbnails`, `preview_ui.rs`), which are kitty-only today. On iTerm2 a thumbnail is a small inline image on its row. It is sent again only when the newest screenshot of that preview changes.

## 4. Sidebar Images section

A `─ images ─` section sits just above `─ previews ─` (`draw.rs`). Previews stay last, so `browser::preview_hit` keeps working.

- **Source.** On connect, and again after `screenshot.captured` or `screenshot.deleted`, each machine is asked for `screenshot.list {limit: ui.images.recent}` (metadata only, no image bytes). The result is kept per machine in `GalleryState`.
- **Rows.** One row per image, newest first, across all connected machines: `📷 s5 login page · claude · 2m`. The caption is used when there is one, else the page title or URL, else the environment label. The agent is the run or pane that attached or captured it. A remote image also shows the machine label.
- **Seen state.** Unseen images are drawn bright in the accent colour. Seen images are dimmed. An image counts as seen when it is shown in the viewer or the gallery's full view, or when it is dismissed. The header shows the count: `─ images · 2 new ─`.
- **Size.** At most `ui.images.recent` rows (default 5). A last row, `+N more`, opens the gallery. The section is hidden when there are no images, or when `ui.images.sidebar = false`.
- **Actions.** `enter` or a click opens the quick viewer on that image. `x` on a selected row removes the image, with the same confirm rules as the viewer. A click on an agent row's `📷N` badge opens the viewer on that agent's newest unseen image.
- **Badges.** `GalleryState.counts` becomes a count of unseen images per pane, derived from the seen state. Badges then clear when you look at the images, not when the gallery opens.
- **Navigation.** Add a `SideRow.image: Option<(usize, String)>` target next to `preview`, so the mouse and navigate mode can reach image rows. The `/` filter matches captions and handles.

## 5. Quick viewer

The viewer is a new popup mode, `Popup::ImageViewer`. It covers the pane area, like the gallery's full view. It shows one image and one action line:

```
s5 · login page · attached by claude · 2m ago
                         <image>
esc dismiss · o open on this machine · x remove · ←/→ older/newer · g gallery · y copy path
```

| Key | Action |
| --- | --- |
| `esc` | Dismiss: mark the image as seen and close the viewer. The image stays in the gallery. |
| `o` | Open the image on this machine with the OS opener. A remote image is fetched first (today's `open_locally`). |
| `x` | Remove the image with `screenshot.delete`. An image attached by an agent (`environment.kind = agent`) asks for a second `x`, shown inline in the action line, with no dialog. A captured screenshot keeps today's confirm step. One that is part of an accepted review still needs `!`. After a removal, the viewer shows the next image, or closes when none is left. |
| `←` / `→` | Older or newer image. When the viewer was opened for unseen images, these step through the unseen ones first. |
| `g` | Open the full gallery on this image. |
| `y` | Copy the handle and path (as in the gallery). |

**Ways to open it.**

- A new key, `open_images = "prefix+alt+i"`, opens the newest unseen image, or the gallery when no image is new. The key is unbound today. `check_keys` must still pass.
- A palette entry, **View new images**.
- The sidebar rows and badges (§4).
- The toast for a new image names the key: `📷 s5 attached by claude · prefix+alt+i to view`.

**Remote images.** Opening the viewer counts as asking for the image, so the viewer fetches a remote image (`screenshot.open`) without a separate `v`. The gallery list keeps its explicit `v`, so scrolling a remote gallery transfers nothing.

## 6. Pop-up when an agent attaches an image

When a client receives `screenshot.captured`, it opens the viewer on that image when all of these are true:

- `ui.images.popup` is `true`;
- `data.environment` is `agent`, which means the image came from `screenshot.add` (an attached file, not a browser capture);
- `subject.pane` is the focused pane of this client, on the machine this client is showing;
- the client is in `Mode::Normal`. Another popup, the palette, a prompt or copy mode stays open. The image then only gets its toast, badge and sidebar row.

Typing must not be lost. Keys that are not viewer keys close the viewer and go on to the pane as usual. Several images attached at once open the viewer once, on the first one. `→` then steps through the rest.

The pop-up opens only in the client whose focused pane received the image. Other attached clients get the toast, badge and sidebar row.

## 7. Configuration

New table in `crates/vk-config/src/default_config.toml` and `types.rs`:

```toml
[ui.images]
sidebar = true      # show the Images section in the sidebar
recent  = 5         # images listed there (1–20)
popup   = true      # open the viewer when an agent attaches an image to the focused pane
```

New key in `[keys]`: `open_images = "prefix+alt+i"`.

`ui.sidebar.preview_thumbnails` keeps its name. Its comment changes from "(kitty graphics)" to "(kitty graphics or iTerm2)".

## 8. Phases

Each phase is one commit on `tui-images`. The main agent builds and tests once at the end (see AGENTS.md).

1. **`image_view` and the gallery on iTerm2.** Add the module, move the kitty code into it, add the iTerm2 path, and switch the gallery and the diff view to it. This fixes the reported bug on its own.
2. **Seen state and the Images section.** Seen tracking, the sidebar rows and actions, badges driven by unseen counts, and the `SideRow.image` target.
3. **Quick viewer.** `Popup::ImageViewer`, its keys, `open_images`, the palette entry and the toast text.
4. **Pop-up on attach.** The trigger rules of §6 and `ui.images.popup`.
5. **Preview thumbnails on iTerm2.** Move `preview_ui` thumbnails to `image_view`. Then look at images placed by programs in panes (`pane_images.rs`). On iTerm2 they show an `[image W×H]` label today. Supporting them needs OSC 1337 placement that follows scrolling and pane clipping. Write that down as its own follow-up if it turns out too large.
6. **Docs.** Update `docs/site/src/concepts/previews.md` (the images section: sidebar, viewer, keys, pop-up setting, iTerm2 support) and `terminal.md` if it lists keys. Regenerate the CLI and configuration references (`VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test it api_docs::`). Update 06 B8 and 08 (keys and config) to point here.

## 9. Open questions

- **Seen state across clients.** Seen is kept per TUI session for now, like today's badges. A later `screenshot.seen` method could sync it to the desktop and phone apps. That would be an additive API change.
- **Pop-up for captured screenshots.** This plan pops up only for attached images. A browser screenshot that an agent captures for review does not pop up. A later value such as `popup = "attached" | "all" | "off"` could widen this. The boolean form would stay valid.

## 10. Tests

Unit tests in the sibling `*_tests.rs` files. Each image test runs twice, once with `caps.kitty_graphics = true` and once with `caps.iterm2_images = true`:

- `image_view`: kitty output places placeholder cells. iTerm2 output writes one OSC 1337 sequence in `after_write`, with the expected cell position and size. Nothing is sent again when the key and rectangle did not change. It is sent again after a resize, and after a toast crossed the rectangle. The rectangle is repainted when the view closes. With neither capability, the hint is drawn.
- Gallery: the full view and the diff view draw on iTerm2. `v` fetches on iTerm2.
- Sidebar: rows, the order, unseen and seen styles, the header count, `+N more`, `ui.images.sidebar = false`, and image row targets for the mouse and navigate mode.
- Viewer: every key in §5, including the two-step `x` for attached images, the confirm step for captured ones, and `!` for review evidence. Also stepping through unseen images, and closing after the last removal.
- Pop-up: opens for an attached image in the focused pane. Does not open for another pane, another machine, a captured screenshot, when the client is not in `Mode::Normal`, or with `ui.images.popup = false`. A non-viewer key closes the viewer and reaches the pane.
- Config: defaults, validation of `recent`, and `check_keys` with `open_images`.

Integration: extend `crates/vibeke/tests/it/screenshot_add.rs` only if a server-side behaviour changes. This plan does not need one.
