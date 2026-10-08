# Phone and browser access

Open [app.vibeke.dev](https://app.vibeke.dev) on your phone or computer.
The browser app connects to your Vibeke host through `relay.vibeke.dev`.
The relay carries encrypted traffic. Your host makes an outbound connection and needs no inbound port.

For local desktop access, use the [desktop connection guide](desktop.md).

## Pair your device

1. [Install the CLI](install.md) on your host and start a session with `vibeke`.
2. In a shell on that host, run:

   ```sh
   vibeke gateway pair \
     --relay https://relay.vibeke.dev \
     --app-url https://app.vibeke.dev
   ```

   This saves the connection settings, enables gateway autostart, and asks the server to start it.
   Keep the command running while you pair.

3. Scan the QR code on your phone, or open the printed link in your browser.
4. Compare the device fingerprint with the fingerprint shown by the pairing command.
5. Confirm pairing on the device and in the terminal when prompted.

After the first setup, pair another device from the TUI or the shell.

In the TUI, press `prefix` then `alt+d`, or run **Pair a phone** from the command palette.
Choose the access level and press `enter`.
Scan the QR code, then confirm the fingerprint in the prompt that appears in the TUI.
Press `c` to copy the link, or `esc` to cancel it.

From a shell:

```sh
vibeke gateway pair
```

For limited access, use `vibeke gateway pair --scope approve` or `vibeke gateway pair --scope view`.
Each invitation is temporary and can be used once.

## Keep the connection available

After setup, the server starts the gateway whenever the server starts.
It restarts the gateway after a crash, with a retry limit. The gateway stops when the server stops.
The TUI status bar shows gateway state after setup.

The host must remain awake and online. Closing the terminal client does not stop the server or its panes.

## Manage devices

In the TUI, press `prefix` then `alt+d` to list your paired devices.
Select one and press `x`, then `y`, to revoke it. Press `n` to pair a new one.
The 📱 count in the status bar shows how many devices are connected.

From a shell:

```sh
vibeke gateway devices
vibeke gateway revoke <id>
```

A revoked device cannot reconnect. Create a new invitation to pair it again.

To stop remote access and disable autostart:

```sh
vibeke gateway off
```

Use `vibeke gateway on` to enable it again.

## Install on your phone

On iOS or iPadOS, open the app in Safari and add it to the Home Screen.
Open that installed app before pairing, so you pair the app you will use.
For supported Web Push notifications, use iOS or iPadOS 16.4 or later.

Select **Settings → Alerts → Turn on** and allow notifications.
Configure notifications separately on each device.

## Troubleshooting

- Run `vibeke gateway status` to check the gateway and relay connection.
- Run `vibeke gateway logs -f` to follow its log.
- Check that the host is awake and the Vibeke server is running.
- Create a new invitation if the previous link expired or the device was revoked.

The browser stores pairing keys for each origin. Changing domains creates a separate device.
The app origin is trusted with keys and decrypted content. Check it before opening a pairing link.

For your own relay or app hosting, see [self-hosting](self-hosting.md).
