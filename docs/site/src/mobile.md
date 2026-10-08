# Mobile and desktop access

The browser and Electron apps connect to a Vibeke session through a gateway. The gateway makes an outbound connection to a relay.

The development host does not need an inbound port. You do not need to expose its control socket.

## Start a local system

1. From the repository root, install the web dependencies:

   ```sh
   cd web
   bun install
   ```

2. Build the browser app:

   ```sh
   bun run build
   cd ..
   ```

3. Start a session with `vibeke`.
4. In another terminal, start the relay:

   ```sh
   cargo run -p vk-relay --bin vibeke-relay -- \
     --public-url http://localhost:8787 \
     --app-dir web/apps/pwa/dist
   ```

You do not run the gateway yourself. The next step starts it.

## Set up and pair the browser

1. Set up the gateway and create a pairing invitation:

   ```sh
   vibeke gateway pair --relay http://localhost:8787
   ```

   The command saves the relay, turns the gateway on, starts it through the server, and waits for it to connect. It then shows the pairing link and QR code.

2. Open the printed link, or scan the QR code.
3. Compare the browser fingerprint with the terminal fingerprint.
4. If they match, select **Pair**.
5. Confirm the pairing in the terminal.

Later runs of `vibeke gateway pair` skip the relay option and only create an invitation.

Localhost works for a browser on the same computer. A phone needs a reachable relay origin with HTTPS. On a phone, `localhost` refers to the phone.

For restricted access, use `pair --scope approve` or `pair --scope view`.

## Autostart

After setup, autostart is on. The server starts the gateway when it starts. The gateway stops when the server stops. If the gateway crashes, the server restarts it after a short delay. After repeated crashes it stops trying and reports `crashed`.

Commands:

- `vibeke gateway status`: show the setup and whether the gateway is running.
- `vibeke gateway on`: turn autostart on and start the gateway.
- `vibeke gateway off`: turn autostart off and stop the gateway.
- `vibeke gateway logs [-f] [-n N]`: show the gateway log. Use `-f` to follow it.
- `vibeke server status`: show the server state, including a `gateway` line.

The terminal client shows a gateway indicator in the status bar. It appears only after setup. Without setup, it is hidden.

If you start `vibeke gateway run` by hand, the server detects it and reports the gateway as `external`. It does not start a second one.

## If you don't use the phone or desktop apps

Nothing runs and no connection opens until you pair. The gateway stays off until you run `vibeke gateway pair` or `vibeke gateway on`.

## Turning it off

1. Stop the gateway and keep it off:

   ```sh
   vibeke gateway off
   ```

   It stays off after server restarts. Run `vibeke gateway on` to turn it back on.

2. To remove a paired device, list the devices and revoke one:

   ```sh
   vibeke gateway devices
   vibeke gateway revoke <id>
   ```

A revoked device cannot reconnect. Create a new invitation to pair it again.

## iOS notifications

Web Push requires HTTPS. On iOS or iPadOS 16.4 and later:

1. Add Vibeke to the Home Screen in Safari.
2. Open the installed app.
3. Select **Settings → Alerts → Turn on**.

Configure notifications separately on each device.

## Desktop app

Run `bun run dev:desktop` from `web/` to start the Electron app.

The desktop app supports relay and local Unix-socket connections. It provides native notifications, menu-bar approvals, and separate pane windows.

See the [desktop development guide](../../../web/apps/desktop/README.md) for builds and packaging.

## Trust and recovery

The relay carries encrypted messages. The app origin remains trusted because its JavaScript can access the device key and decrypted content.

Check the origin and build hash in **Settings → About**.

The browser stores pairing state for each origin. A different port or domain creates a different device. After device revocation, create a new pairing invitation.

See the [web apps guide](../../../web/README.md) for development instructions.
