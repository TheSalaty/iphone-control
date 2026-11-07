# 配置

全部通过环境变量配置，都可以不填：

- **Docker**：写在仓库目录的 `.env`（从 `.env.example` 复制），改完 `docker compose up -d` 重建容器。
- **只写 compose 文件**：写在 `compose.yaml` 的 `environment:` 里（或同目录的 `.env` 加 `env_file`），改完同样 `docker compose up -d`。
- **从源码运行**：写在 `~/.config/iphone-control/env`（systemd 用户服务会读），改完 `systemctl --user restart iphone-control`。
- **macOS**：同样写在 `~/.config/iphone-control/env`，启动守护进程前加载，改完重启守护进程，见 [macOS](../deploy/macos#配置)。

## 常用

| 变量 | 默认 | 含义 |
|---|---|---|
| `HTTP_PROXY` / `HTTPS_PROXY` | 直连 | 访问苹果、anisette、开发者镜像、推送通知用的代理 |
| `IPHONE_CONTROL_LANG` | `zh` | 界面语言:`zh` 或 `en`。工具说明和结果、命令行输出、日志、通知都按它。MCP 和命令行各自按自己的设置请求,守护进程按请求的语言回答;从源码运行时要在 MCP 配置里也设上(`iphone-control mcp-config` 会带上) |
| `IPHONE_UDID` | 自动选 | 固定控制哪台设备。不设时自动选：正在控制的那台一直用到它断开；运行中可以用 `select` 命令或 `iphone_select_device` 工具切换。设了就不能切换 |
| `RESIGN_MIN_DAYS` | `2` | 描述文件剩余不到几天时重签 |
| `RESIGN_CHECK_HOURS` | `6` | 每隔几小时检查一次；手机重新连上或切换设备后 1 分钟内也查一次 |
| `OFFLINE_ALERT_MIN` | `10` | 手机离线超过几分钟推送提醒，`0` 不提醒 |

通知相关的变量见[通知](./notifications)。

## 高级

| 变量 | 含义 |
|---|---|
| `ICTL_ANISETTE_URL` | 自建 anisette 服务地址（默认用公共的第三方服务，见[安全与隐私](./security)） |
| `ICTL_DDI_URL` | 开发者镜像的下载地址，默认社区维护的镜像仓库 |
| `ICTL_DDI_DIR` | 自己从 Xcode 提取的开发者镜像目录。设了就不下载，也不会删里面的文件；缺文件时报错说明缺哪个。不设时用数据目录下的 `ddi/` 缓存，连续 3 次挂载失败后清掉重新下载 |
| `ICTL_MANAGE_USBMUXD` | 守护进程是否自己拉起 usbmuxd。默认：以 root 运行（容器）时拉起，普通用户不拉起，用系统的。`0` 或 `false` 不拉起，例如以 root 运行、宿主机又已有 usbmuxd 服务时 |
| `ICTL_USBMUXD_BIN` | 守护进程自己拉起 usbmuxd 时用的可执行文件，默认 `usbmuxd` |
| `ICTL_WDA_UNSIGNED` | 未签名 WDA 的路径。默认：镜像里的 `/usr/share/iphone-control/WDA-unsigned.ipa`；没有镜像（从源码运行）时是数据目录的 `wda/WDA-unsigned.ipa`，第一次签名前下载。自己指定的路径不会下载 |
| `SCREEN_W` / `SCREEN_H` | HID 后备路线换算坐标用的屏幕尺寸（点）：WDA 不可用、又没有这台设备的尺寸记录时才用，比如 WDA 从没连上过，或切到记录里没有尺寸的设备。不分设备，接多台时要小心 |
| `ICTL_LOG` | 日志级别，例如 `info,ictl_wda::http=debug` 记录每个 WDA 请求的耗时 |

## 目录和控制接口

一般不用改，Docker 部署不要改（容器里 `ICTL_DATA_DIR` 已经固定，数据卷挂在那里）。要改时不能写成空值，守护进程、命令行和 MCP 要用同样的值：除了容器，只有守护进程会读配置文件，见[从源码 / 二进制](../deploy/source#运行)。

| 变量 | 默认 | 含义 |
|---|---|---|
| `ICTL_DATA_DIR` | root：`/var/lib/iphone-control`<br>普通用户：`$XDG_DATA_HOME/iphone-control`，没设时 `~/.local/share/iphone-control` | 数据目录，同命令行的 `--data-dir` |
| `ICTL_SOCKET` | root：`/run/iphone-control/ctl.sock`<br>普通用户：`$XDG_RUNTIME_DIR/iphone-control/ctl.sock`，没有这个变量时（macOS）放在数据目录里 | 守护进程的控制接口，同命令行的 `--socket` |
| `XDG_DATA_HOME` / `XDG_RUNTIME_DIR` | 系统设置 | 不是本项目的变量，只影响普通用户时上面两项的默认位置 |

## 只给 Docker Compose 用的

用仓库自带的 `compose.yaml` 时才有这几项；自己写 compose 文件时直接改对应的 `container_name`、`image`、`volumes`。

| 变量 | 默认 | 含义 |
|---|---|---|
| `CONTAINER_NAME` | `iphone-control` | 容器名 |
| `IMAGE` | `iphone-control:local` | 镜像名；用预构建镜像时改成 `ghcr.io/derrors/iphone-control:<版本>` |
| `DATA_DIR` | `./data` | 数据目录，见[数据与备份](../deploy/backup) |
