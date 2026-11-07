<div align="center">
  <a href="https://github.com/Derrors/iphone-control">
    <img src="docs/images/banner.zh-CN.svg" alt="iphone-control：让 AI agent 操作一台真实的 iPhone" width="100%">
  </a>

  <p align="center">
    让 AI agent 操作一台接在 Linux 或 Mac 上的真实 iPhone 或 iPad：一个 Rust 二进制，不需要 Xcode，不需要付费开发者账号，不越狱。
  </p>

  <p align="center">
    <a href="https://github.com/Derrors/iphone-control/releases/latest"><img src="https://img.shields.io/github/v/release/Derrors/iphone-control?style=flat" alt="Release"></a>
    <img src="https://img.shields.io/badge/iOS-17%2B-black?style=flat&logo=apple" alt="iOS">
    <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS-0E7490?style=flat" alt="平台：Linux | macOS">
    <img src="https://img.shields.io/badge/MCP-17%20tools-8A2BE2?style=flat" alt="MCP">
    <a href="Cargo.toml"><img src="https://img.shields.io/badge/Rust-1.88%2B-CE422B?style=flat&logo=rust&logoColor=white" alt="Rust"></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/Derrors/iphone-control?style=flat" alt="License"></a>
    <a href="https://m8ven.ai/mcp/derrors-iphone-control-1pcmse?s=readme"><img src="https://m8ven.ai/badge/mcp/derrors-iphone-control-1pcmse?variant=verified" alt="M8ven 已验证"></a>
  </p>

  <p align="center">
    <a href="#快速开始"><strong>🚀 快速开始</strong></a>
    &nbsp;·&nbsp;
    <a href="https://derrors.github.io/iphone-control/"><strong>📖 文档</strong></a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/releases">📦 版本发布</a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/issues">🐛 问题反馈</a>
    &nbsp;·&nbsp;
    <a href="README.md">🌐 English</a>
  </p>

  <br>
  <img src="docs/images/demo-stopwatch.webp" alt="左边 Claude Code 调用 iphone_launch_app 和 iphone_tap_element，右边 iPhone 上时钟的秒表正在计时" width="900">
  <p><sub>让 Claude Code「打开时钟，切到秒表并启动」。左边是 Claude Code 的工具调用（只截了前半段），右边是完成后的手机。</sub></p>
</div>

## 简介

AI agent 已经能操作浏览器和电脑桌面，但真实的 iPhone 难得多：苹果的自动化工具需要 Mac 和 Xcode，开源替代品又是零散的几块，每一块都要单独安装、签名、看护。

iphone-control 在 Linux 或 macOS 上用**一个 Rust 二进制**把这些事都做了，基于 [idevice](https://github.com/jkcoxson/idevice)、[Impactor](https://github.com/claration/impactor) 和 [WebDriverAgent](https://github.com/appium/WebDriverAgent)。一个常驻守护进程独占手机（隧道、HID 会话、WebDriverAgent 会话常驻复用），把它交给 Claude Code 或任何 MCP 客户端。它是 [iphone-control-toolkit](https://github.com/Derrors/iphone-control-toolkit) 的继任者，MCP 工具保持兼容。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="website/public/architecture-dark.svg">
    <img src="website/public/architecture-light.svg" alt="架构：MCP 客户端经 stdio 调用 iphone-control mcp，再经 Unix socket 交给守护进程；守护进程经 USB 用 WebDriverAgent 操作 iPhone 或 iPad，CoreDevice HID 作为后备" width="900">
  </picture>
</p>

## 功能特性

<p align="center">
  <img src="docs/images/features.zh-CN.svg" alt="一个二进制、17 个 MCP 工具、少走往返、无人值守、HID 后备、诊断与通知" width="900">
</p>

- **看屏幕、点屏幕**：截图（可放大某块区域）、点击、滑动、长按、输入文字（支持中文）、硬件按键
- **按界面元素操作**：读元素树、按文字点击（自动避开悬浮栏、键盘挡住的部分）、启动和结束 App、打开链接、等某段文字出现
- **省往返**：任何操作都可以等界面稳定后顺带返回操作后的样子，并提示页面是否还在加载、有没有弹窗挡着；`iphone_batch` 一次执行最多 20 步
- **自动后备**：WebDriverAgent 不可用时，截图、点击、滑动、按键改走苹果的 CoreDevice HID 服务（iOS 27 起）
- **无人值守**：拔插、手机重启、WebDriverAgent 崩溃后自动恢复；用免费 Apple ID 签名，7 天描述文件过期前自动重签
- **告警通知**：重签失败、Apple ID 会话失效、手机离线、重启后需要输密码时，经 Telegram 或 Bark 推送
- **可选的自动解锁**：每输一位都核对，密码错了只试一次就停用
- **诊断**：`doctor` 从 USB 到 WebDriverAgent 逐层检查，每项失败都告诉你怎么修

## 快速开始

需要一部 iOS 17 以上的 iPhone 或 iPad 和能**传数据**的线、一个免费 Apple ID（建议专门注册一个），以及一台装了 Docker 的 Linux 主机或一台 Apple Silicon 的 Mac。

**Linux（Docker）**

```bash
git clone https://github.com/Derrors/iphone-control.git && cd iphone-control
cp .env.example .env                                  # 可选：代理、通知；国内网络一般要配代理
docker compose up -d --build                          # 首次构建需要几分钟
docker exec -it iphone-control iphone-control setup   # 安装向导，可重复运行
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

**macOS**（Mac 上的 Docker 不能直通 USB，直接运行二进制）

```bash
V=v0.1.4   # 最新版本号
curl -fsSL https://github.com/Derrors/iphone-control/releases/download/$V/iphone-control-$V-macos-arm64.tar.gz | tar xz
mkdir -p ~/.local/bin && install -m 755 iphone-control-$V-macos-arm64/iphone-control ~/.local/bin/
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile && source ~/.zprofile
iphone-control daemon       # 这个终端保持开着
iphone-control setup        # 在另一个终端运行
iphone-control mcp-config   # 打印 `claude mcp add` 命令，复制运行
```

`setup` 会引导完成配对、开发者模式、登录 Apple ID、安装 WebDriverAgent，按提示在手机上操作即可。然后打开一个新的 Claude Code 会话，试着说：「打开时钟，切到秒表并启动」。

更多：[分步指南](https://derrors.github.io/iphone-control/guide/getting-started) · [预构建镜像](https://derrors.github.io/iphone-control/deploy/docker) · [二进制 / 源码](https://derrors.github.io/iphone-control/deploy/source) · [macOS 指南](https://derrors.github.io/iphone-control/deploy/macos) · [其他 MCP 客户端](https://derrors.github.io/iphone-control/guide/mcp-clients)

## MCP 工具

| 工具 | 作用 |
| --- | --- |
| `iphone_status` | 连接状态、是否锁屏、前台 App、屏幕尺寸 |
| `iphone_screenshot` | 截图（单位为点）；给出 `x/y/width/height` 时放大这块区域 |
| `iphone_list_elements` | 带文字的元素和中心点坐标；可按文字或类型筛选 |
| `iphone_tap` / `iphone_tap_element` | 按坐标或按元素文字点击 |
| `iphone_swipe` / `iphone_long_press` | 滑动（列表停稳后返回）、长按 |
| `iphone_type_text` | 在当前输入框输入文字；输完读回核对，丢字会补上 |
| `iphone_press_button` | `home`、`volume_up`、`volume_down`、`lock` |
| `iphone_list_apps` / `iphone_launch_app` / `iphone_terminate_app` | 按名字或 Bundle ID 管理 App |
| `iphone_open_url` | 打开链接或 URL Scheme |
| `iphone_wait_for` | 等某段文字出现，代替反复截图 |
| `iphone_batch` | 一次执行最多 20 步，出错就停 |
| `iphone_list_devices` | 列出 USB 上接着的 iPhone/iPad，标出正在控制哪台 |
| `iphone_select_device` | 改为控制另一台，或改回自动选 |

操作类工具可以带 `then: "elements" | "screenshot"`，顺带返回操作后的界面。截图的像素就是点击坐标，agent 看到哪里就能点哪里。游戏、地图、视频自己绘制界面，没有元素树，只能看截图按坐标操作。完整参数见 [docs/tools.md](docs/tools.md)（英文）。

工具说明和结果、命令行输出、日志默认是简体中文;设 `IPHONE_CONTROL_LANG=en` 改成英文(见[配置](https://derrors.github.io/iphone-control/guide/configuration))。

## 常用命令

用 `docker exec -it iphone-control iphone-control <命令>` 运行；从源码运行和在 macOS 上直接调用。

| 命令 | 作用 |
| --- | --- |
| `daemon` | 常驻守护进程：容器的入口；从源码运行和 macOS 上要自己启动 |
| `mcp` | MCP 服务，由 AI 客户端拉起 |
| `status` | 守护进程状态、描述文件还剩多久、最近日志 |
| `doctor` | 逐层诊断，每项失败都给出修复办法 |
| `setup` | 安装向导，随时可以重新运行 |
| `login` | Apple ID 会话失效时重新登录 |
| `sign` | 立即签名并安装 WebDriverAgent |
| `resign [--force]` | 快过期才重签 WebDriverAgent（或立即重签） |
| `passcode set\|clear` | 开启或关闭一台设备的自动解锁（密码按设备分开存） |
| `unlock-reset` | 自动解锁被停用后恢复 |
| `devices` / `select <UDID\|设备名\|auto>` | 列出接着的设备 / 切换控制哪一台 |
| `notify-test [事件]` | 发送测试通知 |
| `mcp-config` | 打印注册 MCP 的命令 |
| `wda-bridge [--port]` | 把 WebDriverAgent 桥接到 127.0.0.1 给别的工具用，用完就停 |

全部命令和参数见[命令行](https://derrors.github.io/iphone-control/guide/cli)。

升级：`git pull && docker compose up -d --build`（用预构建镜像时改为 `docker compose pull && docker compose up -d --no-build`）；二进制和 macOS 见[从源码 / 二进制](https://derrors.github.io/iphone-control/deploy/source#升级)、[macOS](https://derrors.github.io/iphone-control/deploy/macos#升级)。升级前先看[更新日志](CHANGELOG.zh-CN.md)里的升级注意事项。配置写在 `.env`，见[配置说明](https://derrors.github.io/iphone-control/guide/configuration)。所有状态都在 `data/`：配对记录、Apple ID 会话和签名私钥，开启自动解锁时还有锁屏密码。**要备份，不要提交或分享。**

## 文档

| | |
| --- | --- |
| 入门 | [什么是 iphone-control](https://derrors.github.io/iphone-control/guide/what-is-iphone-control) · [限制](https://derrors.github.io/iphone-control/guide/what-is-iphone-control#限制) · [快速开始](https://derrors.github.io/iphone-control/guide/getting-started) · [接入 AI 客户端](https://derrors.github.io/iphone-control/guide/mcp-clients) · [工具一览](https://derrors.github.io/iphone-control/guide/tools) |
| 使用 | [配置](https://derrors.github.io/iphone-control/guide/configuration) · [命令行](https://derrors.github.io/iphone-control/guide/cli) · [多台设备](https://derrors.github.io/iphone-control/guide/multi-device) · [通知](https://derrors.github.io/iphone-control/guide/notifications) · [自动解锁](https://derrors.github.io/iphone-control/guide/auto-unlock) |
| 安装 | [Docker](https://derrors.github.io/iphone-control/deploy/docker) · [从源码 / 二进制](https://derrors.github.io/iphone-control/deploy/source) · [macOS](https://derrors.github.io/iphone-control/deploy/macos) · [数据与备份](https://derrors.github.io/iphone-control/deploy/backup) |
| 问题 | [常见问题](https://derrors.github.io/iphone-control/troubleshoot/faq) · [排错（英文）](docs/troubleshooting.md) · [Issues](https://github.com/Derrors/iphone-control/issues) |
| 安全 | [SECURITY.md](SECURITY.md)：存了什么、谁能访问、怎么报告漏洞 |
| 开发 | [贡献指南](CONTRIBUTING.md) · [架构](docs/architecture.md) · [关键取舍](docs/decisions.md) · [更新日志](CHANGELOG.zh-CN.md) |
| 许可证 | [THIRD_PARTY.md](THIRD_PARTY.md)：vendor 组件、版本和许可证 |

> [!WARNING]
> 这个工具能完全控制你的手机，agent 可能点到付款、删除、发送这类按钮，请在 MCP 客户端里保持工具调用确认。本项目与 Apple 无关；只在你自己的设备和账号上使用。开启自动解锁前请先读 [SECURITY.md](SECURITY.md)。

## 参与贡献

欢迎通过 [Issues](https://github.com/Derrors/iphone-control/issues) 反馈问题、提交 Pull Request，详见[贡献指南](CONTRIBUTING.md)。

## License

[MIT](LICENSE)，第三方组件见 [THIRD_PARTY.md](THIRD_PARTY.md)。
