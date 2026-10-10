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

The download is about 100 MB. Vibeke checks the file against a known checksum before it unpacks the browser.

Browser windows use a normal Chrome or Chromium installation instead. Install one of these browsers if you want to use windows.

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
