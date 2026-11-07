# Contributing

Thanks for helping! Issues and pull requests are welcome in **English or Chinese** (欢迎用中文提 issue 和 PR).

Most of the code comments, design documents and commit messages in this repository are written in Chinese. Follow the language of the code you are editing; English is fine for new files and for commit messages.

## Before you start

- **Bugs:** include the iPhone or iPad model, iOS version, how you run it (Docker, prebuilt binary or source; Linux or macOS), and the output of `iphone-control doctor`, with private data removed. The [issue template](.github/ISSUE_TEMPLATE/bug_report.yml) asks for these.
- **Larger changes:** open an issue first. The key design decisions and their reasons are in [docs/decisions.md](docs/decisions.md); the regression cases every change must keep passing are in [docs/dev/regression-cases.md](docs/dev/regression-cases.md).
- **Security problems:** don't open an issue — see [SECURITY.md](SECURITY.md).

## Building and testing without a phone

You need Rust 1.88 or later (CI pins 1.99.1) and a C compiler. Everything except the device tests runs offline:

```bash
# --all-features also builds the debug-cmds feature, as CI does
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
# vendor/ is upstream code and isn't formatted; check only this repository's packages:
for p in ictl-l10n ictl-device ictl-wda ictl-sign ictl-unlock ictl-daemon ictl-mcp ictl-protocol iphone-control; do cargo fmt -p $p -- --check; done
```

Or, without Rust installed:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1-trixie sh -c \
  'rustup component add clippy && cargo test --workspace --all-features --locked && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings'
```

Behind a firewall add `--network host -e HTTP_PROXY -e HTTPS_PROXY` (`idevice` is a git dependency). `cargo test -p <one crate>` can fail to compile `plume_store` because of feature unification; test the whole workspace instead.

The pure logic — element-tree parsing, occlusion, the auto-unlock rules (against a fake WebDriverAgent), the daemon state machine, MCP argument handling — is covered by unit tests. Real iOS 27 element trees used as fixtures are in [crates/ictl-wda/tests/fixtures](crates/ictl-wda/tests/fixtures).

## Testing on a real phone

Anything that touches the device, timing or WebDriverAgent behaviour needs a real phone. With the daemon running (container or source):

```bash
python3 -m venv .venv && .venv/bin/pip install -r tests/device/requirements.txt
.venv/bin/python tests/device/run.py            # default steps: harmless (Home, Clock tabs, Spotlight search)
.venv/bin/python tests/device/run.py '[["iphone_launch_app", {"app": "设置"}], ["iphone_screenshot", {}]]'
```

`run.py` prints every result with its wall time and saves screenshots to `./shots`. Set `MCP_CMD` for a source install (for example `MCP_CMD="iphone-control mcp"`). Please mention in the pull request what you ran on which device and iOS version.

On macOS, the full checklist (setup, `debug` checks, coexistence with Finder, reconnect) is in [docs/dev/macos-verification.md](docs/dev/macos-verification.md).

Notes:

- Wait until `iphone-control status` says 就绪 (ready) before calling tools after a restart. Restarting the daemon several times in quick succession can make the phone ask for the passcode to enable UI automation again.
- Never run two daemons, or the hidden `iphone-control debug …` diagnostics, against the same phone at once: only one usbmuxd and one XCTest session can own it. `debug` is not compiled by default; build it with `cargo build -p iphone-control --features debug-cmds` or `cargo install --path apps/iphone-control --locked --features debug-cmds`.
- Don't automate anything that sends messages, pays, deletes data or changes account or privacy settings in tests.

### Adding an element-tree fixture

1. `iphone-control wda-bridge --port 18100` (stop it afterwards — WebDriverAgent has no authentication).
2. ```bash
   curl -s "127.0.0.1:18100/source?format=xml&excluded_attributes=visible,accessible,focused,enabled,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame" \
     | python3 -c 'import sys, json; print(json.load(sys.stdin)["value"])' > fixture.xml
   ```
3. **Scrub it:** names, phone numbers, email addresses, locations, notification contents, the device name. Check with `grep -o 'label="[^"]*"' fixture.xml | sort -u`.

## Rules of thumb

- **Pin exact versions** (`=x.y.z`) and keep `Cargo.lock` committed; upgrade dependencies in separate commits ([D9](docs/decisions.md)).
- **Don't break the MCP interface:** tool names, parameters and default outputs stay compatible; add optional parameters instead ([D10](docs/decisions.md)).
- **Vendored code** (`vendor/`) keeps its license. Record any change in [THIRD_PARTY.md](THIRD_PARTY.md); changed MPL-2.0 files stay MPL-2.0.
- **Never commit** `data/`, `.env`, certificates, provisioning profiles, pairing records or anything Apple ships (Developer Disk Images, frameworks).
- Comments explain *why*, especially device quirks; put measured numbers next to timeouts and thresholds.

## Pull requests

- Keep them focused; describe what changed and how you tested it (offline and/or which device).
- CI must pass: format, clippy with `-D warnings`, tests (on Linux and macOS), the MSRV check, gitleaks and the image build.
- User-visible changes get a line in [CHANGELOG.md](CHANGELOG.md) and [CHANGELOG.zh-CN.md](CHANGELOG.zh-CN.md) under *Unreleased* (a Chinese line is welcome but optional for outside contributors; maintainers add it before release).

## Documentation site

The user guide at [derrors.github.io/iphone-control](https://derrors.github.io/iphone-control/) is a VitePress site in [`website/`](website/) (Chinese at the root, English under `en/`; keep both in step). Preview it with `cd website && pnpm install && pnpm docs:dev`. [`docs.yml`](.github/workflows/docs.yml) builds it on pull requests and pushes to `main` that change `website/` (dead links fail the build) and deploys it from `main`. Changes elsewhere, including `docs/`, don't trigger it.

## Releasing (maintainers)

1. Move the *Unreleased* entries in [CHANGELOG.md](CHANGELOG.md) and [CHANGELOG.zh-CN.md](CHANGELOG.zh-CN.md) under `## [x.y.z] - YYYY-MM-DD` and update the compare links at the bottom. Both files need the section — the release notes are built from the two (English, then Chinese) and the workflow fails if either is missing. Keep entries short: one line per user-visible change, no implementation details.
2. Set `version` in the root `Cargo.toml` (`[workspace.package]`) and run `cargo check --workspace` to update `Cargo.lock`.
3. Update `V=vx.y.z` in `README.md`, `README.zh-CN.md`, `website/deploy/macos.md` and `website/en/deploy/macos.md`, and the pinned image examples (`:x.y.z`) in `website/deploy/docker.md` and `website/en/deploy/docker.md`.
4. Commit, tag `vx.y.z`, push the tag. [`release.yml`](.github/workflows/release.yml) builds the amd64/arm64 images on native runners and pushes them to `ghcr.io/derrors/iphone-control` (tags `x.y.z`, `x.y`, `latest`; pre-releases such as `v0.2.0-rc.1` only get their own tag), builds the Linux binaries (`iphone-control-vx.y.z-linux-x86_64.tar.gz`, `…-linux-aarch64.tar.gz`), and creates the GitHub release from the two changelog sections. The macOS binary (`…-macos-arm64.tar.gz`) is built afterwards and uploaded to the release; if GitHub has no macOS runner available the job is cancelled after about 15 minutes — re-run just that job.
5. Update Glama: on glama.ai, the build steps under this server's *Admin → Dockerfile* pin the version. Change it and rebuild.
6. First release only: make the ghcr package public (*Package settings → Change visibility*) — new packages start private.

By contributing you agree that your contributions are licensed under the MIT license, except for changes to vendored files, which stay under their original licenses.
