# Previews and browsers

A **preview** gives local access to an app port. For a remote app, Vibeke forwards the port through the host connection. The local proxy uses separate origins and loopback addresses.

A **browser pane** shows an interactive Chromium page inside the terminal. It uses a separate browser profile. Vibeke does not expose the Chrome DevTools Protocol endpoint.

## Open a browser pane

1. Check browser availability:

   ```sh
   vibeke browser status
   ```

2. If Chromium is unavailable, run `vibeke browser install`. Confirm the download when prompted. On a Mac, or on Linux with a display, it installs the full Chrome for Testing, which also opens preview windows; on a headless machine it installs the smaller headless shell. Use `--full` or `--headless-shell` to choose.
3. Open a browser pane for your app:

   ```sh
   vibeke browser pane http://localhost:3000 --split right
   ```

The terminal must support graphics. Use `vibeke doctor` to check terminal support.

A browser pane supports page input, navigation, screenshots, and fixed device viewports. Remote previews keep the app on its original host.

## Manage previews

Press `prefix+shift+o` to open the **Previews** window. It lists the previews of every connected machine. Each row shows the port, the label and the pane that owns the preview. Markers show how you already view a preview:

- `▣ pane`: a browser pane shows the preview.
- `◎ proxy`: the local proxy has an address for the preview.
- `⇄ :port`: the preview is mirrored to a port on this machine.
- `!N`: the page reported N console errors.

Select a preview with the arrow keys and press a key:

| Key | Action |
| --- | --- |
| `enter` | Open the preview the way your `[preview] mode` setting says (a browser pane by default). |
| `w` | Open the preview in a separate browser window. |
| `p` | Open the preview in your normal browser through the proxy. |
| `m` | Mirror a remote preview to the same port on this machine, or stop the mirror. |
| `y` | Copy the preview URL. |
| `g` | Go to the pane that owns the preview. |
| `d` | Forget the preview. |
| `/` | Filter the list. |
| `i` | Install a browser for browser panes. |

A mirror opens a port without authentication. Any program of yours on this machine can connect to it. Vibeke asks before it creates a mirror. Stop the mirror when you are done.

The sidebar also lists previews. Click a preview row to show its actions: `[pane]`, `[window]`, `[proxy]`, `[mirror]` and `[copy]`. Click an action to run it. Double-click the row to open the preview. Right-click the row to choose an action from the command palette.

## Install a browser for browser panes

Browser panes need Chromium on the machine whose Vibeke server draws them. In a local session and with `vibeke ssh`, this is your computer. If you start Vibeke inside a plain `ssh` session, it is the remote machine. If no Chromium is found, the Previews window shows a red line. Press `i` there, or run this command on that machine:

```sh
vibeke browser install
```

On a Mac, or on Linux with a display, this installs the full Chrome for Testing (about 190 MB), which also opens browser windows. On a machine without a display, it installs the smaller headless shell (about 100 MB), which is enough for browser panes and agent sessions. Use `--full` or `--headless-shell` to choose. Vibeke checks the file against a known checksum before it unpacks the browser.

Browser windows also work with an installed Chrome or Chromium.

## Previews over vibeke ssh

With `vibeke ssh`, the Vibeke server on your computer shows remote previews. Browser panes, browser windows, the proxy and mirrors all run on your computer. The browser reaches the remote app through the host connection. So install the browser on your computer, not on the remote machine.

## Agent browser tools

An agent can open a browser session through the API. It can navigate, click, type, inspect page content, and capture screenshots. Console and network logs are also available.

Each session has a separate browser context. An agent can control its own browser sessions and sessions for panes that it created.

Destination policy limits browser access. Script evaluation needs an explicit `browser.script` capability.

Use `vibeke browser list` to find a session ID. Replace `<session>` with that ID in these commands:

```sh
vibeke browser snapshot <session>
vibeke browser console <session> --level error
vibeke browser network <session> --failed
vibeke browser screenshot <session> --full-page --out page.png
```

## Show an agent image

An agent can attach an image file, such as a screenshot from a test run. You then see the image in Vibeke. The image is stored with your other screenshots.

An agent in a Vibeke pane attaches an image with this command:

```sh
vibeke screenshot add test-results/login.png --caption "Login page after the fix"
```

The caption is optional. The command accepts PNG and JPEG files.

An agent can also use the `show_image` tool of `vibeke mcp`. To add this tool to Claude Code, run this command:

```sh
vibeke integration install claude --mcp
```

Use `codex` in place of `claude` for Codex. The pi extension adds the same tool by itself. The command shows a plan first. Add `--yes` to write the change.

You can see attached images in these places:

- In the terminal interface, the **Images** section of the sidebar, the viewer and the gallery (see below).
- In the desktop app and the phone app, open the **Screenshots** tab in the workspace panel.

The gallery shows the caption of an attached image. It marks the image as "attached by agent".

## Images in the terminal interface

### The Images section

The sidebar has an **Images** section below the attention section. It lists the newest images, one row for each image. A row shows the handle, the caption, the agent that attached the image and its age. New images are bright. An image stays new until you look at it. The header counts the new images, for example `images · 2 new`.

When more images exist, a row such as `+12 more on devbox` appears. Select it to open the gallery of that machine.

Two settings control the section:

```toml
[ui.images]
sidebar = true   # show the Images section
recent  = 5      # how many images it lists (1 to 20)
```

Select a row and press `enter`, or click it, to open the image in the viewer. Press `x` on a selected row to remove the image. A camera badge such as `📷2` next to an agent shows how many new images that agent has. Click the badge to open the newest one.

The **gallery** lists all images of one machine. Open it from the command palette with **Screenshot gallery**, or with `g` in the viewer.

### The viewer

The **viewer** shows one image over the pane area. Open it in one of these ways:

- Press `prefix+alt+i`. This opens the newest new image. If no image is new, it opens the gallery.
- Choose **View new images** in the command palette.
- Select a row in the **Images** section, or click a `📷N` badge.

These keys work in the viewer:

| Key | Action |
| --- | --- |
| `esc` | Dismiss. The image counts as seen, and the viewer closes. The image stays in the gallery. |
| `o` | Open the image on this machine, with your normal image program. |
| `x` | Remove the image. For an image that an agent attached, press `x` a second time to confirm. For a screenshot, the viewer asks you to confirm. |
| `←` and `→` | Show the older or the newer image. |
| `g` | Open the gallery on this image. |
| `y` | Copy the handle and the path of the image. |

A screenshot that a review accepted needs one more confirmation before Vibeke removes it.

### The pop-up card

When an agent attaches an image to the pane you are using, a small card appears over that pane. The card shows the image and its caption. It does not take the keyboard. Every key you type still goes to the pane, and the first key you type closes the card. The image stays new in the sidebar.

To look at the image, press `prefix+alt+i` or click the card. The viewer opens. Turn the card off in your configuration:

```toml
[ui.images]
popup = false
```

The card appears only for images that an agent attached to your focused pane. It does not appear for screenshots that you take yourself.

### Terminal support

The terminal interface shows images in terminals that support the **kitty graphics protocol**, such as kitty and Ghostty. It also shows images in iTerm2. The same terminals show preview thumbnails.

Other terminals show a short text hint in place of the image. The `o` key still opens the image on this machine. Images are not shown inside tmux. The hint appears there too.

### Remote machines

When the image lives on a remote machine, Vibeke fetches it when you open it in the viewer. It fetches the image at the size of the view, so the transfer stays small. In the gallery list, an image from a remote machine loads only when you press `v`.

The `o` key needs the original file. A remote original larger than 8 MiB cannot be opened this way. The viewer then shows the path on the host, and `y` copies it.

### Preview thumbnails

Set `ui.sidebar.preview_thumbnails = true` to show a small image of the newest screenshot on each preview row. Thumbnails work in kitty graphics terminals and in iTerm2. They also work for previews on remote machines.

## View and control a session

1. Open the agent browser in a pane:

   ```sh
   vibeke browser watch <session> --split right
   ```

2. Take control:

   ```sh
   vibeke browser take-over <session>
   ```

3. Use the page controls as necessary.
4. Return control to the agent:

   ```sh
   vibeke browser release <session>
   ```

While you control the page, agent browser commands return `human_control`. This prevents simultaneous input from you and the agent.

See the [browser commands](../reference/cli.md#vibeke-browser) and [control API](../reference/api.md) for arguments.
