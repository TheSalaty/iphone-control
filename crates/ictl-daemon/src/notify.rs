//! 通知：Bark、Telegram。同一事件去重（[`Dedup`]），恢复时推送一次。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::NotifyConfig;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    ResignFailed(String),
    SessionExpired(String),
    NeedsAutomationPin,
    WdaRecovered,
    UnlockDisabled,
    DeviceOffline { minutes: u64 },
    DeviceOnline { minutes: u64 },
}

impl Event {
    /// 去重用的键：同一类事件只推一次，直到被对应的恢复事件清掉或过了冷却时间
    fn key(&self) -> &'static str {
        match self {
            Self::ResignFailed(_) => "resign",
            Self::SessionExpired(_) => "session",
            Self::NeedsAutomationPin => "pin",
            Self::WdaRecovered => "pin-recovered",
            Self::UnlockDisabled => "unlock",
            Self::DeviceOffline { .. } => "offline",
            Self::DeviceOnline { .. } => "online",
        }
    }

    /// 每类事件一个样例（`iphone-control notify-test` 用），顺序与枚举一致
    pub fn samples() -> Vec<Event> {
        vec![
            Self::ResignFailed(
                ictl_l10n::tr!("（测试）苹果服务器返回错误", "(test) Apple's server returned an error").into(),
            ),
            Self::SessionExpired(ictl_l10n::tr!("（测试）令牌已过期", "(test) the token expired").into()),
            Self::NeedsAutomationPin,
            Self::WdaRecovered,
            Self::UnlockDisabled,
            Self::DeviceOffline { minutes: 10 },
            Self::DeviceOnline { minutes: 12 },
        ]
    }

    /// 样例的短名，`notify-test <名字>` 只发这一类
    pub fn sample_name(&self) -> &'static str {
        self.key()
    }

    pub fn title_body(&self) -> (String, String) {
        match self {
            Self::ResignFailed(why) => (
                ictl_l10n::tr!("iPhone WDA 重签失败", "iPhone: re-signing WDA failed").into(),
                ictl_l10n::trf!("{why}\n描述文件过期后 WDA 将无法启动(截图/点击的 HID 路线不受影响)。\n排查: iphone-control doctor", "{why}\nOnce the provisioning profile expires WDA can't start (the HID path for screenshots/taps is not affected).\nTroubleshoot: iphone-control doctor"),
            ),
            Self::SessionExpired(why) => (
                ictl_l10n::tr!("Apple ID 会话已失效", "Apple ID session expired").into(),
                ictl_l10n::trf!("{why}\n下次重签前需要重新登录: iphone-control login", "{why}\nSign in again before the next re-sign: iphone-control login"),
            ),
            Self::NeedsAutomationPin => (
                ictl_l10n::tr!("iPhone 需要输入锁屏密码", "iPhone needs the passcode").into(),
                ictl_l10n::tr!("手机重启后要启用 UI 自动化:请解锁手机,在「为 XCTest 输入 iPhone 密码」弹窗里输入锁屏密码。弹窗没出现或已关闭的话,1 分钟内会重新弹出。", "After a reboot, UI automation has to be enabled: unlock the phone and enter the passcode in the “Enter iPhone passcode for XCTest” prompt. If the prompt didn't appear or was dismissed, it comes back within a minute.").into(),
            ),
            Self::WdaRecovered => (ictl_l10n::tr!("iPhone WDA 已恢复", "iPhone WDA is back").into(), ictl_l10n::tr!("已输入密码,WDA 重新就绪,可以正常操作了。", "Passcode entered, WDA is ready again and everything works.").into()),
            Self::UnlockDisabled => (
                ictl_l10n::tr!("iPhone 自动解锁失败,已停用", "iPhone auto-unlock failed and was disabled").into(),
                ictl_l10n::tr!("整串密码输完手机仍锁着(可能密码配置不对),或输入途中出错、无法确认密码是否已提交。为免累计输错,自动解锁已停用。\n请人工解锁手机、核对密码(iphone-control passcode set),再运行 iphone-control unlock-reset。", "The phone stayed locked after entering the whole passcode (the configured passcode may be wrong), or an error happened while entering it so it's unclear whether it was submitted. Auto-unlock is now disabled so wrong attempts don't pile up.\nUnlock the phone by hand, check the passcode (iphone-control passcode set), then run iphone-control unlock-reset.").into(),
            ),
            Self::DeviceOffline { minutes } => (
                ictl_l10n::trf!("iPhone 已离线 {minutes} 分钟", "iPhone offline for {minutes} minutes"),
                ictl_l10n::tr!("连不上手机:检查 USB 线、手机是否没电关机。重新连上后会自动恢复。", "Can't reach the phone: check the USB cable and whether the phone ran out of battery. It recovers automatically once reconnected.").into(),
            ),
            Self::DeviceOnline { minutes } => (ictl_l10n::tr!("iPhone 已重新连上", "iPhone reconnected").into(), ictl_l10n::trf!("离线约 {minutes} 分钟,正在自动恢复。", "Offline for about {minutes} minutes, recovering automatically.")),
        }
    }
}

/// 事件去重：同一键在冷却期内只放行一次；`clear` 后可以再次放行
#[derive(Debug, Default)]
pub struct Dedup {
    sent: HashMap<&'static str, Instant>,
}

/// 冷却：重签失败、会话失效 20 小时内不重复推；其余等恢复事件清除
fn cooldown(e: &Event) -> Option<Duration> {
    match e {
        Event::ResignFailed(_) | Event::SessionExpired(_) => Some(Duration::from_secs(20 * 3600)),
        Event::WdaRecovered | Event::DeviceOnline { .. } => Some(Duration::ZERO),
        _ => None,
    }
}

impl Dedup {
    pub fn should_send(&mut self, e: &Event, now: Instant) -> bool {
        let key = e.key();
        let ok = match (self.sent.get(key), cooldown(e)) {
            (None, _) => true,
            (Some(_), Some(cd)) if cd.is_zero() => true,
            (Some(t), Some(cd)) => now.duration_since(*t) >= cd,
            (Some(_), None) => false,
        };
        if ok {
            self.sent.insert(key, now);
        }
        ok
    }

    pub fn clear(&mut self, key: &'static str) {
        self.sent.remove(key);
    }

    /// 之前是否推过（用来决定恢复时要不要推「已恢复」）
    pub fn was_sent(&self, key: &'static str) -> bool {
        self.sent.contains_key(key)
    }
}

/// 实际发送，返回成功的渠道数。失败只记日志，不影响守护进程
pub async fn send(cfg: &NotifyConfig, title: &str, body: &str) -> usize {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "{}",
                ictl_l10n::trf!("通知客户端创建失败：{e}", "Creating the notification client failed: {e}")
            );
            return 0;
        }
    };
    let mut ok = 0;
    if let (Some(token), Some(chat)) = (&cfg.tg_bot_token, &cfg.tg_chat_id) {
        let r = client
            .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
            .form(&[("chat_id", chat.as_str()), ("text", &format!("{title}\n{body}"))])
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match r {
            Ok(_) => ok += 1,
            Err(e) => tracing::warn!(
                "{}",
                ictl_l10n::trf!("Telegram 发送失败：{}", "Sending via Telegram failed: {}", e.without_url())
            ),
        }
    }
    if let Some(key) = &cfg.bark_key {
        let r = client
            .post(format!("{}/{key}", cfg.bark_server.trim_end_matches('/')))
            .form(&[("title", title), ("body", body), ("group", "iPhone")])
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match r {
            Ok(_) => ok += 1,
            Err(e) => tracing::warn!(
                "{}",
                ictl_l10n::trf!("Bark 发送失败：{}", "Sending via Bark failed: {}", e.without_url())
            ),
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_cover_every_event_kind_once() {
        let names: Vec<&str> = Event::samples().iter().map(|e| e.sample_name()).collect();
        let mut uniq = names.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), names.len());
        // 新增事件时这里的数字要跟着改，提醒同时补样例
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn same_event_is_sent_once_until_cleared() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send(&Event::NeedsAutomationPin, t));
        assert!(!d.should_send(&Event::NeedsAutomationPin, t + Duration::from_secs(3600)));
        d.clear("pin");
        assert!(d.should_send(&Event::NeedsAutomationPin, t));
    }

    #[test]
    fn resign_failure_has_20h_cooldown() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send(&Event::ResignFailed("x".into()), t));
        assert!(!d.should_send(&Event::ResignFailed("y".into()), t + Duration::from_secs(19 * 3600)));
        assert!(d.should_send(&Event::ResignFailed("z".into()), t + Duration::from_secs(21 * 3600)));
    }

    #[test]
    fn recovery_events_always_pass() {
        let mut d = Dedup::default();
        let t = Instant::now();
        assert!(d.should_send(&Event::WdaRecovered, t));
        assert!(d.should_send(&Event::WdaRecovered, t));
    }

    #[test]
    fn texts_match_toolkit() {
        assert_eq!(Event::NeedsAutomationPin.title_body().0, "iPhone 需要输入锁屏密码");
        assert_eq!(Event::DeviceOffline { minutes: 10 }.title_body().0, "iPhone 已离线 10 分钟");
    }
}
