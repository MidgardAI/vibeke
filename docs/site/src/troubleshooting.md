# Troubleshooting

Find your symptom below. Each section lists likely causes and the commands that check them. If nothing here helps, [collect a debug bundle](#collect-a-debug-bundle-to-report-a-bug).

Start with this command. It checks your installation, terminal, sockets and integrations, and it prints a hint for each problem:

```sh
vibeke doctor
```

## Vibeke does not start or cannot connect to the server

Likely causes: the shell cannot find `vibeke`, an old server is stuck, or the installation is damaged.

1. Run `vibeke --version`. If the shell says the command is not found, add `export PATH="$HOME/.local/bin:$PATH"` to your shell configuration and open a new terminal. See [installation](install.md).
2. Run `vibeke doctor` and read every line marked `warn` or `FAIL`.
3. Run `vibeke server status` to see whether a server is running.
4. If the server does not answer, stop it and start Vibeke again:

   ```sh
   vibeke server stop
   vibeke
   ```

   `vibeke server stop` leaves the pane processes running. They reconnect when the new server starts. See [holders](concepts/holders.md).

The option `--no-spawn` makes a command fail instead of starting a server. Use it when you want to check for a running server without starting one.

## Agent state does not show in the sidebar

Likely causes: the integration for your agent is not installed, or you started the agent outside a Vibeke pane.

1. Start the agent in a Vibeke pane. Vibeke shows the agents that run in its panes.
2. Check the integrations:

   ```sh
   vibeke integration status all
   vibeke integration doctor all
   ```

3. If an integration is not installed, open **Setup** in the command palette (**Ctrl+B**, then **:**). Select the integration, press **d** to inspect the changes, and press **i** to install. You can also run `vibeke integration install claude` and replace `claude` with `codex`, `pi` or `omp`.
4. Start the agent again after the installation.

Without an integration, Vibeke reads the terminal output. This shows less detail than the integration does. See [agents and interactions](concepts/agents.md).

## Answers do not reach the agent

Likely causes: the agent has no integration that can take answers, or the request was already answered or closed.

- Run `vibeke integration doctor all`. It shows what each integration can do.
- Open the request in the attention inbox (**Ctrl+B**, then **i**). The card shows whether Vibeke delivered the answer.
- Answer in the pane itself if the integration cannot deliver answers. Terminal detection can show a request, but the agent's own interface takes the answer.
- Vibeke keeps requests and delivery state after a server restart. It does not send the same answer twice.

See [answer a request](concepts/agents.md#answer-a-request).

## Images do not show in the terminal

Likely causes: your terminal does not draw images, or you run Vibeke inside tmux.

- The terminal interface draws images in terminals that support the kitty graphics protocol, such as kitty and Ghostty, and in iTerm2.
- Other terminals, and tmux, show a short text hint. Press `o` on the image to open it on this machine.

See [images in the terminal interface](concepts/previews.md#images-in-the-terminal-interface).

## Keys do not work

Likely causes: another program takes the same keys, or you run Vibeke inside tmux.

Vibeke uses **Ctrl+B** as its prefix. tmux uses the same key by default. When you run Vibeke inside tmux, tmux receives the prefix first. Use one of these fixes:

- Press **Ctrl+B** twice. tmux then passes one **Ctrl+B** to Vibeke.
- Change the Vibeke prefix. Run `vibeke config edit` and set `prefix` in the `[keys]` section, for example `prefix = "ctrl+a"`. Then run `vibeke config validate`.

To see what Vibeke binds, run `vibeke keys`, or press **Ctrl+B** and wait for the key menu. Vibeke reports key conflicts when it checks the configuration. See [customize Vibeke](customize.md) and the [configuration reference](reference/config.md).

## Phone pairing fails or the app shows offline

Likely causes: the host is asleep, the gateway is not running, the invitation expired, or the relay needs an account.

1. Check that the host is awake and that a Vibeke session is running.
2. Run `vibeke gateway status` to check the gateway and the relay connection.
3. Run `vibeke gateway logs -f` to follow the gateway log while you try again.
4. Run `vibeke gateway pair` for a new invitation. An invitation works once and expires. A revoked device cannot reconnect either.
5. If you use `cloud.vibeke.dev`, run `vibeke whoami` to check that you are signed in. Run `vibeke login` if you are not. With `--relay https://relay.vibeke.dev` no account is needed.
6. Compare the device fingerprint on the phone with the one on the host before you confirm.

The browser stores pairing keys for each origin. A different domain is a separate device and needs its own pairing. If the host is offline, the app shows the last saved state with the time it was captured. See [phone and browser access](mobile.md).

## Panes are gone after a restart

Each pane has a holder process that keeps the pane process running. The holder survives these events:

- You close the terminal or detach the client.
- The server stops, crashes or restarts, for example during `vibeke update`.

The holder does not survive a restart of the host computer, or a failure of the holder itself. Those events stop the pane processes. Start the agent again in a new pane.

Run `vibeke` again after a server restart. Screen recovery can be incomplete, so a pane may show a partial screen until the program redraws. See [holders and process recovery](concepts/holders.md).

## The desktop app cannot find the CLI

The desktop app does not include the host CLI. It looks for `vibeke` in the `VIBEKE_BIN` environment variable, in the usual install locations, and on your `PATH`.

1. Install the CLI and check `vibeke --version` in a terminal. See [installation](install.md).
2. In the app, select **Choose…** and pick the `vibeke` executable, for example `~/.local/bin/vibeke`.
3. Start `vibeke` in your project and keep it running. Select **Start gateway** in the app if it asks.
4. On Linux, check that a Secret Service or keyring backend is available.

Windows and Intel Macs have no host CLI. Connect them to a remote host. See the [desktop guide](desktop.md).

## Collect a debug bundle to report a bug

A debug bundle is an archive with diagnostics. Vibeke passes it through redaction, which removes recognized secrets. Redaction is best-effort, so review the files before you share them.

```sh
vibeke debug bundle
vibeke debug bundle --out ~/vibeke-debug.tar.gz
```

The command prints the path, each file in the bundle, and what it leaves out. It works when no server runs and never starts one. Two options add more detail:

- `--include-pane <pane>` adds the visible screen of a pane. Repeat it for more panes. This needs a running server.
- `--include-scrollback` adds saved scrollback.

Add the Vibeke version from `vibeke --version`, your operating system and the steps that cause the problem. Report bugs at [github.com/MidgardAI/vibeke/issues](https://github.com/MidgardAI/vibeke/issues).

## Next steps

- [Frequently asked questions](faq.md) for short answers about safety, platforms and updates.
- [Process durability](concepts/holders.md) explains what survives a restart.
- [Phone and browser access](mobile.md) covers pairing, devices and recovery.
