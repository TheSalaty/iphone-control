# Configuration

Everything is configured with environment variables, and all of them are optional:

- **Docker:** in `.env` in the repository directory (copied from `.env.example`); apply with `docker compose up -d`.
- **Just a compose file:** under `environment:` in your `compose.yaml` (or a `.env` next to it plus `env_file`); apply with `docker compose up -d` as well.
- **Source install:** in `~/.config/iphone-control/env` (read by the systemd user service); apply with `systemctl --user restart iphone-control`.
- **macOS:** also in `~/.config/iphone-control/env`, loaded before starting the daemon; restart the daemon to apply. See [macOS](../deploy/macos#configuration).

## Common

| Variable | Default | Meaning |
|---|---|---|
| `HTTP_PROXY` / `HTTPS_PROXY` | direct | Proxy for Apple, anisette, Developer Disk Image and notification traffic |
| `IPHONE_CONTROL_LANG` | `zh` | Language: `zh` or `en`. Applies to tool descriptions and results, CLI output, logs and notifications. The MCP server and the CLI each send their own setting and the daemon answers in it, so for a source install also set it in the MCP client config (`iphone-control mcp-config` includes it) |
| `IPHONE_UDID` | automatic | Pin the device to control. Unset: the controlled device is kept until it disconnects, and you can switch with the `select` command or the `iphone_select_device` tool. When set, switching is refused |
| `RESIGN_MIN_DAYS` | `2` | Re-sign when fewer days than this remain |
| `RESIGN_CHECK_HOURS` | `6` | How often to check; it is also checked within a minute after the phone reconnects or you switch devices |
| `OFFLINE_ALERT_MIN` | `10` | Notify after the phone has been offline this many minutes; `0` disables |

Notification variables: see [Notifications](./notifications).

## Advanced

| Variable | Meaning |
|---|---|
| `ICTL_ANISETTE_URL` | Your own anisette server (the default is a public third-party one; see [Security](./security)) |
| `ICTL_DDI_URL` | Developer Disk Image download URL; the default is a community-maintained mirror |
| `ICTL_DDI_DIR` | A Developer Disk Image directory you extracted from Xcode. The daemon doesn't download anything or delete its files, and the error names any missing file. When unset, the `ddi/` cache in the data directory is used, and it is deleted and downloaded again after 3 failed mounts in a row |
| `ICTL_MANAGE_USBMUXD` | Whether the daemon starts its own usbmuxd. Default: yes as root (container), no as a regular user, which uses the system one. `0` or `false` turns it off, for example as root on a host that already runs a usbmuxd service |
| `ICTL_USBMUXD_BIN` | The executable the daemon runs when it starts usbmuxd itself; default `usbmuxd` |
| `ICTL_WDA_UNSIGNED` | Path of the unsigned WDA. Default: `/usr/share/iphone-control/WDA-unsigned.ipa` in the image; without an image (source install), `wda/WDA-unsigned.ipa` in the data directory, downloaded before the first signing. A path you set yourself is not downloaded |
| `SCREEN_W` / `SCREEN_H` | Screen size in points for the HID fallback, used only while WDA is down and there is no record of this device's size: WDA has never connected, or you switched to a device without one. It applies to every device, so take care with several |
| `ICTL_LOG` | Log filter, e.g. `info,ictl_wda::http=debug` logs every WDA request with its duration |

## Directories and control socket

You normally leave these alone, and shouldn't change them with Docker (the container fixes `ICTL_DATA_DIR`, where the data volume is mounted). If you do set them, don't leave them empty, and give the daemon, the CLI and the MCP server the same values: outside a container only the daemon reads the settings file; see [Source / Binary](../deploy/source#run).

| Variable | Default | Meaning |
|---|---|---|
| `ICTL_DATA_DIR` | root: `/var/lib/iphone-control`<br>regular user: `$XDG_DATA_HOME/iphone-control`, or `~/.local/share/iphone-control` if unset | Data directory; same as the CLI's `--data-dir` |
| `ICTL_SOCKET` | root: `/run/iphone-control/ctl.sock`<br>regular user: `$XDG_RUNTIME_DIR/iphone-control/ctl.sock`, or in the data directory without that variable (macOS) | The daemon's control socket; same as the CLI's `--socket` |
| `XDG_DATA_HOME` / `XDG_RUNTIME_DIR` | set by the system | Not this project's variables; they only change the two defaults above for a regular user |

## Docker Compose only

These apply to the repository's own `compose.yaml`; in a compose file you wrote yourself, edit `container_name`, `image` and `volumes` directly.

| Variable | Default | Meaning |
|---|---|---|
| `CONTAINER_NAME` | `iphone-control` | Container name |
| `IMAGE` | `iphone-control:local` | Image; set `ghcr.io/derrors/iphone-control:<version>` for the prebuilt one |
| `DATA_DIR` | `./data` | Data directory; see [Data and Backups](../deploy/backup) |
