//! 接着多台设备时：列出来、选一台控制。
//!
//! 同一时间只控制一台（一个 WDA 会话、一把设备锁）。选哪台见 [`crate::machine::pick_device`]：
//! 选定了就只认那一台；没选定时正在控制的那台一直用到它断开。切换时拆掉上一台的会话，在新的一台上拉起 WDA

use ictl_device::{Device, DeviceInfo, UsbDevice};

use crate::core::{Daemon, Inner, device_label};

/// 按 UDID 或设备名找设备，返回下标：先比 UDID（不分大小写），再比设备名，最后是设备名包含这段文字且只有一台
pub fn resolve_device(devices: &[DeviceInfo], query: &str) -> Result<usize, String> {
    let q = query.trim();
    let listed = || devices.iter().map(device_label).collect::<Vec<_>>().join(ictl_l10n::tr!("、", ", "));
    if devices.is_empty() {
        return Err(ictl_l10n::tr!("USB 上没有接着设备", "No device connected over USB").into());
    }
    if let Some(i) = devices.iter().position(|d| d.udid.eq_ignore_ascii_case(q)) {
        return Ok(i);
    }
    let by_name: Vec<usize> = (0..devices.len()).filter(|&i| devices[i].name == q).collect();
    let hits = if by_name.is_empty() {
        let lq = q.to_lowercase();
        (0..devices.len()).filter(|&i| !lq.is_empty() && devices[i].name.to_lowercase().contains(&lq)).collect()
    } else {
        by_name
    };
    match hits[..] {
        [i] => Ok(i),
        [] => Err(ictl_l10n::trf!("没有接着「{q}」,接着的有 {}", "“{q}” is not connected; connected are {}", listed())),
        _ => Err(ictl_l10n::trf!(
            "「{q}」对上了不止一台:{};用 UDID 指定",
            "“{q}” matches more than one device: {}; use the UDID",
            hits.iter().map(|&i| device_label(&devices[i])).collect::<Vec<_>>().join(ictl_l10n::tr!("、", ", "))
        )),
    }
}

impl Daemon {
    /// 记下这一轮 USB 上接着的设备；新接上的读一次名字、型号
    pub(crate) async fn note_connected(&self, g: &mut Inner, devices: Vec<UsbDevice>) {
        for d in &devices {
            if !g.infos.contains_key(&d.udid)
                && let Ok(i) = Device::from_usb(d).info().await
            {
                g.infos.insert(d.udid.clone(), i);
            }
        }
        g.connected = devices;
    }

    pub(crate) fn list_devices_text(&self, g: &Inner) -> String {
        let infos = self.connected_infos(g);
        if infos.is_empty() {
            return match self.pinned(g) {
                Some(p) => {
                    ictl_l10n::trf!("USB 上没有接着设备(选定的是 {p})", "No device connected over USB (selected: {p})")
                }
                None => ictl_l10n::tr!("USB 上没有接着设备", "No device connected over USB").into(),
            };
        }
        let current = g.device.as_ref().map(|d| d.udid.as_str());
        let mode = match (&self.cfg.udid, self.pinned(g)) {
            (Some(u), _) => ictl_l10n::trf!("IPHONE_UDID 固定为 {u}", "IPHONE_UDID pins {u}"),
            (None, Some(p)) => ictl_l10n::trf!("选定了 {p}", "{p} selected"),
            (None, None) => ictl_l10n::tr!(
                "自动选(正在控制的那台断开才换)",
                "automatic (switches only when the controlled one disconnects)"
            )
            .into(),
        };
        let mut lines = vec![ictl_l10n::trf!("接着 {} 台,{mode}:", "{} connected, {mode}:", infos.len())];
        for i in &infos {
            let mark = if Some(i.udid.as_str()) == current {
                ictl_l10n::tr!("→ 正在控制 ", "→ controlled ")
            } else {
                "  "
            };
            let pin = if self.cfg.passcode_file_for(&i.udid).is_file() {
                ictl_l10n::tr!("已设", "set")
            } else {
                ictl_l10n::tr!("没设", "not set")
            };
            lines.push(ictl_l10n::trf!(
                "{mark}{} 自动解锁密码{pin}",
                "{mark}{} auto-unlock passcode {pin}",
                device_label(i)
            ));
        }
        if self.cfg.udid.is_none() && infos.len() > 1 {
            lines.push(
                ictl_l10n::tr!(
                    "切换:iphone_select_device(UDID 或设备名),auto 改回自动选",
                    "Switch: iphone_select_device (UDID or device name), auto to go back to automatic"
                )
                .into(),
            );
        }
        lines.join("\n")
    }

    /// 改为控制 `query` 指的那台（UDID、设备名或 `auto`），选定的写进 state.json，重启后还记得
    pub(crate) async fn select_device(&self, g: &mut Inner, query: &str) -> Result<String, String> {
        if let Ok(d) = ictl_device::list_devices().await {
            self.note_connected(g, d).await;
        }
        let q = query.trim();
        if q.eq_ignore_ascii_case("auto") || q == ictl_l10n::tr!("自动", "automatic") {
            if let Some(u) = &self.cfg.udid {
                return Err(ictl_l10n::trf!(
                    "IPHONE_UDID 固定为 {u};要改回自动选,先在 .env 里清掉它再重建容器(或重启守护进程)",
                    "IPHONE_UDID pins {u}; to go back to automatic, remove it from .env and recreate the container (or restart the daemon)"
                ));
            }
            g.persisted.selected_udid = None;
            g.persisted.save(&self.cfg.state_file());
            return Ok(match g.info.as_ref().filter(|_| g.device.is_some()) {
                Some(i) => ictl_l10n::trf!("已改回自动选;正在控制 {},它断开时换成别的接着的设备", "Back to automatic; controlling {}, switches to another connected device when it disconnects", device_label(i)),
                None if g.connected.is_empty() => ictl_l10n::tr!("已改回自动选;现在没接设备,接上后自动控制它", "Back to automatic; no device connected now, it takes over the next one plugged in").into(),
                None => ictl_l10n::tr!("已改回自动选;10 秒内接管接着的设备,先用 iphone_status 确认就绪", "Back to automatic; it takes over a connected device within 10 seconds, check readiness with iphone_status first").into(),
            });
        }
        let infos = self.connected_infos(g);
        let target = infos[resolve_device(&infos, q)?].clone();
        if let Some(u) = &self.cfg.udid
            && !u.eq_ignore_ascii_case(&target.udid)
        {
            return Err(ictl_l10n::trf!(
                "IPHONE_UDID 固定为 {u};要换设备,先改 .env 里的 IPHONE_UDID 再重建容器(或重启守护进程)",
                "IPHONE_UDID pins {u}; to switch devices, change IPHONE_UDID in .env and recreate the container (or restart the daemon)"
            ));
        }
        if self.cfg.udid.is_none() {
            g.persisted.selected_udid = Some(target.udid.clone());
            g.persisted.save(&self.cfg.state_file());
        }
        let label = device_label(&target);
        if g.device.as_ref().is_some_and(|d| d.udid == target.udid) {
            return Ok(ictl_l10n::trf!(
                "已经在控制 {label},之后固定控制这台",
                "Already controlling {label}; it stays selected from now on"
            ));
        }
        let usb = g
            .connected
            .iter()
            .find(|d| d.udid == target.udid)
            .cloned()
            .ok_or(ictl_l10n::tr!("设备刚刚断开了", "the device just disconnected"))?;
        // 上一台的锁屏时间不能拿来等这一台
        g.last_lock = None;
        self.online(g, &usb).await;
        let pin = if self.cfg.passcode_file_for(&target.udid).is_file() {
            ""
        } else {
            ictl_l10n::tr!(
                ";这台没设自动解锁密码,锁屏时要人工解锁(在这台连着时运行 iphone-control passcode set)",
                "; this device has no auto-unlock passcode, so unlock it by hand when locked (run iphone-control passcode set while it is connected)"
            )
        };
        Ok(ictl_l10n::trf!(
            "已切换到 {label},之后固定控制这台。状态:{};WDA 起来要 10~60 秒,先用 iphone_status 确认就绪{pin}",
            "Switched to {label}; it stays selected from now on. State: {}; WDA takes 10-60 seconds to start, check readiness with iphone_status first{pin}",
            g.state.describe()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(udid: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            udid: udid.into(),
            name: name.into(),
            product_type: "iPhone16,2".into(),
            product_version: "27.0".into(),
        }
    }

    #[test]
    fn passcode_file_is_per_device_and_stays_in_its_dir() {
        let cfg = crate::Config::from_env(std::path::Path::new("/data"));
        assert_eq!(
            cfg.passcode_file_for("00008020-000925043499002E"),
            cfg.passcodes_dir().join("00008020-000925043499002E")
        );
        let odd = cfg.passcode_file_for("../../etc/x");
        assert_eq!(odd.parent(), Some(cfg.passcodes_dir().as_path()));
        assert_ne!(cfg.passcode_file_for("A"), cfg.legacy_passcode_file());
    }

    #[test]
    fn device_is_found_by_udid_then_name() {
        let ds =
            [dev("00008130-AAAA", "Derrors 的 iPhone"), dev("00008020-BBBB", "Derrors 的 iPad"), dev("C", "iPad 2")];
        assert_eq!(resolve_device(&ds, "00008020-bbbb"), Ok(1));
        assert_eq!(resolve_device(&ds, "Derrors 的 iPhone"), Ok(0));
        assert_eq!(resolve_device(&ds, " iphone "), Ok(0));
        // 「iPad」两台都包含：要用 UDID 指定
        assert!(resolve_device(&ds, "ipad").unwrap_err().contains("不止一台"));
        assert!(resolve_device(&ds, "Mac").unwrap_err().contains("没有接着"));
        assert!(resolve_device(&[], "x").is_err());
        // 名字完全相同的优先于包含
        assert_eq!(resolve_device(&ds, "iPad 2"), Ok(2));
    }
}
