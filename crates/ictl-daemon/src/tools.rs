//! 各工具请求的实现：优先 WDA，WDA 连不上时坐标类操作退到 HID。调用方持有设备锁。

use std::time::{Duration, Instant};

use base64::Engine;
use ictl_device::{AppInfo, Button, DeviceError, HidSession, Rotation, ScreenSize, apps, hid::ascii_to_usage};
use ictl_unlock::UnlockError;
use ictl_wda::client::SPRINGBOARD;
use ictl_wda::elements::{self, TapPoint, Tree};
use ictl_wda::http::DeviceTransport;
use ictl_wda::{Launch, WdaClient, WdaError};

use crate::core::{Daemon, Inner, drop_tunnel, hid_supported, wda_down};
use crate::imaging::{self, fmt_g};
use crate::notify::Event;
use crate::protocol::{ALERT_BLOCKING, ALERT_BLOCKING_EN, Response, tap_missed_prefixes};

/// 锁屏后至少等这么久再自动解锁：锁屏过渡没走完就唤醒，WDA 第一下上滑卡约 16 秒后报 `point.x != INFINITY`
pub(crate) const LOCK_SETTLE: Duration = Duration::from_secs(3);
/// 元素少于这么多个就按画面判断是否稳定（游戏只有几个状态栏元素；普通 App 页面通常几十个）
const VISUAL_SETTLE_BELOW: usize = 12;
/// 两帧缩略图平均逐像素差低于这个值算静止（0～255）。游戏背景的轻微摆动约 1～2，消除动画几十
const VISUAL_STILL: f64 = 3.0;
/// 整体静止时，某一小块的平均差超过这个值算「还有一小块在动」（加载圈、小动画）。完全没动的两帧差是 0。
/// 只在画面大部分是空白时才提醒（[`imaging::mostly_blank`]），游戏、地图的待机动画不算
const LOCAL_MOTION: f64 = 4.0;
/// 等界面稳定时，前后两次元素位置差不超过这么多点就算没动（惯性滚动的尾巴）
const STILL_TOLERANCE: i64 = 3;
/// 滑动后最多等这么久让列表停下（快速滑动的惯性滚动约 2 秒）
const SCROLL_SETTLE: Duration = Duration::from_millis(2500);
/// WDA 不可用时 status 里说明哪些功能用不了
const NEEDS_WDA: &str = "读界面元素、按名字点击、启动 App、输入中文要等 WDA 恢复";
const NEEDS_WDA_EN: &str =
    "reading elements, tapping by text, launching apps and typing non-ASCII text wait until WDA is back";
/// 按 Home 后最多等这么久，确认主屏到了前台（iPhone 12 mini 上 IOHID 的 Home 约 1.3～1.5 秒）
const HOME_SETTLE: Duration = Duration::from_secs(2);
/// 按电源键后最多等这么久，等 WDA 报锁屏（iPhone 12 mini 实测约 1 秒）
const LOCK_WAIT: Duration = Duration::from_secs(3);

impl Daemon {
    /// 配置了密码就在锁屏时自动解锁；没配置而手机锁着则报错（与 toolkit 一致）
    pub(crate) async fn ensure_unlocked(&self, g: &mut Inner) -> Result<(), String> {
        let cfg = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        if let Some(left) = g.last_lock.and_then(|t| LOCK_SETTLE.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let Some(wda) = self.wda(g) else { return Ok(()) };
        match ictl_unlock::ensure_unlocked(wda, &cfg).await {
            Ok(_) => Ok(()),
            Err(e @ (UnlockError::WrongPasscode | UnlockError::Uncertain(_))) => {
                self.notify(g, Event::UnlockDisabled);
                Err(ictl_l10n::trf!("{e},已推送通知", "{e}, notification sent"))
            }
            Err(UnlockError::NoPasscode) if self.cfg.legacy_passcode_file().is_file() => {
                Err(ictl_l10n::tr!("手机已锁屏,需要人工解锁:旧版本保存的锁屏密码没记是哪台设备的,同时接着几台时不会用它;\
在这台设备连着时运行 iphone-control passcode set 重新设置", "The phone is locked and has to be unlocked by hand: the passcode saved by an older version has no device, and it isn't used while several devices are connected; run iphone-control passcode set while this device is connected to set it again")
                    .into())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn screen(&self, g: &mut Inner) -> Result<ScreenSize, String> {
        if let Some(w) = self.wda(g)
            && let Ok(s) = w.screen_size().await
        {
            return Ok(s);
        }
        // 记下的尺寸是别的设备的就不用（旧版本没记是哪台，照用）
        let same = match (&g.persisted.screen_udid, &g.device) {
            (Some(u), Some(d)) => *u == d.udid,
            (Some(_), None) => false,
            (None, _) => true,
        };
        g.persisted
            .screen
            .filter(|_| same)
            .or(self.cfg.screen_fallback)
            .ok_or_else(|| ictl_l10n::tr!("WDA 不可用,也不知道屏幕尺寸;等 WDA 起来一次,或在 .env 里设 SCREEN_W/SCREEN_H", "WDA is unavailable and the screen size is unknown; wait for WDA to start once, or set SCREEN_W/SCREEN_H in .env").into())
    }

    /// 记下 WDA 报的界面旋转角：WDA 挂了以后 HID 点击要靠它换算横屏坐标（约 20 毫秒）
    pub(crate) async fn refresh_hid_rotation(&self, g: &mut Inner) {
        if let Some(w) = self.wda(g)
            && let Ok(r) = w.rotation().await
        {
            g.wda_rotation = Some((Instant::now(), r));
        }
    }

    /// 每次 HID 触摸前重新截图核对横竖，不依赖上次截图（转屏、回主屏、解锁都可能改变方向）。
    /// 截图只给宽高，不能分辨横屏左右；短暂掉线时借用近期 WDA 方向，过期或不匹配就不触摸。
    async fn hid_touch_rotation(&self, g: &mut Inner) -> Result<Rotation, String> {
        self.refresh_hid_rotation(g).await;
        let png = self.hid_screenshot(g).await.map_err(|e| {
            ictl_l10n::trf!(
                "核对 HID 方向失败，没有发送触摸：{e}",
                "Checking the HID orientation failed, no touch sent: {e}"
            )
        })?;
        let (w, h) = imaging::png_size(&png)
            .ok_or(ictl_l10n::tr!("无法读取截图尺寸，没有发送触摸", "Can't read the screenshot size, no touch sent"))?;
        let known = g.wda_rotation.map(|(at, r)| (at.elapsed(), r));
        hid_rotation(w, h, known)
    }

    /// 经隧道截图（WDA 不可用时）。失败就丢掉隧道，下次重建
    async fn hid_screenshot(&self, g: &mut Inner) -> Result<Vec<u8>, String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let t = self.ensure_tunnel(g, &device).await.map_err(|e| e.to_string())?;
        match t.screenshot_png().await {
            Ok(p) => Ok(p),
            Err(e) => {
                drop_tunnel(g).await;
                Err(e.to_string())
            }
        }
    }

    /// HID 会话（WDA 不可用时的后备路线）。开不起来就丢掉隧道，下次重建
    async fn hid<'a>(&self, g: &'a mut Inner) -> Result<&'a mut HidSession, String> {
        // 低版本建隧道只会报 service not found：直接说清楚
        if !hid_supported(g.info.as_ref()) {
            return Err(wda_down(&g.state, g.info.as_ref()));
        }
        let screen = self.screen(g).await?;
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        // HID 触摸按设备竖放的坐标归一化
        let screen = screen.portrait();
        let t = self
            .ensure_tunnel(g, &device)
            .await
            .map_err(|e| ictl_l10n::trf!("HID 后备路线不可用：{e}", "HID fallback unavailable: {e}"))?;
        if let Err(e) = t.hid(screen).await {
            drop_tunnel(g).await;
            return Err(ictl_l10n::trf!("HID 后备路线不可用：{e}", "HID fallback unavailable: {e}"));
        }
        // 第二次调用直接返回已开好的会话（这样写是为了绕开借用检查对条件返回的限制）
        g.tunnel.as_mut().expect("刚建好").hid(screen).await.map_err(|e| e.to_string())
    }

    async fn hid_do<F>(&self, g: &mut Inner, f: F) -> Result<(), String>
    where
        F: for<'h> FnOnce(
            &'h mut HidSession,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ictl_device::Result<()>> + Send + 'h>>,
    {
        let r = f(self.hid(g).await?).await;
        g.hid_used = Some(Instant::now());
        match r {
            Ok(()) => Ok(()),
            // 参数问题（例如不能输入的字符），连接没坏，不用重建
            Err(e @ DeviceError::InvalidInput(_)) => Err(e.to_string()),
            Err(e) => {
                drop_tunnel(g).await;
                Err(ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}"))
            }
        }
    }

    /// 接着多台、或选定的那台没接时，说明在控制哪台、怎么切换；只接着一台时为空（输出和原来一样）
    pub(crate) fn devices_line(&self, g: &Inner) -> Option<String> {
        let pinned = self.pinned(g);
        let current = g.device.as_ref().map(|d| d.udid.as_str());
        let others: Vec<String> = self
            .connected_infos(g)
            .iter()
            .filter(|i| Some(i.udid.as_str()) != current)
            .map(crate::core::device_label)
            .collect();
        let how = if self.cfg.udid.is_some() {
            ictl_l10n::tr!(
                "(IPHONE_UDID 固定了设备,要换先改 .env)",
                " (IPHONE_UDID pins the device; change .env to switch)"
            )
        } else {
            ictl_l10n::tr!(";用 iphone_select_device 切换", "; switch with iphone_select_device")
        };
        match (current, &pinned) {
            (None, Some(p)) => Some(ictl_l10n::trf!(
                "设备: 选定的 {p} 没连接{}{how}",
                "Device: the selected {p} is not connected{}{how}",
                if others.is_empty() {
                    String::new()
                } else {
                    ictl_l10n::trf!(",接着的有 {}", ", connected are {}", others.join(ictl_l10n::tr!("、", ", ")))
                }
            )),
            (Some(_), _) if !others.is_empty() => {
                let me = g.info.as_ref().map(crate::core::device_label).unwrap_or_default();
                let mode = if pinned.is_some() {
                    ictl_l10n::tr!("选定", "selected")
                } else {
                    ictl_l10n::tr!("自动选", "automatic")
                };
                Some(ictl_l10n::trf!(
                    "设备: 正在控制 {me}({mode});另外接着 {}{how}",
                    "Device: controlling {me} ({mode}); also connected {}{how}",
                    others.join(ictl_l10n::tr!("、", ", "))
                ))
            }
            _ => None,
        }
    }

    pub(crate) async fn status_text(&self, g: &mut Inner) -> String {
        let mut daemon_line = ictl_l10n::trf!("守护进程: {}", "Daemon: {}", g.state.describe());
        if let Some(l) = self.devices_line(g) {
            daemon_line = format!("{l}\n{daemon_line}");
        }
        let unlock = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        let Some(w) = self.wda(g) else {
            return format!("{}\n{}\n{daemon_line}", wda_down(&g.state, g.info.as_ref()), needs_wda());
        };
        let r: Result<String, WdaError> = async {
            let ready = w.ready().await?;
            let app = w.active_app().await?;
            let s = w.screen_size().await?;
            let check = w.check_lock(None, false).await?;
            let locked = check.locked;
            let mut lock = if check.uncertain {
                ictl_l10n::tr!("可能已锁屏(屏幕上是锁屏样式的界面,WDA 说没锁;配置了自动解锁时操作前会先尝试解锁)", "possibly locked (the screen looks like the lock screen though WDA says unlocked; with auto-unlock configured, actions try to unlock first)").to_owned()
            } else if !locked {
                ictl_l10n::tr!("未锁屏", "unlocked").to_owned()
            } else if unlock.disabled_by_failure() {
                ictl_l10n::tr!("已锁屏(自动解锁之前失败过已停用,需要人工解锁,核对密码后运行 iphone-control unlock-reset)", "locked (auto-unlock was disabled after an earlier failure; unlock by hand, check the passcode, then run iphone-control unlock-reset)").to_owned()
            } else if unlock.enabled() {
                ictl_l10n::tr!("已锁屏(操作时会自动解锁)", "locked (actions unlock automatically)").to_owned()
            } else {
                ictl_l10n::tr!("已锁屏(未配置自动解锁,需要人工解锁)", "locked (no auto-unlock configured; unlock by hand)").to_owned()
            };
            if !locked && unlock.disabled_by_failure() {
                lock += ictl_l10n::tr!(";自动解锁已停用", "; auto-unlock disabled");
            }
            let ready = if ready { "True" } else { "False" };
            Ok(ictl_l10n::trf!("WDA 就绪: {ready}\n锁屏: {lock}\n前台 App: {app}\n屏幕: {}x{} 点", "WDA ready: {ready}\nLocked: {lock}\nForeground app: {app}\nScreen: {}x{} points",
                fmt_g(s.width),
                fmt_g(s.height)
            ))
        }
        .await;
        match r {
            Ok(t) => format!("{t}\n{daemon_line}"),
            Err(e) => format!("{e}\n{}\n{daemon_line}", needs_wda()),
        }
    }

    pub(crate) async fn screenshot(
        &self,
        g: &mut Inner,
        region: Option<(f64, f64, f64, f64)>,
    ) -> Result<Response, String> {
        self.ensure_unlocked(g).await?;
        let wda_png = match self.wda(g) {
            Some(w) => w.screenshot_png().await.ok(),
            None => None,
        };
        // 给短暂 WDA 掉线时的 HID 留一份近期方向；不把方向永久缓存到磁盘。
        if wda_png.is_some() {
            self.refresh_hid_rotation(g).await;
        }
        let (png, via) = match wda_png {
            Some(p) => (p, "WDA"),
            None => {
                if !hid_supported(g.info.as_ref()) {
                    return Err(wda_down(&g.state, g.info.as_ref()));
                }
                (
                    self.hid_screenshot(g)
                        .await
                        .map_err(|e| ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}"))?,
                    "HID",
                )
            }
        };
        let screen = self.screen(g).await?;
        // 解码、缩放、JPEG 编码是 CPU 活（1290×2796），放到阻塞线程，不卡住主循环
        // 说明文字在阻塞线程里生成：带上这个请求的语言
        let lang = ictl_l10n::lang();
        let (jpeg, caption) = tokio::task::spawn_blocking(move || {
            ictl_l10n::with_lang(lang, || match region {
                None => imaging::full_screen(&png, screen, via),
                Some(r) => imaging::region(&png, screen, r, via),
            })
        })
        .await
        .map_err(|e| ictl_l10n::trf!("图像处理任务异常：{e}", "Image processing task failed: {e}"))??;
        Ok(Response::Image { jpeg_base64: base64::engine::general_purpose::STANDARD.encode(jpeg), caption })
    }

    /// 当前界面的元素树。缓存不超过 `max_age` 就直接用（0 = 总是重读）；重读的结果写回缓存
    async fn tree(&self, g: &mut Inner, max_age: Duration) -> Result<Tree, String> {
        if !max_age.is_zero()
            && let Some((t, tree)) = &g.elements_cache
            && t.elapsed() <= max_age
        {
            return Ok(tree.clone());
        }
        let tree = self.require_wda(g)?.tree().await.map_err(|e| e.to_string())?;
        g.elements_cache = Some((Instant::now(), tree.clone()));
        Ok(tree)
    }

    pub(crate) async fn list_elements(
        &self,
        g: &mut Inner,
        filter: &str,
        limit: usize,
        max_age: Duration,
    ) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let els = self.tree(g, max_age).await?.elements;
        let shown: Vec<&elements::Element> =
            if filter.is_empty() { els.iter().collect() } else { elements::filter(&els, filter) };
        let lines: Vec<_> = shown.iter().take(limit).map(|e| e.describe()).collect();
        let more = if shown.len() > limit {
            ictl_l10n::trf!(
                "\n……另有 {} 个未列出,可用 filter 缩小范围",
                "\n…{} more not listed, narrow it down with filter",
                shown.len() - limit
            )
        } else {
            String::new()
        };
        Ok(if lines.is_empty() { no_elements_text(filter, els.len()) } else { lines.join("\n") } + &more)
    }

    /// 等界面稳定，最多等 `max`：
    /// - 有元素树的界面：连续两次读到的元素一样，并且没有加载圈。轮询期间让 WDA 读树时不等界面空闲、动画冷却
    ///   （否则滑完第一次读要等惯性滚动停下再冷却 2 秒，页面加载时一次能卡好几秒，超过上限很多）
    /// - 元素很少的界面（游戏、地图，画面变化不反映在元素树里）：改看画面，连续两帧缩略图几乎一样。
    ///   截图约 0.2 秒一张，两帧之间再隔一点，避免两帧落在同一段静止的过渡里
    ///
    /// 屏幕上有弹窗时在结果里提醒（读树时顺带看，不多花时间）
    pub(crate) async fn settle(&self, g: &mut Inner, max: Duration) -> Result<String, String> {
        let t0 = Instant::now();
        let Some(w) = self.wda(g) else {
            tokio::time::sleep(max.min(Duration::from_millis(500))).await;
            return Ok(ictl_l10n::tr!("WDA 不可用,等了 0.5 秒", "WDA unavailable, waited 0.5 s").into());
        };
        w.begin_polling().await.map_err(|e| e.to_string())?;
        let r = poll_stable(w, t0, max).await;
        w.end_polling().await;
        let (first, polled) = r?;
        let Some((msg, tree)) = polled else {
            // 元素很少：改看画面
            let note = alert_note(&first);
            return Ok(self.settle_visually(g, t0, max).await? + &note);
        };
        let note = alert_note(&tree);
        // 自己画的加载圈（不是 ActivityIndicator）不在树里，树稳定了页面可能还在加载。
        // 加载中的页面中间是空的：只在这种时候多看一眼画面（约 0.5 秒），平常不多花时间
        let screen = self.screen(g).await?;
        let msg = if msg.starts_with(ictl_l10n::tr!("界面已稳定", "Screen settled"))
            && middle_is_empty(&tree, screen.height)
        {
            msg + &self.motion_note(g).await
        } else {
            msg
        };
        // 紧接着的 then=elements 直接用这棵树
        g.elements_cache = Some((Instant::now(), tree));
        Ok(msg + &note)
    }

    async fn settle_visually(&self, g: &mut Inner, t0: Instant, max: Duration) -> Result<String, String> {
        let deadline = t0 + max;
        let w = self.require_wda(g)?;
        let mut prev: Option<Vec<u8>> = None;
        loop {
            let png = w.screenshot_png().await.map_err(|e| e.to_string())?;
            let cur = thumbnail(png).await?;
            if let Some(p) = &prev {
                let m = imaging::thumb_motion(p, &cur);
                if m.mean < VISUAL_STILL {
                    let local = if m.max_tile >= LOCAL_MOTION && imaging::mostly_blank(&cur) {
                        ictl_l10n::tr!(
                            ";但有一小块还在动(可能是加载圈或小动画),内容可能还没加载完,要等结果用 iphone_wait_for 或稍后再截图",
                            "; but a small area is still moving (perhaps a spinner or a small animation), content may still be loading; to wait for a result use iphone_wait_for or take a screenshot later"
                        )
                    } else {
                        ""
                    };
                    return Ok(ictl_l10n::trf!(
                        "画面已静止({:.1} 秒){local}",
                        "Picture still ({:.1} s){local}",
                        t0.elapsed().as_secs_f64()
                    ));
                }
            }
            // 不够再截一张（约 0.2 秒）、隔一下再比的时间了
            if deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(300) {
                return Ok(ictl_l10n::trf!(
                    "{:.1} 秒内画面一直在动,按最后的状态返回",
                    "The picture kept moving for {:.1} s, returning the last state",
                    max.as_secs_f64()
                ));
            }
            prev = Some(cur);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// 隔一下截两张图，画面还在动就返回提醒，否则返回空串
    async fn motion_note(&self, g: &mut Inner) -> String {
        let Some(w) = self.wda(g) else { return String::new() };
        let mut thumbs = Vec::new();
        for i in 0..2 {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let Ok(png) = w.screenshot_png().await else { return String::new() };
            let Ok(t) = thumbnail(png).await else { return String::new() };
            thumbs.push(t);
        }
        let m = imaging::thumb_motion(&thumbs[0], &thumbs[1]);
        if (m.mean >= VISUAL_STILL || m.max_tile >= LOCAL_MOTION) && imaging::mostly_blank(&thumbs[1]) {
            ictl_l10n::tr!(";但画面中间是空的、还有东西在动(可能还在加载),要等结果用 iphone_wait_for 或稍后再截图", "; but the middle of the screen is empty and something is still moving (perhaps still loading); to wait for a result use iphone_wait_for or take a screenshot later").into()
        } else {
            String::new()
        }
    }

    /// 前台是 SpringBoard 时查有没有系统弹窗（权限请求等），有就返回提醒，没有返回空串。
    /// 不带 then 的操作做完后调：弹窗挡着时点击、滑动都会落在弹窗上，WDA 却照样报成功。
    /// 前台是 App 时只多一次约 0.13 秒的查询；在主屏上再多约 0.4 秒
    pub(crate) async fn alert_check(&self, g: &mut Inner) -> Result<String, String> {
        let Some(w) = self.wda(g) else { return Ok(String::new()) };
        if w.active_app().await.map_err(|e| e.to_string())? != SPRINGBOARD {
            return Ok(String::new());
        }
        Ok(match w.alert_text().await {
            Ok(Some(text)) => alert_msg(&text),
            _ => String::new(),
        })
    }

    /// 等包含 `text` 的元素出现
    pub(crate) async fn wait_for(&self, g: &mut Inner, text: &str, timeout: Duration) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let t0 = Instant::now();
        let w = self.require_wda(g)?;
        // 轮询时不等界面空闲：页面还在加载、播放动画时每次读树也只要约 0.2 秒
        w.begin_polling().await.map_err(|e| e.to_string())?;
        let r = async {
            loop {
                let els = w.elements().await.map_err(|e| e.to_string())?;
                // 不算输入框里的字：刚输入的搜索词不能算「结果出来了」
                if let Some(e) = elements::find_shown(&els, text).first() {
                    return Ok(ictl_l10n::trf!("已出现 {}({:.1} 秒)", "Appeared: {} ({:.1} s)", e.describe(), t0.elapsed().as_secs_f64()));
                }
                if t0.elapsed() >= timeout {
                    return Err(ictl_l10n::trf!("{:.0} 秒内没出现包含 \"{text}\" 的元素,截图看看屏幕", "No element containing \"{text}\" appeared within {:.0} s; take a screenshot to look at the screen", timeout.as_secs_f64()));
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        .await;
        w.end_polling().await;
        r
    }

    pub(crate) async fn tap(&self, g: &mut Inner, x: f64, y: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let pos = format!("({},{})", fmt_g(x), fmt_g(y));
        if let Some(w) = self.wda(g) {
            // 只有连不上才退到 HID；超时说明请求已发出，再用 HID 做一遍可能点两次
            match w.tap(x, y).await {
                Ok(()) => return Ok(ictl_l10n::trf!("已点击 {pos}", "Tapped {pos}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.tap(x, y).await
            })
        })
        .await?;
        Ok(ictl_l10n::trf!("已点击 {pos}(经 HID)", "Tapped {pos} (via HID)"))
    }

    pub(crate) async fn tap_element(
        &self,
        g: &mut Inner,
        text: &str,
        index: usize,
        max_age: Duration,
    ) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let tree = self.tree(g, max_age).await?;
        let matches = elements::find(&tree.elements, text);
        if matches.is_empty() {
            return Ok(ictl_l10n::trf!(
                "{}包含 \"{text}\" 的元素,先用 iphone_list_elements 或截图看看屏幕",
                "{}: no element contains \"{text}\"; look at the screen with iphone_list_elements or a screenshot first",
                tap_missed_prefixes()[0]
            ));
        }
        if index >= matches.len() {
            let list: Vec<_> = matches.iter().map(|e| e.describe()).collect();
            return Ok(ictl_l10n::trf!(
                "{} {} 个:\n{}",
                "{} {} (index out of range):\n{}",
                tap_missed_prefixes()[1],
                matches.len(),
                list.join("\n")
            ));
        }
        let e = matches[index];
        // 中心被悬浮栏、键盘、浮层之类盖住时，点上去会落在盖着的东西上
        let ((x, y), moved) = match tree.tap_point(e) {
            TapPoint::Center(p) => (p, String::new()),
            TapPoint::Moved { at, by } => (
                at,
                ictl_l10n::trf!(
                    ";它的中心被「{by}」盖住,改点没被盖住的 ({},{})",
                    "; its center is covered by “{by}”, tapped an uncovered part at ({},{}) instead",
                    fmt_g(at.0),
                    fmt_g(at.1)
                ),
            ),
            TapPoint::Covered { by } => {
                return Ok(ictl_l10n::trf!(
                    "{}:{} 整个被「{by}」盖住,没有点;先滑动让它露出来,或截图后按坐标点",
                    "{}: {} is completely covered by “{by}”, not tapped; scroll to reveal it first, or tap by coordinates from a screenshot",
                    tap_missed_prefixes()[2],
                    e.describe()
                ));
            }
        };
        self.require_wda(g)?.tap(x, y).await.map_err(|e| e.to_string())?;
        let note = if matches.len() > 1 {
            ictl_l10n::trf!("(共匹配 {} 个,点的是第 {index} 个)", " ({} matches, tapped number {index})", matches.len())
        } else {
            String::new()
        };
        Ok(ictl_l10n::trf!("已点击 {}{note}{moved}", "Tapped {}{note}{moved}", e.describe()))
    }

    pub(crate) async fn long_press(&self, g: &mut Inner, x: f64, y: f64, seconds: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.touch_and_hold(x, y, seconds).await.map_err(|e| e.to_string())?;
        Ok(ictl_l10n::trf!("已长按 ({},{}) {} 秒", "Long-pressed ({},{}) for {} s", fmt_g(x), fmt_g(y), fmt_g(seconds)))
    }

    pub(crate) async fn swipe(
        &self,
        g: &mut Inner,
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
    ) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let desc = format!("({},{}) → ({},{})", fmt_g(from.0), fmt_g(from.1), fmt_g(to.0), fmt_g(to.1));
        if let Some(w) = self.wda(g) {
            match w.swipe(from, to, (seconds * 1000.0) as u64).await {
                // 滑动请求不等惯性滚动（等的话设置不对能卡十几秒），这里等列表停下再返回：
                // 列表还在滚时下一次点击只会让它停住，点不到东西
                Ok(()) => {
                    let vertical = (to.1 - from.1).abs() >= (to.0 - from.0).abs();
                    return Ok(ictl_l10n::trf!("已滑动 {desc}{}", "Swiped {desc}{}", wait_scrolled(w, vertical).await));
                }
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.drag(from, to, seconds).await
            })
        })
        .await?;
        Ok(ictl_l10n::trf!("已滑动 {desc}(经 HID)", "Swiped {desc} (via HID)"))
    }

    pub(crate) async fn type_text(&self, g: &mut Inner, text: &str, submit: bool) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let n = text.chars().count();
        if let Some(w) = self.wda(g) {
            // /wda/keys 没有输入框获得焦点时也返回成功，其实什么都没输：先看屏幕上有没有键盘
            let typed = match w.keyboard_shown().await {
                Ok(false) => {
                    return Err(
                        ictl_l10n::tr!("屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入", "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up").into(),
                    );
                }
                Ok(true) if text.is_empty() => Ok(()),
                Ok(true) => w.keys(text).await,
                Err(e) => Err(e),
            };
            match typed {
                Ok(()) => {
                    // 有的 App 边输入边刷新输入框，会吞掉一部分字（高德搜索框「故宫博物院」只剩「故宫博」），WDA 照样报成功：
                    // 输完读回输入框核对，少了结尾几个字就补上；对不上就不按回车
                    let note = match verify_typed(w, text).await {
                        Typed::Ok | Typed::Unknown => String::new(),
                        Typed::Fixed(k) => ictl_l10n::trf!(
                            "(第一次有 {k} 个字没输进去,已补上)",
                            " ({k} characters were dropped the first time and typed again)"
                        ),
                        Typed::Mismatch(now) => {
                            return Err(ictl_l10n::trf!(
                                "输入了 {n} 个字符,但输入框里现在是「{now}」,和要输入的「{text}」对不上(App 可能吞掉了部分字符){};截图确认后再处理",
                                "Typed {n} characters, but the field now contains “{now}”, which doesn't match “{text}” (the app may have swallowed some characters){}; check with a screenshot before going on",
                                if submit { ictl_l10n::tr!(",没有按回车", ", Return not pressed") } else { "" }
                            ));
                        }
                    };
                    if submit {
                        w.keys("\n").await.map_err(|e| {
                            ictl_l10n::trf!(
                                "文字已输入{note},按回车失败:{e}",
                                "Text typed{note}, pressing Return failed: {e}"
                            )
                        })?;
                    }
                    return Ok(ictl_l10n::trf!(
                        "已输入 {n} 个字符{note}{}",
                        "Typed {n} characters{note}{}",
                        if submit { ictl_l10n::tr!(",并按了回车", " and pressed Return") } else { "" }
                    ));
                }
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        let payload = if submit { format!("{text}\n") } else { text.to_owned() };
        // 先检查，一个都不输；不打开 HID 会话
        if let Some(c) = payload.chars().find(|c| ascii_to_usage(*c).is_none()) {
            return Err(ictl_l10n::trf!(
                "WDA 不可用,HID 只能输入美式键盘上的英文和符号,不能输入 {c:?}",
                "WDA unavailable; HID can only type English letters and symbols of a US keyboard, not {c:?}"
            ));
        }
        self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&payload).await })).await?;
        Ok(ictl_l10n::trf!("已输入 {n} 个字符(经 HID)", "Typed {n} characters (via HID)"))
    }

    pub(crate) async fn press_button(&self, g: &mut Inner, button: &str) -> Result<String, String> {
        let hid_button = Button::parse(button).ok_or(ictl_l10n::tr!(
            "button 只能是 home / volume_up / volume_down / lock",
            "button must be home / volume_up / volume_down / lock"
        ))?;
        let wda_name = match hid_button {
            Button::Lock => return self.lock_screen(g).await,
            Button::Home => "home",
            Button::VolumeUp => "volumeUp",
            Button::VolumeDown => "volumeDown",
        };
        self.ensure_unlocked(g).await?;
        if let Some(w) = self.wda(g) {
            // Home 在系统弹窗（定位、通知权限等）挡着时不生效，WDA 却照样返回成功。弹窗属于 SpringBoard，
            // 看前台 App 发现不了，要单独查；按之前查（App 静止时约 0.4 秒，按下后动画中查要好几秒）
            if hid_button == Button::Home
                && let Ok(Some(text)) = w.alert_text().await
            {
                return Ok(ictl_l10n::trf!(
                    "{},按 Home 不会生效,没有按;{TAP_ALERT}",
                    "{}, pressing Home would have no effect, not pressed; {TAP_ALERT_EN}",
                    blocking(&text)
                ));
            }
            let r = if hid_button == Button::Home {
                press_home(w).await
            } else {
                w.press_button(wda_name).await.map(|()| ictl_l10n::trf!("已按 {button}", "Pressed {button}"))
            };
            match r {
                Ok(text) => return Ok(text),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(hid_button))).await?;
        Ok(ictl_l10n::trf!("已按 {button}", "Pressed {button}"))
    }

    async fn lock_screen(&self, g: &mut Inner) -> Result<String, String> {
        if let Some(w) = self.wda(g) {
            // 已经锁着就不按：熄屏时再按电源键会把屏幕点亮
            let locked = matches!(w.locked().await, Ok(true)) || lock_by_power_hid(w).await || {
                // WDA 锁屏偶尔等 5 秒报 500（手机其实已锁）：确认锁了就不退到 HID（HID 会开屏幕媒体流）
                match w.lock().await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::info!("{}", ictl_l10n::trf!("WDA 锁屏报错：{e}", "WDA lock returned an error: {e}"));
                        w.wda_reports_locked().await.unwrap_or(false)
                    }
                }
            };
            if locked {
                g.last_lock = Some(Instant::now());
                return Ok(ictl_l10n::tr!("已锁屏", "Locked").into());
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(Button::Lock))).await?;
        g.last_lock = Some(Instant::now());
        Ok(ictl_l10n::tr!("已锁屏", "Locked").into())
    }

    pub(crate) async fn list_apps(&self, g: &mut Inner, include_system: bool) -> Result<Vec<AppInfo>, String> {
        if let Some(a) = g.apps.get(&include_system) {
            return Ok(a.clone());
        }
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let a = device.list_apps(include_system).await.map_err(|e| e.to_string())?;
        g.apps.insert(include_system, a.clone());
        Ok(a)
    }

    async fn resolve_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        if apps::looks_like_bundle_id(app) {
            return Ok(app.to_owned());
        }
        if let Some(b) = self.resolve_cached(g, app).await? {
            return Ok(b);
        }
        // 守护进程启动后新装的 App 不在缓存里：刷新一次再找
        g.apps.clear();
        self.resolve_cached(g, app).await?.ok_or_else(|| {
            ictl_l10n::trf!(
                "找不到名为 \"{app}\" 的 App,用 iphone_list_apps 查看",
                "No app named \"{app}\"; see iphone_list_apps"
            )
        })
    }

    async fn resolve_cached(&self, g: &mut Inner, app: &str) -> Result<Option<String>, String> {
        // 先只查用户 App；列系统 App 要好几秒，只在找不到时才去取
        let user = self.list_apps(g, false).await?;
        if let Some(b) = user.iter().find(|a| a.name == app) {
            return Ok(Some(b.bundle_id.clone()));
        }
        let system = self.list_apps(g, true).await?;
        Ok(apps::resolve(app, [&user[..], &system[..]]))
    }

    pub(crate) async fn launch_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        let w = self.require_wda(g)?;
        Ok(match w.launch_app(&bid).await.map_err(|e| e.to_string())? {
            Launch::Foreground => ictl_l10n::trf!("已启动 {bid}", "Launched {bid}"),
            Launch::Blocked(text) => {
                ictl_l10n::trf!(
                    "已启动 {bid},但{};{TAP_ALERT}",
                    "Launched {bid}, but {}; {TAP_ALERT_EN}",
                    blocking(&text)
                )
            }
            Launch::Slow => ictl_l10n::trf!(
                "已启动 {bid},但 8 秒内没到前台(可能启动慢),先截图确认",
                "Launched {bid}, but it didn't come to the foreground within 8 s (maybe slow to start); check with a screenshot"
            ),
        })
    }

    pub(crate) async fn terminate_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.terminate_app(&bid).await.map_err(|e| e.to_string())?;
        Ok(ictl_l10n::trf!("已结束 {bid}", "Terminated {bid}"))
    }

    pub(crate) async fn open_url(&self, g: &mut Inner, url: &str) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let (r, app) = self.require_wda(g)?.open_url(url).await.map_err(|e| e.to_string())?;
        Ok(match r {
            Launch::Foreground => ictl_l10n::trf!("已打开 {url}(前台 {app})", "Opened {url} (foreground {app})"),
            Launch::Blocked(text) => {
                ictl_l10n::trf!(
                    "已打开 {url},但{};{TAP_ALERT}",
                    "Opened {url}, but {}; {TAP_ALERT_EN}",
                    blocking(&text)
                )
            }
            Launch::Slow => ictl_l10n::trf!(
                "已打开 {url},但 8 秒内没有 App 到前台(前台还是 {app}),先截图确认",
                "Opened {url}, but no app came to the foreground within 8 s (still {app}); check with a screenshot"
            ),
        })
    }
}

/// 只给 WDA 短暂掉线留窗口。缓存不能跨进程重启、跨设备或长期代替方向查询。
const HID_ROTATION_MAX_AGE: Duration = Duration::from_secs(30);

/// 当前截图必须能确定横竖；横屏还需近期 WDA 旋转角。仅凭宽高无法识别 WDA 离线期间的 180° 翻转，
/// 所以后备操作期间不要翻转设备；恢复 WDA 后重新确认方向。
pub(crate) fn hid_rotation(w: u32, h: u32, known: Option<(Duration, Rotation)>) -> Result<Rotation, String> {
    if w == 0 || h == 0 || w == h {
        return Err(ictl_l10n::tr!(
            "截图尺寸无效，无法核对 HID 方向，没有发送触摸",
            "Invalid screenshot size, can't check the HID orientation, no touch sent"
        )
        .into());
    }
    let recent = known.filter(|(age, _)| *age <= HID_ROTATION_MAX_AGE).map(|(_, r)| r);
    if w > h {
        return recent.filter(|r| r.is_landscape()).ok_or_else(|| {
            ictl_l10n::tr!("当前为横屏，但没有近期且匹配的 WDA 旋转角，没有发送 HID 触摸；等 WDA 恢复后重试，或转回竖屏", "The screen is landscape but there is no recent matching WDA rotation, no HID touch sent; retry once WDA is back, or rotate to portrait").into()
        });
    }
    Ok(if recent == Some(Rotation::UpsideDown) { Rotation::UpsideDown } else { Rotation::Portrait })
}

/// 按 Home，等主屏真的到前台再说「已按」（紧接着查状态时前台不再是旧 App）。
/// 先经 IOHID 按（XCTest 的 home 在 iPhone 12 mini / iOS 26.4.1 上不起作用却报成功），
/// [`HOME_SETTLE`] 内没回到主屏再用 XCTest 的 home 补一次；IOHID 这一下报错也改用 XCTest 的
async fn press_home(w: &mut WdaClient<DeviceTransport>) -> Result<String, WdaError> {
    let mut app = String::new();
    for via_hid in [true, false] {
        let sent = if via_hid { w.press_home_hid().await } else { w.press_button("home").await };
        match sent {
            Ok(()) => {}
            Err(e @ WdaError::Unavailable(_)) => return Err(e),
            Err(e) if via_hid => {
                tracing::info!(
                    "{}",
                    ictl_l10n::trf!(
                        "经 IOHID 按 Home 报错,改用 XCTest：{e}",
                        "Pressing Home via IOHID failed, using XCTest: {e}"
                    )
                );
                continue;
            }
            Err(e) => return Err(e),
        }
        let deadline = Instant::now() + HOME_SETTLE;
        loop {
            app = w.active_app().await.unwrap_or_default();
            if app == SPRINGBOARD {
                return Ok(ictl_l10n::tr!("已按 home", "Pressed home").into());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if via_hid {
            tracing::info!(
                "{}",
                ictl_l10n::trf!(
                    "经 IOHID 按 Home 后前台还是 {app},改用 XCTest 再按一次",
                    "After pressing Home via IOHID the foreground is still {app}, pressing again via XCTest"
                )
            );
        }
    }
    Ok(ictl_l10n::trf!(
        "已按 home,但前台还是 {app},先截图看看",
        "Pressed home, but the foreground is still {app}; take a screenshot"
    ))
}

/// 经 IOHID 按电源键锁屏，等 WDA 报锁屏，最多 [`LOCK_WAIT`]。锁上了返回 true。
/// WDA 的 `/wda/lock` 在 iOS 27 以下用 XCTest 的 `pressLockButton`，iPhone 12 mini（iOS 26.4.1）上锁不了屏
async fn lock_by_power_hid(w: &mut WdaClient<DeviceTransport>) -> bool {
    if let Err(e) = w.press_power_hid().await {
        tracing::info!(
            "{}",
            ictl_l10n::trf!("经 IOHID 按电源键报错：{e}", "Pressing the power key via IOHID failed: {e}")
        );
        return false;
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if w.wda_reports_locked().await.unwrap_or(false) {
            return true;
        }
        if Instant::now() >= deadline {
            tracing::info!(
                "{}",
                ictl_l10n::trf!(
                    "经 IOHID 按电源键后 {} 秒还没锁屏,改用 WDA 的锁屏",
                    "Still not locked {} s after pressing the power key via IOHID, using WDA's lock",
                    LOCK_WAIT.as_secs()
                )
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 屏幕中间（上下各去掉一段，留给导航栏、标签栏）几乎没有元素：加载中、空白页
fn middle_is_empty(tree: &Tree, screen_h: f64) -> bool {
    let (top, bottom) = ((screen_h * 0.25) as i64, (screen_h * 0.85) as i64);
    tree.elements.iter().filter(|e| e.center.1 > top && e.center.1 < bottom).count() <= 1
}

/// 滑动后等列表停下（最多 [`SCROLL_SETTLE`]）：沿滑动方向的位置连续两次差不超过 [`STILL_TOLERANCE`]。
/// 没停下返回提醒；元素很少（游戏、地图）看不出来，不等
async fn wait_scrolled(w: &mut WdaClient<DeviceTransport>, vertical: bool) -> String {
    if w.begin_polling().await.is_err() {
        return String::new();
    }
    let t0 = Instant::now();
    let r: Result<bool, WdaError> = async {
        let mut prev = w.tree().await?;
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            return Ok(true);
        }
        while t0.elapsed() < SCROLL_SETTLE {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let cur = w.tree().await?;
            if cur.still_along(&prev, vertical, STILL_TOLERANCE) {
                return Ok(true);
            }
            prev = cur;
        }
        Ok(false)
    }
    .await;
    w.end_polling().await;
    match r {
        Ok(false) => ictl_l10n::trf!(
            "({:.1} 秒后列表还在动,可能还在滚动)",
            " (the list was still moving after {:.1} s, it may still be scrolling)",
            SCROLL_SETTLE.as_secs_f64()
        ),
        _ => String::new(),
    }
}

/// 轮询元素树直到连续两次一样（位置差不超过 [`STILL_TOLERANCE`]，且没有加载圈）或到 `max`。返回第一次读到的树，以及 (结果说明, 最后的树)；
/// 第一次读到的元素很少（游戏、地图）时第二项是 None，由调用方改看画面。
/// 不中途丢弃请求：丢掉的请求手机上仍在执行，后面的请求要排在它后面
async fn poll_stable(
    w: &mut WdaClient<DeviceTransport>,
    t0: Instant,
    max: Duration,
) -> Result<(Tree, Option<(String, Tree)>), String> {
    let first = w.tree().await.map_err(|e| e.to_string())?;
    if first.elements.len() < VISUAL_SETTLE_BELOW {
        return Ok((first, None));
    }
    let mut prev = first.clone();
    let msg = loop {
        if t0.elapsed() >= max {
            break if prev.loading {
                ictl_l10n::trf!(
                    "{:.1} 秒内页面一直在加载(有加载圈),按最后的状态返回;要等结果出来用 iphone_wait_for",
                    "The page kept loading for {:.1} s (spinner shown), returning the last state; to wait for the result use iphone_wait_for",
                    max.as_secs_f64()
                )
            } else {
                ictl_l10n::trf!(
                    "{:.1} 秒内界面一直在变(可能有动画),按最后的状态返回",
                    "The screen kept changing for {:.1} s (perhaps an animation), returning the last state",
                    max.as_secs_f64()
                )
            };
        }
        // 一次读树本身约 0.2 秒，两次之间不用再多等
        tokio::time::sleep(Duration::from_millis(30)).await;
        let cur = w.tree().await.map_err(|e| e.to_string())?;
        let same = cur.close_to(&prev, STILL_TOLERANCE);
        prev = cur;
        if same && !prev.loading {
            break ictl_l10n::trf!("界面已稳定({:.1} 秒)", "Screen settled ({:.1} s)", t0.elapsed().as_secs_f64());
        }
    };
    Ok((first, Some((msg, prev))))
}

/// 判断画面动没动用的缩略图。解码是 CPU 活，放到阻塞线程
async fn thumbnail(png: Vec<u8>) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || imaging::thumbnail(&png))
        .await
        .map_err(|e| ictl_l10n::trf!("图像处理任务异常：{e}", "Image processing task failed: {e}"))?
}

/// 列元素一个都没列出来时的说明。带 filter 没匹配上时要说清楚，不然 agent 会以为屏幕上什么都没有
fn no_elements_text(filter: &str, total: usize) -> String {
    if filter.is_empty() || total == 0 {
        // 与 toolkit 的返回文字一致
        ictl_l10n::tr!("没有找到带文字的元素", "No elements with text found").into()
    } else {
        ictl_l10n::trf!(
            "没有找到包含 \"{filter}\" 的元素(屏幕上另有 {total} 个带文字的元素,不带 filter 可全部列出)",
            "No element containing \"{filter}\" ({total} other elements with text on screen; leave out filter to list them all)"
        )
    }
}

fn alert_msg(text: &str) -> String {
    ictl_l10n::trf!(
        "\n注意:{},点击、滑动都会落在弹窗上;{TAP_ALERT}",
        "\nNote: {}, taps and swipes land on the alert; {TAP_ALERT_EN}",
        blocking(text)
    )
}

const TAP_ALERT: &str = "先用 iphone_tap_element 点弹窗上的按钮";
const TAP_ALERT_EN: &str = "first tap a button on the alert with iphone_tap_element";

fn needs_wda() -> &'static str {
    if ictl_l10n::is_en() { NEEDS_WDA_EN } else { NEEDS_WDA }
}

/// 「屏幕上有系统弹窗「…」挡着」：MCP 端靠固定的开头认出结果里已经报过弹窗
fn blocking(text: &str) -> String {
    ictl_l10n::trf!("{ALERT_BLOCKING}「{text}」挡着", "{ALERT_BLOCKING_EN}: “{text}”")
}

/// 树里有弹窗就返回提醒
fn alert_note(tree: &Tree) -> String {
    match tree.alert() {
        Some(a) => {
            let title = [&a.label, &a.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
            ictl_l10n::trf!(
                "\n注意:屏幕上有弹窗「{title}」,先点弹窗上的按钮(见元素列表或截图)",
                "\nNote: an alert “{title}” is on screen; tap a button on it first (see the element list or the screenshot)"
            )
        }
        None => String::new(),
    }
}

/// 输入后核对的结果
#[derive(Debug, PartialEq)]
enum Typed {
    Ok,
    /// 核对不了：没读到输入框、有密码框、或读树失败
    Unknown,
    /// 结尾有几个字没输进去，已补上
    Fixed(usize),
    /// 对不上（输入框里现在的内容）
    Mismatch(String),
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 输入框里的内容有没有包含刚输入的文字（忽略空白：手机号之类会被自动加空格）
fn typed_into(values: &[String], text: &str) -> bool {
    let want = squash(text);
    values.iter().any(|v| squash(v).contains(&want))
}

/// 输入框结尾是 `text` 的前几个字（后面的丢了）：返回输进去了几个字
fn typed_prefix(values: &[String], text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    values
        .iter()
        .filter_map(|v| {
            let v = squash(v);
            (1..chars.len()).rev().find(|&k| {
                let p = squash(&chars[..k].iter().collect::<String>());
                !p.is_empty() && v.ends_with(&p)
            })
        })
        .max()
}

async fn verify_typed(w: &mut WdaClient<DeviceTransport>, text: &str) -> Typed {
    if squash(text).is_empty() {
        return Typed::Unknown;
    }
    let mut values = None;
    // 树可能比输入框慢一拍：第一次对不上隔一下再读一次，确认真的少了再补，免得补重
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let Some(v) = w.elements().await.ok().and_then(|e| elements::field_values(&e)) else { return Typed::Unknown };
        if v.is_empty() {
            return Typed::Unknown;
        }
        if typed_into(&v, text) {
            return Typed::Ok;
        }
        values = Some(v);
    }
    let values = values.unwrap_or_default();
    let shown = || values.iter().find(|v| !v.is_empty()).cloned().unwrap_or_default();
    let Some(k) = typed_prefix(&values, text) else { return Typed::Mismatch(shown()) };
    // 剩下的一个字一个字慢慢补
    let rest: Vec<char> = text.chars().skip(k).collect();
    for c in &rest {
        if w.keys(&c.to_string()).await.is_err() {
            return Typed::Mismatch(shown());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    match w.elements().await.ok().and_then(|e| elements::field_values(&e)) {
        Some(v) if typed_into(&v, text) => Typed::Fixed(rest.len()),
        Some(v) => Typed::Mismatch(v.into_iter().find(|v| !v.is_empty()).unwrap_or_default()),
        None => Typed::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_rotation_requires_fresh_matching_landscape() {
        use Rotation::*;
        let fresh = |r| Some((Duration::ZERO, r));
        assert_eq!(hid_rotation(2796, 1290, fresh(TopToLeft)), Ok(TopToLeft));
        assert_eq!(hid_rotation(2796, 1290, fresh(TopToRight)), Ok(TopToRight));
        assert!(hid_rotation(2796, 1290, None).is_err());
        assert!(hid_rotation(2796, 1290, fresh(Portrait)).is_err());
        assert!(hid_rotation(2796, 1290, Some((Duration::from_secs(31), TopToLeft))).is_err());
        assert_eq!(hid_rotation(1290, 2796, fresh(TopToLeft)), Ok(Portrait));
        assert_eq!(hid_rotation(1290, 2796, fresh(UpsideDown)), Ok(UpsideDown));
        assert_eq!(hid_rotation(1290, 2796, None), Ok(Portrait));
        assert!(hid_rotation(0, 2796, fresh(Portrait)).is_err());
        assert!(hid_rotation(100, 100, fresh(Portrait)).is_err());
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn no_match_with_filter_names_the_filter() {
        assert_eq!(no_elements_text("", 0), "没有找到带文字的元素");
        assert_eq!(no_elements_text("通用", 0), "没有找到带文字的元素");
        let t = no_elements_text("通用", 12);
        assert!(t.contains("\"通用\"") && t.contains("12 个"), "{t}");
    }

    #[test]
    fn typed_text_is_found_ignoring_spaces() {
        assert!(typed_into(&v(&["搜索", "红烧肉做法"]), "红烧肉做法"));
        assert!(typed_into(&v(&["130 1234 5678"]), "13012345678"));
        assert!(!typed_into(&v(&["故宫博"]), "故宫博物院"));
    }

    #[test]
    fn dropped_tail_is_detected_after_existing_text() {
        assert_eq!(typed_prefix(&v(&["故宫博"]), "故宫博物院"), Some(3));
        // 输入框原来就有字
        assert_eq!(typed_prefix(&v(&["abc故宫"]), "故宫博物院"), Some(2));
        // 一个字都没进去（或者是占位文字）：不猜，报对不上
        assert_eq!(typed_prefix(&v(&["搜索"]), "故宫博物院"), None);
        assert_eq!(typed_prefix(&v(&[""]), "故宫"), None);
    }
}
