//! `iphone-control doctor`：逐层诊断，每项失败都给出修复办法。

use std::time::Duration;

use crate::core::{Daemon, DeviceProfile, Inner};

impl Daemon {
    pub(crate) async fn doctor(&self, g: &mut Inner) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut fails = 0;

        out.push(ictl_l10n::tr!("== 设备连接", "== Device connection").into());
        match ictl_device::list_devices().await {
            Ok(_) => out.push(ictl_l10n::tr!("✅ usbmuxd 在运行", "✅ usbmuxd is running").into()),
            Err(e) => {
                out.push(ictl_l10n::trf!("❌ 连不上 usbmuxd：{e}\n     → {}", "❌ Can't reach usbmuxd: {e}\n     → {}",
                    if self.cfg.manage_usbmuxd {
                        ictl_l10n::tr!("由守护进程自动拉起，看数据目录 logs/usbmuxd.log；宿主机已有 usbmuxd 服务时设 ICTL_MANAGE_USBMUXD=0", "the daemon starts it itself, see logs/usbmuxd.log in the data directory; if the host already runs a usbmuxd service, set ICTL_MANAGE_USBMUXD=0")
                    } else if cfg!(target_os = "macos") {
                        ictl_l10n::tr!("macOS 自带 usbmuxd，应一直在运行：sudo launchctl kickstart -k system/com.apple.usbmuxd", "macOS ships usbmuxd and it should always be running: sudo launchctl kickstart -k system/com.apple.usbmuxd")
                    } else {
                        ictl_l10n::tr!("用的是系统的 usbmuxd 服务（插上手机时由 udev 拉起）：确认已安装 usbmuxd、手机已插好", "using the system usbmuxd service (udev starts it when the phone is plugged in): make sure usbmuxd is installed and the phone is plugged in")
                    }
                ));
                fails += 1;
            }
        }
        // 判断不了（不是 Linux/macOS）就不报，免得误报「没有设备」
        match ictl_device::host_ios_usb_present().await {
            Some(true) => out.push(ictl_l10n::tr!("✅ USB 上有 iPhone/iPad", "✅ An iPhone/iPad is on USB").into()),
            Some(false) => {
                out.push(ictl_l10n::tr!("❌ USB 上没有 iPhone/iPad\n     → 插好数据线;用 USB-C 转接头的话换一根试试", "❌ No iPhone/iPad on USB\n     → check the data cable; if you use a USB-C adapter, try another cable").into());
                fails += 1;
            }
            None => {}
        }
        match (&g.device, &g.info) {
            (Some(d), info) => {
                let i = info.clone().unwrap_or_default();
                out.push(ictl_l10n::trf!(
                    "✅ 已连接 {} {} iOS {}",
                    "✅ Connected {} {} iOS {}",
                    d.udid,
                    i.product_type,
                    i.product_version
                ));
                let major = i.major_version();
                if major > 0 && major < 17 {
                    out.push(
                        ictl_l10n::tr!(
                            "⚠️  iOS 低于 17  未在此版本测试过",
                            "⚠️  iOS below 17  not tested on this version"
                        )
                        .into(),
                    );
                } else if major > 0 && major < 27 {
                    out.push(
                        ictl_l10n::tr!(
                            "⚠️  iOS 低于 27  HID 后备路线(免签名截图/点击)不可用,只能走 WDA",
                            "⚠️  iOS below 27  no HID fallback (screenshots/taps without signing), WDA only"
                        )
                        .into(),
                    );
                }
            }
            // 选定的那台没接、别的接着：usbmuxd 没问题，不是重启它能解决的
            (None, _) if self.pinned(g).is_some() && !g.connected.is_empty() => {
                let p = self.pinned(g).unwrap_or_default();
                let how = if self.cfg.udid.is_some() {
                    ictl_l10n::tr!("或改 .env 里的 IPHONE_UDID", "or change IPHONE_UDID in .env")
                } else {
                    ictl_l10n::tr!(
                        "或 iphone-control select 换成接着的另一台(auto 改回自动选)",
                        "or switch to another connected device with iphone-control select (auto for automatic)"
                    )
                };
                out.push(ictl_l10n::trf!(
                    "❌ 选定的设备 {p} 没连接\n     → 接上它;{how}",
                    "❌ The selected device {p} is not connected\n     → plug it in; {how}"
                ));
                fails += 1;
            }
            (None, _) => {
                out.push(
                    if self.cfg.manage_usbmuxd {
                        ictl_l10n::tr!("❌ 守护进程看不到手机\n     → 等 30 秒,会自动重启 usbmuxd;仍不行就重启容器(或守护进程)", "❌ The daemon can't see the phone\n     → wait 30 seconds, usbmuxd restarts automatically; if that doesn't help, restart the container (or the daemon)")
                    } else if cfg!(target_os = "macos") {
                        ictl_l10n::tr!("❌ 守护进程看不到手机\n     → 重新插拔数据线;仍不行就 sudo launchctl kickstart -k system/com.apple.usbmuxd", "❌ The daemon can't see the phone\n     → replug the cable; if that doesn't help, sudo launchctl kickstart -k system/com.apple.usbmuxd")
                    } else {
                        ictl_l10n::tr!("❌ 守护进程看不到手机\n     → 重新插拔数据线;仍不行就 sudo systemctl restart usbmuxd", "❌ The daemon can't see the phone\n     → replug the cable; if that doesn't help, sudo systemctl restart usbmuxd")
                    }
                    .into(),
                );
                fails += 1;
            }
        }
        out.push(ictl_l10n::trf!("   状态：{}", "   State: {}", g.state.describe()));
        if let Some(l) = self.devices_line(g) {
            out.push(format!("   {l}"));
        }

        out.push(ictl_l10n::tr!("== Apple ID 与签名", "== Apple ID and signing").into());
        match self.account().selected_email().await {
            Ok(_) => out.push(ictl_l10n::tr!("✅ 已登录 Apple ID", "✅ Signed in to an Apple ID").into()),
            Err(_) => {
                out.push(
                    ictl_l10n::tr!(
                        "❌ 没有登录 Apple ID\n     → iphone-control login",
                        "❌ Not signed in to an Apple ID\n     → iphone-control login"
                    )
                    .into(),
                );
                fails += 1;
            }
        }
        match &g.persisted.wda_bundle {
            Some(b) => {
                out.push(ictl_l10n::trf!("✅ WDA 已签名 {b}", "✅ WDA signed {b}"));
                // 接着手机时看手机上装的那份：最近一次签名可能是给别的手机签的
                if g.device.is_some() {
                    self.refresh_device_profile(g).await;
                }
                let whose = match &g.device_profile {
                    DeviceProfile::Expires(_) => {
                        ictl_l10n::tr!("这台手机上的描述文件", "The provisioning profile on this phone")
                    }
                    DeviceProfile::Unknown(e) => {
                        out.push(ictl_l10n::trf!("⚠️  读不到手机上的描述文件({e}),下面按上次签名的记录", "⚠️  Can't read the provisioning profiles on the phone ({e}), going by the last signing record"));
                        ictl_l10n::tr!("描述文件", "The provisioning profile")
                    }
                    _ => ictl_l10n::tr!("描述文件", "The provisioning profile"),
                };
                match g.device_profile.left(&g.persisted) {
                    _ if g.device_profile == DeviceProfile::Missing => {
                        out.push(
                            ictl_l10n::tr!("❌ 这台手机上没有装当前 Apple ID 签的 WDA\n     → iphone-control setup(会把这台手机注册到 Apple ID 并装上 WDA)", "❌ No WDA signed by the current Apple ID on this phone\n     → iphone-control setup (registers this phone with the Apple ID and installs WDA)")
                                .into(),
                        );
                        fails += 1;
                    }
                    Some(left) if left.is_zero() => {
                        out.push(ictl_l10n::trf!(
                            "❌ {whose}已过期\n     → iphone-control resign --force",
                            "❌ {whose} has expired\n     → iphone-control resign --force"
                        ));
                        fails += 1;
                    }
                    Some(left) if left < Duration::from_secs(48 * 3600) => {
                        out.push(ictl_l10n::trf!(
                            "⚠️  {whose}只剩 {} 小时  守护进程会自动重签",
                            "⚠️  {whose} has only {} hours left  the daemon re-signs automatically",
                            left.as_secs() / 3600
                        ));
                    }
                    Some(left) => out.push(ictl_l10n::trf!(
                        "✅ {whose}有效  还剩 {} 天",
                        "✅ {whose} is valid  {} days left",
                        left.as_secs() / 86400
                    )),
                    None => out.push(
                        ictl_l10n::tr!(
                            "⚠️  不知道描述文件什么时候到期",
                            "⚠️  Unknown when the provisioning profile expires"
                        )
                        .into(),
                    ),
                }
            }
            None => {
                out.push(
                    ictl_l10n::tr!(
                        "❌ WDA 还没签名安装\n     → iphone-control sign",
                        "❌ WDA not signed and installed yet\n     → iphone-control sign"
                    )
                    .into(),
                );
                fails += 1;
            }
        }
        if let Some(r) = &g.persisted.last_resign {
            out.push(ictl_l10n::trf!("   上次重签：{r}", "   Last re-sign: {r}"));
        }

        out.push("== WDA".into());
        match self.wda(g) {
            Some(w) if w.ready().await.unwrap_or(false) => out.push(
                ictl_l10n::tr!(
                    "✅ WDA 就绪（经 usbmuxd 直连；守护进程不监听端口，wda-bridge 运行时除外）",
                    "✅ WDA ready (directly over usbmuxd; the daemon listens on no port, except while wda-bridge runs)"
                )
                .into(),
            ),
            _ => {
                out.push(ictl_l10n::trf!("❌ WDA 不可用：{}", "❌ WDA unavailable: {}", g.state.describe()));
                fails += 1;
            }
        }

        out.push(ictl_l10n::tr!("== 附加功能", "== Extras").into());
        if self.cfg.notify.any() {
            out.push(ictl_l10n::tr!("✅ 通知已配置", "✅ Notifications configured").into());
        } else {
            out.push(ictl_l10n::tr!("⚠️  没有配置通知  重签失败、手机离线、需要输密码时不会提醒你", "⚠️  No notifications configured  you won't be alerted when re-signing fails, the phone goes offline or a passcode is needed").into());
        }
        let unlock = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        if self.cfg.legacy_passcode_file().is_file() {
            out.push(
                ictl_l10n::tr!("⚠️  有一份旧版本保存的锁屏密码,没记是哪台设备的;同时接着几台时不会用它\n     \
→ 在要用的设备连着时运行 iphone-control passcode set(USB 上只接着一台时会自动归到那台名下)", "⚠️  There is a passcode saved by an older version without a device; it isn't used while several devices are connected\n     → run iphone-control passcode set while the device you want is connected (with only one device on USB it is assigned to it automatically)")
                    .into(),
            );
        }
        if unlock.disabled_by_failure() {
            out.push(
                ictl_l10n::tr!("❌ 自动解锁已停用(之前输错过或输入中途出错)\n     → 核对密码后 iphone-control unlock-reset", "❌ Auto-unlock is disabled (a wrong passcode or an error while entering it)\n     → check the passcode, then iphone-control unlock-reset").into(),
            );
            fails += 1;
        } else if unlock.enabled() {
            out.push(ictl_l10n::tr!("✅ 自动解锁已启用(这台设备)", "✅ Auto-unlock enabled (this device)").into());
        } else if g.device.is_some() {
            out.push(ictl_l10n::tr!("⚠️  这台设备没设自动解锁密码  锁屏时需要人工解锁(可选:iphone-control passcode set)", "⚠️  No auto-unlock passcode for this device  unlock it by hand when locked (optional: iphone-control passcode set)").into());
        } else {
            out.push(
                ictl_l10n::tr!("⚠️  没接设备,看不了自动解锁", "⚠️  No device connected, can't check auto-unlock")
                    .into(),
            );
        }
        out.push(String::new());
        out.push(if fails == 0 {
            ictl_l10n::tr!("全部正常", "All good").into()
        } else {
            ictl_l10n::trf!("{fails} 项需要处理", "{fails} item(s) need attention")
        });
        out.join("\n")
    }
}
