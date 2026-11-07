//! 设备层：一台 iPhone 的全部底层能力，基于 `idevice`。
//!
//! - 发现：usbmuxd `get_devices`；容器里没有 udev，另用 sysfs 数 USB 上有几台 iPhone/iPad（[`ios_usb_count`]）
//! - 配对、设备信息、开发者模式：lockdown / amfi
//! - 隧道：CoreDeviceProxy + jktcp 用户态 TCP 栈（免 root、不建网卡），RSD 握手（[`Tunnel`]）
//! - 开发者镜像：`cryptexd::installed_ddi` / `install_ddi`（个性化 + TSS）；DDI 下载见 [`ddi`]
//! - HID：universalhidservice（触摸）、indigo（按键、键盘）；**必须同时持有显示媒体流**（[`hid::HidSession`]）
//! - 截图：CoreDevice screencaptureservice
//! - App：installation_proxy（列出时过滤 `SBAppTags` 含 `hidden` 的后台服务，见 [`apps`]）、安装 IPA
//!
//! 不负责：WDA（见 `ictl-wda`）、状态机（见 `ictl-daemon`）。

pub mod apps;
pub mod ddi;
pub mod hid;
mod media;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use idevice::{
    IdeviceError, IdeviceService, RsdService,
    amfi::AmfiClient,
    core_device::{ImageFormat, ScreenCaptureServiceClient},
    core_device_proxy::CoreDeviceProxy,
    cryptexd::{Cryptex1Assets, install_ddi, installed_ddi},
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    misagent::MisagentClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr},
};

pub use apps::AppInfo;
pub use hid::{Button, HidSession};

/// 连接标签，出现在设备日志里
pub const LABEL: &str = "iphone-control";

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("{}", ictl_l10n::tr!("设备未连接", "Device not connected"))]
    NotConnected,
    #[error("{}", ictl_l10n::tr!("设备未配对，需要在手机上点「信任」", "Device not paired: tap “Trust” on the phone"))]
    NotPaired,
    #[error("{}", ictl_l10n::tr!("手机上的「信任此电脑」弹窗还在等待确认", "The “Trust This Computer” prompt on the phone is still waiting for an answer"))]
    PairingPending,
    #[error("{}", ictl_l10n::tr!("用户在手机上拒绝了配对", "Pairing was declined on the phone"))]
    PairingDenied,
    #[error("{}", ictl_l10n::tr!("开发者模式未开启", "Developer Mode is off"))]
    DeveloperModeOff,
    /// 锁屏且自开机以来从未解锁（BFU），或锁屏时建隧道，都会报这个
    #[error("{}", ictl_l10n::tr!("手机锁屏（PasswordProtected），需要解锁", "The phone is locked (PasswordProtected) and has to be unlocked"))]
    PasswordProtected,
    #[error("{}", ictl_l10n::trf!("下载开发者镜像失败：{0}", "Failed to download the developer disk image: {0}", .0))]
    Download(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{}", ictl_l10n::trf!("idevice：{0}", "idevice: {0}", .0))]
    Idevice(String),
    /// 手机上的服务没在限定时间内应答。调用方应丢掉隧道重建
    #[error("{}", ictl_l10n::trf!("超时：{0}", "Timed out: {0}", .0))]
    Timeout(String),
    /// 调用方给的参数不合法（例如 HID 不能输入的字符）。与连接无关，不需要重建会话
    #[error("{0}")]
    InvalidInput(String),
}

/// usbmuxd、lockdown 这类一问一答的查询的时限。idevice 读应答没有超时，对方接了连接不应答会一直等
const QUERY: Duration = Duration::from_secs(15);

/// 给对手机的一次调用加时限：卡住的服务不能把调用方（守护进程持锁时）一起卡死
pub(crate) async fn limit<T>(what: &str, d: Duration, f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(d, f)
        .await
        .map_err(|_| DeviceError::Timeout(ictl_l10n::trf!("{what}（{} 秒）", "{what} ({} s)", d.as_secs())))?
}

impl From<IdeviceError> for DeviceError {
    fn from(e: IdeviceError) -> Self {
        match e {
            IdeviceError::PasswordProtected => Self::PasswordProtected,
            IdeviceError::DeviceNotFound => Self::NotConnected,
            IdeviceError::InvalidHostID => Self::NotPaired,
            IdeviceError::PairingDialogResponsePending => Self::PairingPending,
            IdeviceError::UserDeniedPairing => Self::PairingDenied,
            IdeviceError::DeveloperModeNotEnabled => Self::DeveloperModeOff,
            other => Self::Idevice(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, DeviceError>;

/// 屏幕逻辑尺寸（点）
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScreenSize {
    pub width: f64,
    pub height: f64,
}

impl ScreenSize {
    /// 设备竖放时的尺寸（短边是宽）
    pub fn portrait(self) -> Self {
        Self { width: self.width.min(self.height), height: self.width.max(self.height) }
    }
}

/// 界面相对设备竖放转了多少（WDA `GET /rotation` 的 z）。HID 触摸始终按设备竖放的坐标解释，
/// 横屏时要把界面上的点换算回去。真机：iPhone 15 Pro Max 摄像头在左 z=270、在右 z=90；
/// iPad mini 5 Home 键在左 z=90。两个横屏方向 WDA 的 `/orientation` 都报 `LANDSCAPE`，分不出来
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Portrait,
    /// z=90：竖放的顶边转到了屏幕右边
    TopToRight,
    /// z=180
    UpsideDown,
    /// z=270：竖放的顶边转到了屏幕左边
    TopToLeft,
}

impl Rotation {
    pub fn from_degrees(z: f64) -> Option<Self> {
        if !z.is_finite() || z.fract() != 0.0 {
            return None;
        }
        Some(match z.rem_euclid(360.0) as u16 {
            0 => Self::Portrait,
            90 => Self::TopToRight,
            180 => Self::UpsideDown,
            270 => Self::TopToLeft,
            _ => return None,
        })
    }

    pub fn is_landscape(self) -> bool {
        matches!(self, Self::TopToRight | Self::TopToLeft)
    }

    /// 界面上的点 → 设备竖放时的点。`portrait`：竖放时的屏幕尺寸
    pub fn to_portrait(self, (x, y): (f64, f64), portrait: ScreenSize) -> (f64, f64) {
        let (w, h) = (portrait.width, portrait.height);
        match self {
            Self::Portrait => (x, y),
            Self::UpsideDown => (w - x, h - y),
            Self::TopToLeft => (w - y, x),
            Self::TopToRight => (y, h - x),
        }
    }
}

/// usbmuxd 报告的一台 USB 设备
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub udid: String,
    pub device_id: u32,
}

/// 设备基本信息（lockdown 不开会话就能读）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub product_type: String,
    pub product_version: String,
}

impl DeviceInfo {
    /// iOS 主版本号，读不出返回 0
    pub fn major_version(&self) -> u32 {
        self.product_version.split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
}

/// usbmuxd 地址：环境变量 `USBMUXD_SOCKET_ADDRESS` 优先，否则默认 `/var/run/usbmuxd`
pub fn usbmuxd_addr() -> UsbmuxdAddr {
    UsbmuxdAddr::from_env_var().unwrap_or_default()
}

/// sysfs 里 iPhone/iPad/iPod 的台数：厂商号 05ac、产品名以 iPhone/iPad/iPod 开头（Apple 键盘之类的配件不算）。
/// `root` 一般是 `/sys/bus/usb/devices`。
/// 接着几台时和 usbmuxd 认到的台数比，才看得出「新插的那台没被认出来」
pub fn ios_usb_count(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let read = |p: &Path, f: &str| std::fs::read_to_string(p.join(f)).map(|v| v.trim().to_owned()).unwrap_or_default();
    entries
        .flatten()
        .filter(|e| {
            let p = e.path();
            read(&p, "idVendor") == "05ac"
                && ["iPhone", "iPad", "iPod"].iter().any(|n| read(&p, "product").starts_with(n))
        })
        .count()
}

/// Linux 上 USB 设备在 sysfs 里的目录
pub const SYSFS_USB: &str = "/sys/bus/usb/devices";

/// 宿主机 USB 上有没有 iPhone/iPad：Linux 读 sysfs（[`ios_usb_count`]），macOS 查 IORegistry。
/// 判断不了（其他系统、`ioreg` 运行失败）返回 `None`，调用方应跳过这项检查而不是当成「没有」。
pub async fn host_ios_usb_present() -> Option<bool> {
    if cfg!(target_os = "linux") {
        return Some(ios_usb_count(Path::new(SYSFS_USB)) > 0);
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    // Mac 上没有 sysfs；Apple Silicon 自带的 Apple 设备（键盘、触控板）也不在 IOUSB 平面上，但外接的 Apple 配件会在，
    // 所以按节点名认手机而不是只看厂商号
    let out = tokio::process::Command::new("/usr/sbin/ioreg").args(["-p", "IOUSB", "-w0"]).output().await.ok()?;
    out.status.success().then(|| ioreg_has_ios_device(&String::from_utf8_lossy(&out.stdout)))
}

/// `ioreg -p IOUSB` 的输出里有没有 iPhone/iPad/iPod 节点（形如 `+-o iPhone@00100000  <class IOUSBHostDevice, …>`）
fn ioreg_has_ios_device(out: &str) -> bool {
    out.lines().filter_map(|l| l.split("+-o ").nth(1)).any(|node| {
        let name = node.split('@').next().unwrap_or("");
        ["iPhone", "iPad", "iPod"].contains(&name)
    })
}

/// usbmuxd 看到的 USB 设备（不含网络设备）。usbmuxd 没在运行时返回错误。
pub async fn list_devices() -> Result<Vec<UsbDevice>> {
    limit(ictl_l10n::tr!("列出 usbmuxd 设备", "list usbmuxd devices"), QUERY, async {
        // 不用 UsbmuxdConnection::default()：它固定连 /var/run/usbmuxd，不看 USBMUXD_SOCKET_ADDRESS
        let mut mux = usbmuxd_addr().connect(0).await?;
        Ok(mux
            .get_devices()
            .await?
            .into_iter()
            .filter(|d| matches!(d.connection_type, Connection::Usb))
            .map(|d| UsbDevice { udid: d.udid, device_id: d.device_id })
            .collect())
    })
    .await
}

/// 一台设备的句柄。只持有 usbmuxd provider，每次调用按需新建连接；隧道见 [`Device::open_tunnel`]。
#[derive(Debug, Clone)]
pub struct Device {
    pub udid: String,
    provider: Arc<UsbmuxdProvider>,
}

impl Device {
    /// `udid` 为空时取 usbmuxd 看到的第一台 USB 设备
    pub async fn find(udid: Option<&str>) -> Result<Self> {
        let devices = list_devices().await?;
        let dev = match udid {
            Some(u) => devices.into_iter().find(|d| d.udid == u),
            None => devices.into_iter().next(),
        }
        .ok_or(DeviceError::NotConnected)?;
        Ok(Self::from_usb(&dev))
    }

    /// 用 [`list_devices`] 列出的一项建句柄（不连 usbmuxd）
    pub fn from_usb(dev: &UsbDevice) -> Self {
        let provider = UsbmuxdProvider {
            addr: usbmuxd_addr(),
            tag: dev.device_id,
            udid: dev.udid.clone(),
            device_id: dev.device_id,
            label: LABEL.into(),
        };
        Self { udid: dev.udid.clone(), provider: Arc::new(provider) }
    }

    /// usbmuxd 给这次连接分的编号。拔插、USB 重新枚举后会变（UDID 不变），旧编号的连接全部失效
    pub fn device_id(&self) -> u32 {
        self.provider.device_id
    }

    /// 是不是 `dev` 这一次连接：UDID 和 usbmuxd 编号都要相同
    pub fn is_connection(&self, dev: &UsbDevice) -> bool {
        self.udid == dev.udid && self.device_id() == dev.device_id
    }

    /// 给 XCTest、WDA 等需要自己建连接的组件用
    pub fn provider(&self) -> Arc<dyn IdeviceProvider> {
        self.provider.clone()
    }

    /// usbmuxd 里有没有这台手机的配对记录。只有「没有记录」算 false；连不上 usbmuxd 等错误照常返回，
    /// 免得 usbmuxd 抖一下就让状态变成「请在手机上点信任」
    pub async fn paired(&self) -> Result<bool> {
        limit(ictl_l10n::tr!("查配对记录", "check the pairing record"), QUERY, async {
            let mut mux = self.provider.addr.connect(0).await?;
            match mux.get_pair_record(&self.udid).await {
                Ok(_) => Ok(true),
                // 没有记录时 usbmuxd 的应答里没有 PairRecordData；记录损坏同样需要重新配对
                Err(IdeviceError::UnexpectedResponse(_)) => Ok(false),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// 发起配对：手机上弹「信任此电脑」。用户还没点时返回 [`DeviceError::PairingPending`]，调用方应轮询重试。
    /// 成功后配对记录交给 usbmuxd 保存（容器里即 `/var/lib/lockdown`）。
    pub async fn pair(&self, host_name: &str) -> Result<()> {
        limit(ictl_l10n::tr!("配对", "pair"), Duration::from_secs(30), async {
            let mut mux = self.provider.addr.connect(0).await?;
            let buid = mux.get_buid().await?;
            let mut lockdown = LockdownClient::connect(&*self.provider).await?;
            let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
            let record = lockdown.pair_once(host_id, buid, Some(host_name)).await?;
            let mut mux = self.provider.addr.connect(0).await?;
            mux.save_pair_record(&self.udid, record.serialize()?).await?;
            Ok(())
        })
        .await
    }

    /// 名称、型号、iOS 版本（不需要配对）
    pub async fn info(&self) -> Result<DeviceInfo> {
        limit(ictl_l10n::tr!("读设备信息", "read device info"), QUERY, self.info_inner()).await
    }

    async fn info_inner(&self) -> Result<DeviceInfo> {
        let mut lockdown = LockdownClient::connect(&*self.provider).await?;
        let mut get = async |key: &str| -> String {
            match lockdown.get_value(Some(key), None).await {
                Ok(v) => v.as_string().map(str::to_owned).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(
                        "{}",
                        ictl_l10n::trf!("读设备信息 {key} 失败：{e}", "Reading device info {key} failed: {e}")
                    );
                    String::new()
                }
            }
        };
        Ok(DeviceInfo {
            udid: self.udid.clone(),
            name: get("DeviceName").await,
            product_type: get("ProductType").await,
            product_version: get("ProductVersion").await,
        })
    }

    /// 手机上装着的全部描述文件（.mobileprovision 原始内容，经 misagent 读）。装 App 时随包带的描述文件会留在这里，
    /// 用它判断「这台手机上的 WDA 还能用多久」：同一个 Apple ID 给几台手机先后签过，每台装的是各自那一次签的
    pub async fn installed_profiles(&self) -> Result<Vec<Vec<u8>>> {
        limit(ictl_l10n::tr!("读描述文件", "read provisioning profiles"), QUERY, async {
            Ok(MisagentClient::connect(&*self.provider).await?.copy_all().await?)
        })
        .await
    }

    pub async fn developer_mode_enabled(&self) -> Result<bool> {
        limit(ictl_l10n::tr!("查开发者模式", "check Developer Mode"), QUERY, async {
            Ok(AmfiClient::connect(&*self.provider).await?.get_developer_mode_status().await?)
        })
        .await
    }

    /// 让「设置 → 隐私与安全性」里出现开发者模式开关（打开仍需用户在手机上操作）
    pub async fn reveal_developer_mode(&self) -> Result<()> {
        limit(ictl_l10n::tr!("显示开发者模式开关", "reveal the Developer Mode switch"), QUERY, async {
            Ok(AmfiClient::connect(&*self.provider).await?.reveal_developer_mode_option_in_ui().await?)
        })
        .await
    }

    /// 建立 CoreDeviceProxy 用户态隧道并完成 RSD 握手。锁屏时报 [`DeviceError::PasswordProtected`]。
    pub async fn open_tunnel(&self) -> Result<Tunnel> {
        limit(ictl_l10n::tr!("建隧道", "open the tunnel"), Duration::from_secs(30), async {
            let proxy = CoreDeviceProxy::connect(&*self.provider).await?;
            let rsd_port = proxy.tunnel_info().server_rsd_port;
            let mut adapter = proxy.create_software_tunnel()?.to_async_handle();
            let stream = adapter.connect(rsd_port).await?;
            let handshake = RsdHandshake::new(stream).await?;
            Ok(Tunnel { adapter, handshake, hid: None })
        })
        .await
    }

    /// 已安装的 App。`include_system` 为 false 只列用户 App，为 true 只列系统 App（与 toolkit 一致）。
    pub async fn list_apps(&self, include_system: bool) -> Result<Vec<AppInfo>> {
        limit(ictl_l10n::tr!("列出 App", "list apps"), Duration::from_secs(30), async {
            let mut proxy = InstallationProxyClient::connect(&*self.provider).await?;
            let kind = if include_system { "System" } else { "User" };
            Ok(apps::parse_apps(proxy.get_apps(Some(kind), None).await?))
        })
        .await
    }

    /// 经 usbmuxd 直连手机上的 TCP 端口（`iphone-control wda-bridge` 用）
    pub async fn connect_port(&self, port: u16) -> Result<Box<dyn idevice::ReadWrite>> {
        let dev = limit(ictl_l10n::tr!("连接手机端口", "connect to a port on the phone"), QUERY, async {
            Ok(self.provider.connect(port).await?)
        })
        .await?;
        dev.get_socket().ok_or_else(|| {
            DeviceError::Idevice(ictl_l10n::tr!("连接没有套接字", "the connection has no socket").into())
        })
    }

    /// 上传并安装 .ipa 或 .app 目录
    pub async fn install(&self, package: &Path) -> Result<()> {
        // 含上传几十 MB 的包，USB 2 上也就一两分钟
        limit(ictl_l10n::tr!("安装", "install"), Duration::from_secs(600), async {
            idevice::utils::installation::install_package(&*self.provider, package, None).await?;
            Ok(())
        })
        .await
    }
}

/// 一条活着的隧道。HID、截图、cryptexd 都经它访问 RSD 服务；丢掉它隧道就断了。
///
/// HID 会话归隧道所有：它持有屏幕媒体流，必须在隧道断开之前停掉，否则手机端一直挂着这个会话
/// （灵动岛「配对的电脑」图标常驻、不会自动锁屏，进程退出和拔插数据线都清不掉）。
/// 所以不要直接丢掉 `Tunnel`，用 [`Tunnel::close`]。
pub struct Tunnel {
    adapter: AdapterHandle,
    handshake: RsdHandshake,
    hid: Option<HidSession>,
}

impl Tunnel {
    /// RSD 里是否提供某个服务（例如 iOS 27 的 `com.apple.coredevice.hid.universalhidservice`）
    pub fn has_service(&self, name: &str) -> bool {
        self.handshake.services.contains_key(name)
    }

    /// 已挂载的开发者镜像版本，未挂载返回 None
    pub async fn ddi_mounted(&mut self) -> Result<Option<String>> {
        limit(ictl_l10n::tr!("查询开发者镜像", "query the developer disk image"), Duration::from_secs(20), async {
            Ok(installed_ddi(&mut self.adapter, &mut self.handshake).await?.map(|c| c.version))
        })
        .await
    }

    /// 个性化并安装开发者镜像。`restore_dir` 见 [`ddi::ensure_cache`]
    pub async fn mount_ddi(&mut self, restore_dir: &Path) -> Result<String> {
        let assets = Cryptex1Assets::load(restore_dir).await?;
        // 含 TSS 联网签票
        limit(ictl_l10n::tr!("挂载开发者镜像", "mount the developer disk image"), Duration::from_secs(180), async {
            Ok(install_ddi(&mut self.adapter, &mut self.handshake, &assets).await?.version)
        })
        .await
    }

    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        limit(ictl_l10n::tr!("截图", "screenshot"), Duration::from_secs(30), async {
            let mut client = ScreenCaptureServiceClient::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
            Ok(client.take_screenshot(None, ImageFormat::Png).await?)
        })
        .await
    }

    /// HID 会话；没有就开一个：先启动显示媒体流（HID 的授权闸门），再连 universalhidservice 和 indigo
    pub async fn hid(&mut self, screen: ScreenSize) -> Result<&mut HidSession> {
        if self.hid.is_none() {
            // 各步自带时限（见 HidSession::open）：不能在这里整体取消，否则起来一半的媒体流没人停
            let h = HidSession::open(&mut self.adapter, &mut self.handshake, screen).await?;
            self.hid = Some(h);
        }
        let h = self.hid.as_mut().expect("刚建好");
        h.screen = screen.portrait();
        Ok(h)
    }

    pub fn hid_open(&self) -> bool {
        self.hid.is_some()
    }

    /// 停掉 HID 会话（含屏幕媒体流），隧道保留
    pub async fn close_hid(&mut self) {
        if let Some(h) = self.hid.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), h.close()).await;
        }
    }

    /// 关掉隧道：先停 HID 的屏幕媒体流
    pub async fn close(mut self) {
        self.close_hid().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机：摄像头在左（z=270）时，HID 按竖放发 (265,314) 落在了界面的 (314,165)，反过来要换算回 (265,314)
    #[test]
    fn rotation_maps_screen_points_back_to_portrait() {
        let p = ScreenSize { width: 932.0, height: 430.0 }.portrait();
        assert_eq!(p, ScreenSize { width: 430.0, height: 932.0 });
        assert_eq!(Rotation::from_degrees(270.0), Some(Rotation::TopToLeft));
        assert_eq!(Rotation::from_degrees(90.0), Some(Rotation::TopToRight));
        assert_eq!(Rotation::from_degrees(-90.0), Some(Rotation::TopToLeft));
        assert_eq!(Rotation::from_degrees(0.0), Some(Rotation::Portrait));
        assert_eq!(Rotation::from_degrees(45.0), None);
        for z in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.4, 90.1] {
            assert_eq!(Rotation::from_degrees(z), None);
        }
        assert_eq!(Rotation::TopToLeft.to_portrait((314.0, 165.0), p), (265.0, 314.0));
        // 顶边在右：界面左上角是竖放时的左下角
        assert_eq!(Rotation::TopToRight.to_portrait((0.0, 0.0), p), (0.0, 932.0));
        assert_eq!(Rotation::TopToRight.to_portrait((226.0, 97.0), p), (97.0, 706.0));
        assert_eq!(Rotation::UpsideDown.to_portrait((0.0, 0.0), p), (430.0, 932.0));
        assert_eq!(Rotation::Portrait.to_portrait((12.0, 34.0), p), (12.0, 34.0));
    }

    #[test]
    fn ios_usb_count_counts_iphones_and_ipads_only() {
        let root = std::env::temp_dir().join(format!("ictl-sysfs-{}", std::process::id()));
        let dev = |name: &str, vendor: &str, product: &str| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("idVendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(d.join("product"), format!("{product}\n")).unwrap();
        };
        dev("5-1", "05ac", "iPhone");
        dev("7-1", "05ac", "iPad");
        dev("3-1", "05ac", "Magic Keyboard");
        dev("1-1", "2109", "USB2.0 Hub");
        // 接口目录没有 idVendor
        std::fs::create_dir_all(root.join("5-1:1.0")).unwrap();
        assert_eq!(ios_usb_count(&root), 2);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(ios_usb_count(&root), 0);
    }

    #[test]
    fn same_udid_new_mux_id_is_new_connection() {
        let first = UsbDevice { udid: "00008130-0019295E012B803A".into(), device_id: 2 };
        let d = Device::from_usb(&first);
        assert_eq!(d.device_id(), 2);
        assert!(d.is_connection(&first));
        // 拔插后 usbmuxd 重新编号：UDID 相同也要当成新连接
        assert!(!d.is_connection(&UsbDevice { device_id: 3, ..first.clone() }));
        assert!(!d.is_connection(&UsbDevice { udid: "other".into(), ..first }));
    }

    #[test]
    fn ioreg_finds_phone_node() {
        let with = "+-o Root  <class IORegistryEntry, id 0x100000100, retain 35>
  +-o AppleT8122USBXHCI@00000000  <class AppleT8122USBXHCI, id 0x1000003f8, registered>
  | +-o iPhone@00100000  <class IOUSBHostDevice, id 0x10009fe32, registered, matched>
  +-o AppleT8122USBXHCI@02000000  <class AppleT8122USBXHCI, id 0x100000424, registered>";
        assert!(ioreg_has_ios_device(with));
        let without = "+-o Root  <class IORegistryEntry>
  +-o AppleT8122USBXHCI@00000000  <class AppleT8122USBXHCI>
  | +-o Magic Keyboard@00100000  <class IOUSBHostDevice>";
        assert!(!ioreg_has_ios_device(without));
    }
}
