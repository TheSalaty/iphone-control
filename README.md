<div align="center">
  <a href="https://github.com/Derrors/iphone-control">
    <img src="docs/images/banner.svg" alt="iphone-control: Let AI agents operate a real iPhone" width="100%">
  </a>

  <p align="center">
    Let an AI agent operate a real iPhone or iPad plugged into a Linux machine or a Mac — one Rust binary, no Xcode, no paid developer account, no jailbreak.
  </p>

  <p align="center">
    <a href="https://github.com/Derrors/iphone-control/releases/latest"><img src="https://img.shields.io/github/v/release/Derrors/iphone-control?style=flat" alt="Release"></a>
    <img src="https://img.shields.io/badge/iOS-17%2B-black?style=flat&logo=apple" alt="iOS">
    <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS-0E7490?style=flat" alt="Platform: Linux | macOS">
    <img src="https://img.shields.io/badge/MCP-17%20tools-8A2BE2?style=flat" alt="MCP">
    <a href="Cargo.toml"><img src="https://img.shields.io/badge/Rust-1.88%2B-CE422B?style=flat&logo=rust&logoColor=white" alt="Rust"></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/Derrors/iphone-control?style=flat" alt="License"></a>
    <a href="https://m8ven.ai/mcp/derrors-iphone-control-1pcmse?s=readme"><img src="https://m8ven.ai/badge/mcp/derrors-iphone-control-1pcmse?variant=verified" alt="M8ven verified"></a>
  </p>

  <p align="center">
    <a href="#quick-start"><strong>🚀 Quick Start</strong></a>
    &nbsp;·&nbsp;
    <a href="https://derrors.github.io/iphone-control/en/"><strong>📖 Docs</strong></a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/releases">📦 Releases</a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/issues">🐛 Issues</a>
    &nbsp;·&nbsp;
    <a href="README.zh-CN.md">🌐 简体中文</a>
  </p>

  <br>
  <img src="docs/images/demo-stopwatch.webp" alt="Claude Code on the left calls iphone_launch_app and iphone_tap_element; on the right, the iPhone shows the Clock stopwatch running" width="900">
  <p><sub>Asking Claude Code to “open Clock, switch to the stopwatch and start it”. Left: Claude Code’s tool calls (first part only). Right: the phone afterwards.</sub></p>
</div>

## Overview

AI agents can drive browsers and desktops, but a real iPhone is harder: Apple's automation tools need a Mac and Xcode, and the open-source alternatives come as separate pieces that each need their own setup, signing and babysitting.

iphone-control does it all in **one Rust binary** on Linux or macOS, built on [idevice](https://github.com/jkcoxson/idevice), [Impactor](https://github.com/claration/impactor) and [WebDriverAgent](https://github.com/appium/WebDriverAgent). A long-running daemon owns the phone — tunnel, HID session, WebDriverAgent session — and exposes it to Claude Code or any MCP client. It succeeds [iphone-control-toolkit](https://github.com/Derrors/iphone-control-toolkit) and keeps its MCP tools compatible.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="website/public/architecture-dark.svg">
    <img src="website/public/architecture-light.svg" alt="Architecture: an MCP client talks to iphone-control mcp over stdio, which talks to the daemon over a Unix socket; the daemon drives the iPhone or iPad over USB through WebDriverAgent, with CoreDevice HID as fallback" width="900">
  </picture>
</p>

## Features

<p align="center">
  <img src="docs/images/features.svg" alt="One binary, 17 MCP tools, fewer round trips, runs unattended, HID fallback, doctor and alerts" width="900">
</p>

- **See and touch the screen**: screenshots with region zoom, tap, swipe, long-press, text input (including Chinese), hardware buttons
- **Work with UI elements**: read the element tree, tap by text (on a part not covered by floating bars or the keyboard), launch and close apps, open URLs, wait for text to appear
- **Fewer round trips**: any action can return the resulting screen once it has settled, flagging pages still loading and blocking alerts; `iphone_batch` runs up to 20 steps in one call
- **Automatic fallback**: when WebDriverAgent is down, screenshots, taps, swipes and keys switch to Apple's CoreDevice HID service (iOS 27+)
- **Runs unattended**: recovers from replugging, phone reboots and WebDriverAgent crashes; re-signs WebDriverAgent with a free Apple ID before the 7-day profile expires
- **Alerts**: Telegram or Bark notifications when re-signing fails, the Apple ID session expires, the phone goes offline, or a passcode is needed after a reboot
- **Optional auto-unlock**: every passcode digit is verified, and a wrong passcode disables auto-unlock after one attempt
- **Diagnostics**: `doctor` checks every layer from USB to WebDriverAgent and tells you how to fix each failure

## Quick Start

You need an iPhone or iPad on iOS 17+ connected by a **data** cable, a free Apple ID (use a dedicated one), and a Linux host with Docker or an Apple Silicon Mac.

**Linux (Docker)**

```bash
git clone https://github.com/Derrors/iphone-control.git && cd iphone-control
cp .env.example .env                                  # optional: proxy, notifications
docker compose up -d --build                          # the first build takes a few minutes
docker exec -it iphone-control iphone-control setup   # guided setup; safe to re-run
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

**macOS** (Docker can't pass USB through on a Mac, so run the binary directly)

```bash
V=v0.1.4   # latest release
curl -fsSL https://github.com/Derrors/iphone-control/releases/download/$V/iphone-control-$V-macos-arm64.tar.gz | tar xz
mkdir -p ~/.local/bin && install -m 755 iphone-control-$V-macos-arm64/iphone-control ~/.local/bin/
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile && source ~/.zprofile
iphone-control daemon       # keep this terminal open
iphone-control setup        # in a second terminal
iphone-control mcp-config   # prints the `claude mcp add` command to run
```

`setup` walks you through pairing, Developer Mode, Apple ID sign-in and installing WebDriverAgent; do what it asks on the phone. Then open a new Claude Code session and ask, for example, *“Open Clock, switch to the stopwatch and start it.”*

More: [step-by-step guide](https://derrors.github.io/iphone-control/en/guide/getting-started) · [prebuilt image](https://derrors.github.io/iphone-control/en/deploy/docker) · [binary / source](https://derrors.github.io/iphone-control/en/deploy/source) · [macOS guide](https://derrors.github.io/iphone-control/en/deploy/macos) · [other MCP clients](https://derrors.github.io/iphone-control/en/guide/mcp-clients).

## MCP Tools

| Tool | Description |
| --- | --- |
| `iphone_status` | Connection, lock state, foreground app, screen size |
| `iphone_screenshot` | Screenshot scaled to points; pass `x/y/width/height` to zoom into a region |
| `iphone_list_elements` | Elements with text and their center coordinates; filter by text or type |
| `iphone_tap` / `iphone_tap_element` | Tap by coordinates or by element text |
| `iphone_swipe` / `iphone_long_press` | Swipe (returns once the list stops), long-press |
| `iphone_type_text` | Type into the focused field; reads it back and fixes dropped characters |
| `iphone_press_button` | `home`, `volume_up`, `volume_down`, `lock` |
| `iphone_list_apps` / `iphone_launch_app` / `iphone_terminate_app` | Manage apps by name or bundle ID |
| `iphone_open_url` | Open a URL or URL scheme |
| `iphone_wait_for` | Wait until some text appears instead of polling with screenshots |
| `iphone_batch` | Run up to 20 steps in one call, stopping at the first error |
| `iphone_list_devices` | Every iPhone/iPad on USB; marks the one being controlled |
| `iphone_select_device` | Switch to another device, or back to automatic choice |

Action tools accept `then: "elements" | "screenshot"` to return the screen after the action. Screenshot pixels are tap coordinates, so the agent can tap what it sees. Games, maps and video players draw their own UI and have no element tree — use screenshots and coordinates there. Full parameters: [docs/tools.md](docs/tools.md).

Tool texts, results, CLI output and logs are in Simplified Chinese by default; set `IPHONE_CONTROL_LANG=en` for English ([configuration](https://derrors.github.io/iphone-control/en/guide/configuration)).

## Everyday Commands

Run them as `docker exec -it iphone-control iphone-control <command>`, or directly for a source install and on macOS.

| Command | What it does |
| --- | --- |
| `daemon` | The long-running daemon: the container's entry point; on a source install or macOS you start it yourself |
| `mcp` | The MCP server; your AI client starts it |
| `status` | Daemon state, time left on the signing profile, recent log |
| `doctor` | Layer-by-layer health check with a fix for each failure |
| `setup` | Guided setup; re-run any time |
| `login` | Sign in to the Apple ID again when the session expires |
| `sign` | Sign and install WebDriverAgent now |
| `resign [--force]` | Re-sign WebDriverAgent if it expires soon (or now) |
| `passcode set\|clear` | Turn auto-unlock on or off for a device (passcodes are kept per device) |
| `unlock-reset` | Turn auto-unlock back on after it was disabled |
| `devices` / `select <UDID\|name\|auto>` | List connected devices / switch which one is controlled |
| `notify-test [event]` | Send test notifications |
| `mcp-config` | Print the command that registers the MCP server |
| `wda-bridge [--port]` | Expose WebDriverAgent on 127.0.0.1 for other tools; stop it when done |

All commands and options: [CLI reference](https://derrors.github.io/iphone-control/en/guide/cli).

Upgrade with `git pull && docker compose up -d --build` (or `docker compose pull && docker compose up -d --no-build` for the prebuilt image); for the binary and macOS, see [Source / binary](https://derrors.github.io/iphone-control/en/deploy/source#upgrade) and [macOS](https://derrors.github.io/iphone-control/en/deploy/macos#upgrade). Read the upgrade notes in the [changelog](CHANGELOG.md) first. Settings live in `.env` — see [configuration](https://derrors.github.io/iphone-control/en/guide/configuration). All state lives in `data/`: pairing records, the Apple ID session and signing key, and the passcode if auto-unlock is on. **Back it up; never commit or share it.**

## Documentation

| | |
| --- | --- |
| Guide | [What is it](https://derrors.github.io/iphone-control/en/guide/what-is-iphone-control) · [Limitations](https://derrors.github.io/iphone-control/en/guide/what-is-iphone-control#limitations) · [Getting started](https://derrors.github.io/iphone-control/en/guide/getting-started) · [AI clients](https://derrors.github.io/iphone-control/en/guide/mcp-clients) · [Tools](https://derrors.github.io/iphone-control/en/guide/tools) |
| Usage | [Configuration](https://derrors.github.io/iphone-control/en/guide/configuration) · [CLI](https://derrors.github.io/iphone-control/en/guide/cli) · [Several devices](https://derrors.github.io/iphone-control/en/guide/multi-device) · [Notifications](https://derrors.github.io/iphone-control/en/guide/notifications) · [Auto-unlock](https://derrors.github.io/iphone-control/en/guide/auto-unlock) |
| Deployment | [Docker](https://derrors.github.io/iphone-control/en/deploy/docker) · [Source / binary](https://derrors.github.io/iphone-control/en/deploy/source) · [macOS](https://derrors.github.io/iphone-control/en/deploy/macos) · [Data and backups](https://derrors.github.io/iphone-control/en/deploy/backup) |
| Problems | [FAQ](https://derrors.github.io/iphone-control/en/troubleshoot/faq) · [Troubleshooting](docs/troubleshooting.md) · [Issues](https://github.com/Derrors/iphone-control/issues) |
| Security | [SECURITY.md](SECURITY.md): what is stored, who can reach it, reporting vulnerabilities |
| Development | [Contributing](CONTRIBUTING.md) · [Architecture](docs/architecture.md) · [Decisions](docs/decisions.md) · [Changelog](CHANGELOG.md) |
| Licensing | [THIRD_PARTY.md](THIRD_PARTY.md): vendored components, versions and licenses |

The architecture and design documents are in Chinese for now.

> [!WARNING]
> This tool can fully control your phone. An agent may tap payment, delete or send buttons — keep tool-call confirmation on in your MCP client. Not affiliated with Apple; use it only with devices and accounts you own. Read [SECURITY.md](SECURITY.md) before enabling auto-unlock.

## Contributing

Issues and pull requests are welcome — see the [contributing guide](CONTRIBUTING.md).

## License

[MIT](LICENSE). Third-party components: [THIRD_PARTY.md](THIRD_PARTY.md).
