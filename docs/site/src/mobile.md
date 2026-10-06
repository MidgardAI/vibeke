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

5. In another terminal, start the gateway:

   ```sh
   cargo run -p vk-gateway --bin vibeke-gateway -- run \
     --relay http://localhost:8787
   ```

## Pair the browser

1. Create a pairing invitation:

   ```sh
   cargo run -p vk-gateway --bin vibeke-gateway -- pair
   ```

2. Open the printed link.
3. Compare the browser fingerprint with the terminal fingerprint.
4. If they match, select **Pair**.
5. Confirm the pairing in the terminal.

Localhost works for a browser on the same computer. A phone needs a reachable relay origin with HTTPS. On a phone, `localhost` refers to the phone.

For restricted access, use `pair --scope approve` or `pair --scope view`. Use `vibeke-gateway devices` to list devices. Use `vibeke-gateway revoke <id>` to revoke a device.

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
