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
