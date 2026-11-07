//! 巡检和后台任务：usbmuxd 看护、每 10 秒推进状态机、后台启动 WDA、重签、Apple ID 会话检查。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ictl_device::{Device, DeviceError, SYSFS_USB, Tunnel, UsbDevice};
use ictl_wda::{WdaClient, WdaError, WdaRunner, http::DeviceTransport};

use crate::core::{Daemon, DeviceProfile, Inner, close_hid};
use crate::machine::DeviceState;
use crate::notify::Event;

/// 正常 2～10 秒就绪。手机重启后未授权 automation 时，testmanagerd 握手每次卡 30 秒、要重试一两次，
/// 60 秒常常等不到 runner 启动，也就认不出「需要输入密码」
const WDA_START_TIMEOUT: Duration = Duration::from_secs(120);
/// HID 会话空闲这么久就关掉。它持有屏幕媒体流，开着时 iOS 当作屏幕被远程查看：不会自动锁屏，
/// 还会弹「viewed remotely」通知。重开约 0.5 秒
pub(crate) const HID_IDLE: Duration = Duration::from_secs(30);
/// usbmuxd 起来后这么久内就退出，算「起不来」，按退避重试
const USBMUXD_QUICK_EXIT: Duration = Duration::from_secs(30);
const USBMUXD_MAX_DELAY: Duration = Duration::from_secs(300);
/// 连续这么多轮列不出设备才当作离线
const LIST_ERRORS_OFFLINE: u32 = 3;
/// 开发者镜像连续挂载失败这么多次就清缓存重新下载
const DDI_FAILURES_INVALIDATE: u32 = 3;
/// 整个重签（登录会话、注册设备、签名、上传安装）的时限：苹果服务器或安装卡住时不能永远「正在重签」
const RESIGN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Apple ID 会话检查的时限
const SESSION_CHECK_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// 换了设备或重新接上后，这么久内就检查一次要不要重签（离线期间可能过期了，不能等满 RESIGN_CHECK_HOURS）
const RESIGN_CHECK_AFTER_CONNECT: Duration = Duration::from_secs(60);

/// 后台启动 WDA 的结果
pub(crate) enum StartOutcome {
    Ready {
        runner: WdaRunner,
        client: WdaClient<DeviceTransport>,
        mounted: Option<String>,
    },
    NeedsUnlock,
    /// 开发者镜像没准备好。`mount_failed`：是挂载这一步失败（才可能是缓存坏了）；
    /// 建隧道、查询、下载失败都是 false
    Ddi {
        error: String,
        mount_failed: bool,
    },
    Wda(WdaError),
}

impl Daemon {
    // ------------------------------------------------------------------ usbmuxd

    pub(crate) async fn ensure_usbmuxd(&self, g: &mut Inner, restart: bool) {
        if !self.cfg.manage_usbmuxd {
            return;
        }
        let exited = match g.usbmuxd.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => None,
            Some(Ok(Some(st))) => Some(st.to_string()),
            Some(Err(e)) => Some(e.to_string()),
            None => Some(String::new()),
        };
        if exited.is_none() && !restart {
            return;
        }
        if !restart {
            // 起来就退出（例如宿主机上已有 usbmuxd 占着 socket）：退避重试，不要每 10 秒拆一次会话、写一行日志
            if let Some(why) = exited.filter(|w| !w.is_empty())
                && g.usbmuxd_started.is_some_and(|t| t.elapsed() < USBMUXD_QUICK_EXIT)
            {
                g.usbmuxd_delay = (g.usbmuxd_delay * 2).clamp(Duration::from_secs(10), USBMUXD_MAX_DELAY);
                g.usbmuxd_retry_at = Some(Instant::now() + g.usbmuxd_delay);
                g.usbmuxd_started = None;
                self.log.log(&ictl_l10n::trf!("usbmuxd 启动后很快就退出了（{why}），{} 秒后再试；宿主机已在运行 usbmuxd 时设 ICTL_MANAGE_USBMUXD=0", "usbmuxd exited right after starting ({why}), retrying in {} seconds; if the host already runs usbmuxd, set ICTL_MANAGE_USBMUXD=0",
                    g.usbmuxd_delay.as_secs()
                ));
            }
            if g.usbmuxd_retry_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
        }
        // 经旧 usbmuxd 建立的连接（隧道、runner、WDA）都要失效：先停 HID 媒体流（还要经它），再停它
        self.drop_session(g).await;
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.cfg.logs().join("usbmuxd.log"));
        let mut cmd = tokio::process::Command::new(&self.cfg.usbmuxd_bin);
        cmd.args(["-U", "root", "-f"]).kill_on_drop(true);
        if let Ok(f) = log
            && let Ok(f2) = f.try_clone()
        {
            cmd.stdout(f).stderr(f2);
        }
        match cmd.spawn() {
            Ok(c) => {
                g.usbmuxd = Some(c);
                g.usbmuxd_started = Some(Instant::now());
                if g.usbmuxd_retry_at.take().is_none() {
                    g.usbmuxd_delay = Duration::ZERO;
                }
                self.log.log(ictl_l10n::tr!("usbmuxd 已(重新)启动", "usbmuxd (re)started"));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => self.log.log(&ictl_l10n::trf!("启动 usbmuxd 失败：{e}", "Starting usbmuxd failed: {e}")),
        }
    }

    // ------------------------------------------------------------------ 状态机

    /// 每 10 秒一次。有请求在用设备就跳过这一轮（巡检不排队，下一轮再来）
    pub async fn tick(self: Arc<Self>) {
        let Ok(mut g) = self.inner.try_lock() else { return };
        let g = &mut *g;
        self.ensure_usbmuxd(g, false).await;

        if g.tunnel.as_ref().is_some_and(Tunnel::hid_open) && g.hid_used.is_none_or(|t| t.elapsed() >= HID_IDLE) {
            close_hid(g).await;
            tracing::info!(
                "{}",
                ictl_l10n::tr!(
                    "HID 会话空闲，已关闭（停止屏幕媒体流）",
                    "HID session idle, closed it (screen media stream stopped)"
                )
            );
        }
        self.poll_start(g).await;

        let devices = match ictl_device::list_devices().await {
            Ok(d) => {
                g.list_errors = 0;
                d
            }
            Err(e) => {
                // usbmuxd 抖一下就当离线会拆掉整个会话：连续几轮都失败才算
                g.list_errors += 1;
                tracing::debug!(
                    "{}",
                    ictl_l10n::trf!(
                        "列设备失败（第 {} 次）：{e}",
                        "Listing devices failed (attempt {}): {e}",
                        g.list_errors
                    )
                );
                if g.list_errors < LIST_ERRORS_OFFLINE {
                    self.publish(g);
                    return;
                }
                Vec::new()
            }
        };
        self.note_connected(g, devices).await;
        let pinned = self.pinned(g);
        let udids: Vec<&str> = g.connected.iter().map(|d| d.udid.as_str()).collect();
        let current = g.device.as_ref().map(|d| d.udid.as_str());
        let target = crate::machine::pick_device(&udids, pinned.as_deref(), current).map(|i| g.connected[i].clone());
        // 只在容器里（自己看护 usbmuxd、没有 udev）需要；用系统的 usbmuxd 服务时插拔由 udev 处理，也重启不了它
        let on_usb = ictl_device::ios_usb_count(Path::new(SYSFS_USB));
        if self.cfg.manage_usbmuxd && g.mux.observe(on_usb, g.connected.len()) {
            self.log.log(&ictl_l10n::trf!(
                "USB 上有 {on_usb} 台 iPhone/iPad,usbmuxd 只认到 {} 台,重启 usbmuxd",
                "{on_usb} iPhone/iPad on USB but usbmuxd only sees {}, restarting usbmuxd",
                g.connected.len()
            ));
            self.ensure_usbmuxd(g, true).await;
        }

        match target {
            None => self.offline(g).await,
            Some(d) => {
                self.online(g, &d).await;
                self.adopt_legacy_passcode(g);
            }
        }

        let now = Instant::now();
        if now >= g.next_resign {
            g.next_resign = now + self.cfg.resign_check;
            if g.device.is_some() && g.persisted.wda_bundle.is_some() {
                let d = self.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = d.resign_now(false).await {
                        let mut g = d.inner.lock().await;
                        d.notify(&mut g, Event::ResignFailed(e));
                    }
                });
            }
        }
        if now >= g.next_session_check {
            g.next_session_check = now + self.cfg.session_check;
            tokio::task::spawn_local(self.clone().check_session());
        }
        self.publish(g);
    }

    async fn offline(&self, g: &mut Inner) {
        let now = Instant::now();
        g.device_profile = DeviceProfile::NotChecked;
        g.wda_rotation = None;
        if g.device.take().is_some() {
            self.log.log(ictl_l10n::tr!("手机离线", "Phone offline"));
            // 名字、型号、iOS 版本是这台的：留着的话状态、提示文字会把它当成还在控制的设备
            g.info = None;
            self.drop_session(g).await;
            g.offline_since = Some(now);
            g.apps.clear();
        }
        self.set_state(g, DeviceState::Absent);
        let since = *g.offline_since.get_or_insert(now);
        let alert = self.cfg.offline_alert;
        if !alert.is_zero() && now.duration_since(since) >= alert {
            self.notify(g, Event::DeviceOffline { minutes: alert.as_secs() / 60 });
        }
    }

    pub(crate) async fn online(&self, g: &mut Inner, seen: &UsbDevice) {
        // 只比 UDID 不够：断开几秒又接上（短于离线判定、落在两轮巡检之间）时 UDID 不变，
        // usbmuxd 却换了编号，旧句柄的每个连接都报 bad device，永远恢复不了
        if !g.device.as_ref().is_some_and(|d| d.is_connection(seen)) {
            match g.device.take() {
                Some(old) if old.udid == seen.udid => self.log.log(&ictl_l10n::trf!(
                    "手机重新接入（usbmuxd 编号 {} → {}），重建连接",
                    "Phone reconnected (usbmuxd id {} → {}), reconnecting",
                    old.device_id(),
                    seen.device_id
                )),
                Some(old) => self.log.log(&ictl_l10n::trf!(
                    "改为控制 {}（原来是 {}）",
                    "Now controlling {} (was {})",
                    seen.udid,
                    old.udid
                )),
                None => {}
            }
            self.drop_session(g).await;
            g.device_profile = DeviceProfile::NotChecked;
            // App 列表是上一台的（同一台重新接入也重读，便宜）
            g.apps.clear();
            g.wda_rotation = None;
            let d = Device::from_usb(seen);
            g.info = d.info().await.ok();
            if let Some(i) = &g.info {
                g.infos.insert(seen.udid.clone(), i.clone());
            }
            let model =
                g.info.as_ref().map(|i| format!(" {} iOS {}", i.product_type, i.product_version)).unwrap_or_default();
            self.log.log(&ictl_l10n::trf!("手机在线{model}", "Phone online{model}"));
            g.device = Some(d);
            g.backoff.reset();
            g.next_resign = g.next_resign.min(Instant::now() + RESIGN_CHECK_AFTER_CONNECT);
            if let Some(since) = g.offline_since.take()
                && g.dedup.was_sent("offline")
            {
                g.dedup.clear("offline");
                self.notify(g, Event::DeviceOnline { minutes: since.elapsed().as_secs() / 60 });
            }
        }
        let device = g.device.clone().expect("刚设置过");

        match device.paired().await {
            Ok(true) => {
                if g.device_profile == DeviceProfile::NotChecked {
                    self.refresh_device_profile(g).await;
                }
            }
            Ok(false) => return self.set_state(g, DeviceState::NeedsTrust),
            // usbmuxd 出错：这一轮不下结论
            Err(e) => {
                return tracing::debug!(
                    "{}",
                    ictl_l10n::trf!("查配对记录失败：{e}", "Checking the pairing record failed: {e}")
                );
            }
        }
        if g.state != DeviceState::Ready && !device.developer_mode_enabled().await.unwrap_or(true) {
            return self.set_state(g, DeviceState::DeveloperModeOff);
        }
        let Some(bundle) = g.persisted.wda_bundle.clone() else {
            return self.set_state(g, DeviceState::NeedsSigning);
        };

        // WDA 健康：本进程的 runner 还活着，且 /status ready
        let runner_alive = g.runner.as_ref().is_some_and(|r| r.alive());
        let ready = match (&g.wda, runner_alive) {
            (Some(w), true) => w.ready().await.unwrap_or(false),
            _ => false,
        };
        if ready {
            g.wda_misses.reset();
            self.refresh_hid_rotation(g).await;
            if g.state != DeviceState::Ready {
                if g.state == DeviceState::NeedsAutomationPin || g.dedup.was_sent("pin") {
                    g.dedup.clear("pin");
                    self.notify(g, Event::WdaRecovered);
                }
                self.set_state(g, DeviceState::Ready);
                g.backoff.reset();
                if let Some(w) = g.wda.as_mut()
                    && let Ok(s) = w.screen_size().await
                {
                    // 存竖放尺寸：WDA 在横屏时就绪的话这里拿到的是横的
                    g.persisted.screen = Some(s.portrait());
                    g.persisted.screen_udid = g.device.as_ref().map(|d| d.udid.clone());
                    g.persisted.save(&self.cfg.state_file());
                }
            }
            return;
        }
        if runner_alive && !g.wda_misses.hit() {
            return;
        }
        if g.runner.is_some() {
            self.log.log(ictl_l10n::tr!("WDA 不可用", "WDA unavailable"));
            // 隧道和 HID 也一起丢：WDA 掉线常常是底层连接断了（例如 usbmuxd 重启），
            // 留着旧隧道，下次查镜像就报 socket io failed 并白白退避 60 秒
            self.drop_session(g).await;
        }
        g.wda_misses.reset();
        if g.starting.is_some() || !g.backoff.ready(Instant::now()) {
            return;
        }
        self.spawn_start(g, device, bundle);
    }

    /// 在后台启动 WDA：挂开发者镜像（单独开一条隧道，用完关掉）→ 启动 runner 等就绪。
    /// 最长约 2 分钟（等「Enable UI Automation」密码时），期间不持锁：工具调用照常走 HID 后备路线
    fn spawn_start(&self, g: &mut Inner, device: Device, bundle: String) {
        // 正在等密码、等解锁时保持这个状态，免得每次重试都在两个状态间来回切、刷日志
        if !matches!(g.state, DeviceState::NeedsAutomationPin | DeviceState::NeedsUnlock) {
            self.set_state(g, DeviceState::WdaStarting { last_error: None });
        }
        self.log.log(ictl_l10n::tr!("启动 WDA", "Starting WDA"));
        let (cache, url, user_dir) = (self.cfg.ddi_cache(), self.cfg.ddi_url.clone(), self.cfg.ddi_dir.clone());
        g.starting = Some(tokio::task::spawn_local(async move {
            // 手机重启后开发者镜像可能掉了；已挂载时只是一次查询。错误带上「是不是挂载这一步」
            let ddi: Result<Option<String>, (DeviceError, bool)> = async {
                let mut t = device.open_tunnel().await.map_err(|e| (e, false))?;
                let r = async {
                    if t.ddi_mounted().await.map_err(|e| (e, false))?.is_some() {
                        return Ok(None);
                    }
                    let dir = match &user_dir {
                        Some(d) => ictl_device::ddi::require_complete(d),
                        None => ictl_device::ddi::ensure_cache(&cache, &url).await,
                    }
                    .map_err(|e| (e, false))?;
                    Ok(Some(t.mount_ddi(&dir).await.map_err(|e| (e, true))?))
                }
                .await;
                t.close().await;
                r
            }
            .await;
            let mounted = match ddi {
                Ok(v) => v,
                Err((DeviceError::PasswordProtected, _)) => return StartOutcome::NeedsUnlock,
                Err((e, mount_failed)) => return StartOutcome::Ddi { error: e.to_string(), mount_failed },
            };
            match WdaRunner::start(device.provider(), &bundle, WDA_START_TIMEOUT).await {
                Ok((runner, client)) => StartOutcome::Ready { runner, client, mounted },
                Err(e) => StartOutcome::Wda(e),
            }
        }));
    }

    /// 后台启动做完了就收结果
    async fn poll_start(&self, g: &mut Inner) {
        if !g.starting.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let h = g.starting.take().expect("刚检查过");
        let outcome = h.await.unwrap_or_else(|e| {
            StartOutcome::Wda(WdaError::StartFailed(ictl_l10n::trf!("启动任务异常：{e}", "Start task failed: {e}")))
        });
        match outcome {
            StartOutcome::Ready { runner, client, mounted } => {
                if let Some(v) = mounted {
                    self.log.log(&ictl_l10n::trf!("开发者镜像已挂载 {v}", "Developer disk image mounted {v}"));
                }
                g.ddi_failures = 0;
                g.runner = Some(runner);
                g.wda = Some(client);
                // 状态在下面 online() 的健康检查里变成 Ready
            }
            StartOutcome::NeedsUnlock => {
                // 不退避的话，紧接着的 online() 会立刻重新启动，状态被改回「正在启动」，用户看不到要解锁
                g.backoff.retry_after(Instant::now(), crate::machine::UNLOCK_RETRY);
                self.set_state(g, DeviceState::NeedsUnlock);
            }
            StartOutcome::Ddi { error: e, mount_failed } => {
                // 只有挂载失败才可能是缓存坏了；ICTL_DDI_DIR 是用户自己的文件，从不删
                if mount_failed && self.cfg.ddi_dir.is_none() {
                    g.ddi_failures += 1;
                    if g.ddi_failures >= DDI_FAILURES_INVALIDATE {
                        self.log.log(&ictl_l10n::trf!("开发者镜像连续 {} 次挂载失败，清掉缓存，下次重新下载", "Mounting the developer disk image failed {} times in a row, clearing the cache to download it again", g.ddi_failures));
                        ictl_device::ddi::invalidate(&self.cfg.ddi_cache());
                        g.ddi_failures = 0;
                    }
                }
                g.backoff.failed(Instant::now());
                self.set_state(
                    g,
                    DeviceState::WdaStarting {
                        last_error: Some(ictl_l10n::trf!("开发者镜像：{e}", "Developer disk image: {e}")),
                    },
                );
            }
            StartOutcome::Wda(WdaError::NeedsAutomationPin) => {
                g.backoff.waiting_for_pin(Instant::now());
                self.set_state(g, DeviceState::NeedsAutomationPin);
                self.notify(g, Event::NeedsAutomationPin);
            }
            StartOutcome::Wda(e) => {
                g.backoff.failed(Instant::now());
                self.log.log(&ictl_l10n::trf!("WDA 启动失败：{e}", "WDA failed to start: {e}"));
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(e.to_string()) });
            }
        }
    }

    // ------------------------------------------------------------------ 重签与会话

    async fn check_session(self: Arc<Self>) {
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        let r: Result<(), Option<String>> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                match tokio::time::timeout(SESSION_CHECK_TIMEOUT, account.session()).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(ictl_sign::SignError::NotLoggedIn)) => Err(None),
                    Ok(Err(e)) => Err(Some(e.to_string())),
                    Err(_) => Err(Some(ictl_l10n::trf!(
                        "{} 分钟内没有完成",
                        "not finished within {} minutes",
                        SESSION_CHECK_TIMEOUT.as_secs() / 60
                    ))),
                }
            })
        })
        .await
        .unwrap_or_else(|e| Err(Some(ictl_l10n::trf!("会话检查任务异常：{e}", "Session check task failed: {e}"))));
        let mut g = self.inner.lock().await;
        match r {
            Ok(()) => g.dedup.clear("session"),
            Err(None) => {}
            Err(Some(e)) => {
                self.log
                    .log(&ictl_l10n::trf!("Apple ID 会话检查失败：{e}", "Checking the Apple ID session failed: {e}"));
                self.notify(&mut g, Event::SessionExpired(e));
            }
        }
    }

    /// 检查描述文件并按需重签（`force` 立即重签）。分三段：持锁取参数 → 不持锁签名、安装（几十秒）→ 持锁写结果
    pub(crate) async fn resign_now(self: &Arc<Self>, force: bool) -> Result<String, String> {
        let (device, name) = {
            let mut g = self.inner.lock().await;
            let g = &mut *g;
            // 看这台手机上装的那份，不看最近一次签名：给别的手机签过之后，这台装的还是更早签的、更早到期
            if !force && g.persisted.wda_bundle.is_some() {
                self.refresh_device_profile(g).await;
                if g.device_profile == DeviceProfile::Missing {
                    // 自动重签不替没装过的手机注册设备（占 Apple ID 的设备名额），交给用户决定
                    return Ok(
                        ictl_l10n::tr!("这台手机上没有装当前 Apple ID 签的 WDA,不自动重签;要装就运行 iphone-control setup", "No WDA signed by the current Apple ID on this phone, not re-signing automatically; run iphone-control setup to install it").into()
                    );
                }
                if let Some(left) = g.device_profile.left(&g.persisted)
                    && left > self.cfg.resign_min
                {
                    return Ok(ictl_l10n::trf!(
                        "描述文件还剩 {} 小时,无需重签",
                        "{} hours left on the provisioning profile, no need to re-sign",
                        left.as_secs() / 3600
                    ));
                }
            }
            if g.resigning {
                return Ok(ictl_l10n::tr!(
                    "正在重签,稍后用 iphone-control status 查看结果",
                    "Re-signing, check the result later with iphone-control status"
                )
                .into());
            }
            let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
            let name = g
                .info
                .as_ref()
                .map(|i| i.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "iphone-control".into());
            g.resigning = true;
            self.log.log(ictl_l10n::tr!("开始重签", "Re-signing started"));
            (device, name)
        };
        let signed_udid = device.udid.clone();
        let account = self.account();
        let (wda_dir, unsigned, signed_path) =
            (self.cfg.wda_dir(), self.cfg.wda_unsigned.clone(), self.cfg.wda_signed());
        let fetch = self.cfg.wda_unsigned_is_cache() && !unsigned.is_file();
        if fetch {
            self.log.log(&ictl_l10n::trf!(
                "没有未签名的 WDA 包（不在镜像里运行），先下载 {}",
                "No unsigned WDA package (not running in the image), downloading {} first",
                crate::wda_pkg::WDA_VERSION
            ));
        }
        // plume_core 的签名 future 不是 Send（持有 &mut dyn FnMut 和非 Send 的私钥），放到阻塞线程里 block_on
        let r: Result<ictl_sign::Signed, String> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current()
                .block_on(tokio::time::timeout(RESIGN_TIMEOUT, async move {
                    if fetch {
                        crate::wda_pkg::ensure(&unsigned).await?;
                    }
                    let session = account.session().await.map_err(|e| e.to_string())?;
                    session.ensure_device(&device.udid, &name).await.map_err(|e| e.to_string())?;
                    std::fs::create_dir_all(&wda_dir).map_err(|e| e.to_string())?;
                    let tmp = wda_dir.join("WDA-signing.ipa");
                    let signed = session.sign_ipa(&unsigned, &tmp, &device.udid).await.map_err(|e| e.to_string())?;
                    device.install(&tmp).await.map_err(|e| {
                        ictl_l10n::trf!(
                            "安装到手机失败(手机锁屏?)：{e}",
                            "Installing on the phone failed (phone locked?): {e}"
                        )
                    })?;
                    std::fs::rename(&tmp, &signed_path).map_err(|e| e.to_string())?;
                    Ok(signed)
                }))
                .unwrap_or_else(|_| {
                    Err(ictl_l10n::trf!(
                        "{} 分钟内没有完成(苹果服务器或安装卡住)",
                        "not finished within {} minutes (Apple's servers or the install got stuck)",
                        RESIGN_TIMEOUT.as_secs() / 60
                    ))
                })
        })
        .await
        .unwrap_or_else(|e| Err(ictl_l10n::trf!("签名任务异常退出：{e}", "The signing task exited abnormally: {e}")));

        let mut g = self.inner.lock().await;
        let g = &mut *g;
        g.resigning = false;
        let r = match r {
            Ok(s) => {
                let exp = s.profile.expires.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                g.persisted.team_id = Some(s.team_id);
                g.persisted.wda_bundle = Some(s.bundle_id);
                g.persisted.profile_expires = Some(exp);
                g.persisted.last_resign =
                    Some(ictl_l10n::trf!("{} 成功", "{} succeeded", crate::eventlog::timestamp()));
                g.persisted.save(&self.cfg.state_file());
                // 新包装上后旧的 runner 会失效，下一轮巡检重新拉起。签名期间切到了别的设备就不动它的会话
                if g.device.as_ref().is_some_and(|d| d.udid == signed_udid) {
                    self.drop_session(g).await;
                    self.refresh_device_profile(g).await;
                    g.backoff.reset();
                }
                g.dedup.clear("resign");
                let msg = ictl_l10n::trf!(
                    "重签完成,新描述文件到期 {} UTC",
                    "Re-signing done, the new provisioning profile expires {} UTC",
                    crate::eventlog::format_utc(exp)
                );
                self.log.log(&msg);
                Ok(msg)
            }
            Err(e) => {
                g.persisted.last_resign =
                    Some(ictl_l10n::trf!("{} 失败：{e}", "{} failed: {e}", crate::eventlog::timestamp()));
                g.persisted.save(&self.cfg.state_file());
                self.log.log(&ictl_l10n::trf!("重签失败：{e}", "Re-signing failed: {e}"));
                Err(e)
            }
        };
        self.publish(g);
        r
    }
}
