# Desktop app

The desktop app gives you native notifications, menu-bar approvals, and separate pane windows.
Download it from the [latest release](https://github.com/MidgardAI/vibeke/releases/latest).
See [installation](install.md#desktop-app) for supported platforms and signing details.

## Connect to this computer

Local connections work on supported macOS and Linux hosts.

1. [Install the host CLI](install.md#terminal-cli).
2. Start `vibeke` in your project and keep the session running.
3. Open the desktop app.
4. Select **Connect to this Mac** or **Connect to this computer**.
5. If prompted, select **Start gateway** and retry the connection.
6. Select **Open Vibeke** after the connection succeeds.

The app discovers the installed CLI and pairs over a private local socket.
This connection does not need a relay or QR code.
If the CLI is not found, use **Choose…** to select the `vibeke` executable.

## Connect to a remote host

On the remote host, follow the [pairing instructions](mobile.md#pair-your-device).
Paste the resulting pairing link into the desktop app. Confirm the device fingerprint on the host.
The connection uses the public relay. The remote host needs no inbound port.

Windows and Intel Macs can use the desktop app with a supported remote host.
The v0.1.0 release does not include a local host CLI for those platforms.

## Notifications and updates

Configure alerts in the app settings. Allow notifications when your operating system asks.
The host and gateway must remain running to receive live requests.

To update the desktop app, download the new release and install it. Automatic desktop updates are not configured.
For host connection problems, run `vibeke gateway status` and `vibeke doctor` on the host.
