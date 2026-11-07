//! `iphone-control debug …`：真机诊断（`--features debug-cmds` 才编译）。直接调用设备层和 WDA 客户端，
//! 不经守护进程。`wda`、`e2e` 会启动 XCTest、`hid` 会开屏幕媒体流，和守护进程同时跑会互相干扰：
//! 先停掉守护进程（例如在另一个只跑 usbmuxd 的容器里运行）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use ictl_device::{Button, Device, ScreenSize};
use ictl_wda::client::SPRINGBOARD;

#[derive(Args)]
pub struct DebugArgs {
    /// 屏幕点尺寸（HID 坐标换算用；默认是 iPhone 15 Pro Max，别的机型按 `iphone-control status` 里的屏幕尺寸传）
    #[arg(long, default_value_t = 430.0)]
    width: f64,
    #[arg(long, default_value_t = 932.0)]
    height: f64,
    #[command(subcommand)]
    check: Check,
}

#[derive(Subcommand)]
enum Check {
    /// 1. 开发者镜像：已挂载就报告版本；否则用 restore_dir（缺文件会下载）个性化 + TSS 并挂载
    Ddi {
        restore_dir: PathBuf,
        #[arg(long, default_value = ictl_device::ddi::DEFAULT_BASE_URL)]
        url: String,
    },
    /// 2. HID：媒体流 + universalhidservice tap，再按 Home（以截图对比为准，调用成功不代表生效）
    Hid { x: f64, y: f64 },
    /// 3. 截图：screencaptureservice PNG
    Screenshot { out: PathBuf },
    /// 4. 隧道稳定性：同一条隧道持续截图 N 分钟
    Soak {
        #[arg(long, default_value_t = 30)]
        minutes: u64,
    },
    /// 5. 启动 WDA：读 /status、/source、/screenshot，最后停掉 runner
    Wda {
        bundle_id: String,
        /// 等 WDA 就绪的秒数（排查「Enable UI Automation」时调大）
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// 阶段 1 完成标准：隧道 → 镜像 → WDA → 截图 → 读元素 → 点「设置」→ Home，每步以 WDA 报告的前台 App 为准。
    /// 再用 HID 重做「点设置 → Home」一遍，确认 HID 点击真的生效
    E2e {
        bundle_id: String,
        /// 截图保存位置
        #[arg(long, default_value = "e2e.png")]
        out: PathBuf,
    },
}

const PREFERENCES: &str = "com.apple.Preferences";

/// 等前台 App 变成 `want`，最多 8 秒；返回最后看到的前台 App
async fn wait_foreground(wda: &ictl_wda::WdaClient<ictl_wda::http::DeviceTransport>, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let now = wda.active_app().await.unwrap_or_default();
        if now == want || Instant::now() >= deadline {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 一步的结果：打印并在失败时带上说明返回错误
fn check(step: &str, t0: Instant, ok: bool, detail: String) -> anyhow::Result<()> {
    let el = t0.elapsed().as_secs_f64();
    if ok {
        println!("{}", ictl_l10n::trf!("✓ {step}（{el:.2}s）{detail}", "✓ {step} ({el:.2}s){detail}"));
        Ok(())
    } else {
        println!("{}", ictl_l10n::trf!("✗ {step}（{el:.2}s）{detail}", "✗ {step} ({el:.2}s){detail}"));
        anyhow::bail!("{}", ictl_l10n::trf!("{step} 失败：{detail}", "{step} failed: {detail}"))
    }
}

async fn e2e(device: &Device, bundle_id: &str, out: &std::path::Path) -> anyhow::Result<()> {
    let t = Instant::now();
    let mut tunnel = device.open_tunnel().await?;
    check(ictl_l10n::tr!("建隧道", "open tunnel"), t, true, String::new())?;

    let t = Instant::now();
    let ddi = tunnel.ddi_mounted().await?;
    check(
        ictl_l10n::tr!("开发者镜像", "developer disk image"),
        t,
        ddi.is_some(),
        ddi.unwrap_or_else(|| {
            ictl_l10n::tr!("未挂载（先运行 ddi 子命令）", "not mounted (run the ddi subcommand first)").into()
        }),
    )?;

    let t = Instant::now();
    let (runner, mut wda) = ictl_wda::WdaRunner::start(device.provider(), bundle_id, Duration::from_secs(60)).await?;
    check(ictl_l10n::tr!("启动 WDA", "start WDA"), t, true, String::new())?;
    let r = e2e_with_wda(&mut tunnel, &mut wda, out).await;
    runner.stop();
    r
}

async fn e2e_with_wda(
    tunnel: &mut ictl_device::Tunnel,
    wda: &mut ictl_wda::WdaClient<ictl_wda::http::DeviceTransport>,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    // 先回主屏，保证「设置」图标在当前页
    let t = Instant::now();
    wda.press_button("home").await?;
    let now = wait_foreground(wda, SPRINGBOARD).await;
    check(
        ictl_l10n::tr!("回主屏", "home screen"),
        t,
        now == SPRINGBOARD,
        ictl_l10n::trf!("前台 {now}", "foreground {now}"),
    )?;

    let t = Instant::now();
    let png = wda.screenshot_png().await?;
    std::fs::write(out, &png)?;
    check(
        ictl_l10n::tr!("WDA 截图", "WDA screenshot"),
        t,
        png.starts_with(b"\x89PNG"),
        ictl_l10n::trf!("{} 字节 → {}", "{} bytes → {}", png.len(), out.display()),
    )?;

    let t = Instant::now();
    let els = wda.elements().await?;
    // 系统语言是中文或英文时主屏上「设置」的名字
    let icon = els.iter().find(|e| matches!(e.label.as_str(), "设置" | "Settings") && e.kind == "Icon").cloned();
    check(
        ictl_l10n::tr!("读元素", "read elements"),
        t,
        icon.is_some(),
        ictl_l10n::trf!(
            "{} 个元素；「设置」{}",
            "{} elements; “Settings” {}",
            els.len(),
            icon.as_ref().map_or(ictl_l10n::tr!("未找到", "not found").into(), |e| e.describe())
        ),
    )?;
    let (x, y) = icon.unwrap().center;
    let (x, y) = (x as f64, y as f64);

    let t = Instant::now();
    wda.tap(x, y).await?;
    let now = wait_foreground(wda, PREFERENCES).await;
    check(
        ictl_l10n::tr!("WDA 点「设置」", "WDA tap “Settings”"),
        t,
        now == PREFERENCES,
        ictl_l10n::trf!("前台 {now}", "foreground {now}"),
    )?;

    let t = Instant::now();
    wda.press_button("home").await?;
    let now = wait_foreground(wda, SPRINGBOARD).await;
    check("WDA Home", t, now == SPRINGBOARD, ictl_l10n::trf!("前台 {now}", "foreground {now}"))?;

    // HID 路线：同样的点击和 Home，以 WDA 的前台 App 判断是否生效
    let screen = wda.screen_size().await?;
    let t = Instant::now();
    let hid = tunnel.hid(screen).await?;
    check(ictl_l10n::tr!("打开 HID 会话", "open HID session"), t, true, String::new())?;
    let r = async {
        let t = Instant::now();
        hid.tap(x, y).await?;
        let now = wait_foreground(wda, PREFERENCES).await;
        check(
            ictl_l10n::tr!("HID 点「设置」", "HID tap “Settings”"),
            t,
            now == PREFERENCES,
            ictl_l10n::trf!("前台 {now}", "foreground {now}"),
        )?;
        let t = Instant::now();
        hid.press(Button::Home).await?;
        let now = wait_foreground(wda, SPRINGBOARD).await;
        check("HID Home", t, now == SPRINGBOARD, ictl_l10n::trf!("前台 {now}", "foreground {now}"))
    }
    .await;
    // 停掉屏幕媒体流，否则手机端一直挂着
    tunnel.close_hid().await;
    r
}

pub async fn run(udid: Option<&str>, args: DebugArgs) -> anyhow::Result<()> {
    let device = Device::find(udid).await?;
    let info = device.info().await?;
    println!("{}", ictl_l10n::trf!("手机 {} iOS {}", "Phone {} iOS {}", info.product_type, info.product_version));
    let screen = ScreenSize { width: args.width, height: args.height };
    match args.check {
        Check::Ddi { restore_dir, url } => {
            let mut t = device.open_tunnel().await?;
            match t.ddi_mounted().await? {
                Some(v) => println!(
                    "{}",
                    ictl_l10n::trf!(
                        "开发者镜像已挂载 {v}（要测挂载流程，先在手机重启后的未挂载状态下运行）",
                        "Developer disk image already mounted {v} (to test mounting, run it after a phone reboot while it is unmounted)"
                    )
                ),
                None => {
                    let dir = ictl_device::ddi::ensure_cache(&restore_dir, &url).await?;
                    let t0 = Instant::now();
                    let v = t.mount_ddi(&dir).await?;
                    println!(
                        "{}",
                        ictl_l10n::trf!(
                            "挂载成功 {v}，耗时 {:.1}s",
                            "Mounted {v} in {:.1}s",
                            t0.elapsed().as_secs_f64()
                        )
                    );
                }
            }
            let t = device.open_tunnel().await?;
            println!("universalhidservice: {}", t.has_service("com.apple.coredevice.hid.universalhidservice"));
        }
        Check::Hid { x, y } => {
            let mut t = device.open_tunnel().await?;
            let r = async {
                let hid = t.hid(screen).await?;
                hid.tap(x, y).await?;
                println!(
                    "{}",
                    ictl_l10n::trf!("已点 ({x},{y})，3 秒后按 Home", "Tapped ({x},{y}), pressing Home in 3 seconds")
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
                hid.press(Button::Home).await
            }
            .await;
            // 停掉屏幕媒体流，否则手机端一直挂着
            t.close().await;
            r?;
            println!(
                "{}",
                ictl_l10n::tr!(
                    "完成：请以截图/肉眼确认是否真的生效",
                    "Done: confirm with a screenshot or by eye that it really worked"
                )
            );
        }
        Check::Screenshot { out } => {
            let mut t = device.open_tunnel().await?;
            let t0 = Instant::now();
            let png = t.screenshot_png().await?;
            std::fs::write(&out, &png)?;
            println!(
                "{}",
                ictl_l10n::trf!(
                    "{} 字节，耗时 {:.2}s → {}",
                    "{} bytes in {:.2}s → {}",
                    png.len(),
                    t0.elapsed().as_secs_f64(),
                    out.display()
                )
            );
        }
        Check::Soak { minutes } => {
            let mut t = device.open_tunnel().await?;
            let end = Instant::now() + Duration::from_secs(minutes * 60);
            let (mut n, mut bytes, mut worst, mut fails) = (0u64, 0usize, 0f64, 0u64);
            while Instant::now() < end {
                let t0 = Instant::now();
                match t.screenshot_png().await {
                    Ok(p) => {
                        n += 1;
                        bytes += p.len();
                        worst = worst.max(t0.elapsed().as_secs_f64());
                    }
                    Err(e) => {
                        fails += 1;
                        eprintln!(
                            "{}",
                            ictl_l10n::trf!("截图失败：{e}，重建隧道", "Screenshot failed: {e}, reopening the tunnel")
                        );
                        t = device.open_tunnel().await?;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            println!(
                "{}",
                ictl_l10n::trf!(
                    "{n} 次成功、{fails} 次失败，共 {:.1} MB，最慢 {worst:.2}s",
                    "{n} succeeded, {fails} failed, {:.1} MB total, slowest {worst:.2}s",
                    bytes as f64 / 1e6
                )
            );
        }
        Check::Wda { bundle_id, timeout } => {
            let t0 = Instant::now();
            let (runner, mut wda) =
                ictl_wda::WdaRunner::start(device.provider(), &bundle_id, Duration::from_secs(timeout)).await?;
            println!("{}", ictl_l10n::trf!("WDA 就绪，耗时 {:.1}s", "WDA ready in {:.1}s", t0.elapsed().as_secs_f64()));
            println!("{}", ictl_l10n::trf!("屏幕 {:?}", "Screen {:?}", wda.screen_size().await?));
            let t1 = Instant::now();
            let xml = wda.source_xml().await?;
            println!(
                "{}",
                ictl_l10n::trf!(
                    "元素树 {} 字节，耗时 {:.2}s，解析出 {} 个元素",
                    "Element tree {} bytes in {:.2}s, {} elements parsed",
                    xml.len(),
                    t1.elapsed().as_secs_f64(),
                    wda.elements().await?.len()
                )
            );
            let t2 = Instant::now();
            println!(
                "{}",
                ictl_l10n::trf!(
                    "截图 {} 字节，耗时 {:.2}s",
                    "Screenshot {} bytes in {:.2}s",
                    wda.screenshot_png().await?.len(),
                    t2.elapsed().as_secs_f64()
                )
            );
            runner.stop();
            println!("{}", ictl_l10n::tr!("runner 已停止", "runner stopped"));
        }
        Check::E2e { bundle_id, out } => {
            let t0 = Instant::now();
            e2e(&device, &bundle_id, &out).await?;
            println!(
                "{}",
                ictl_l10n::trf!("全部通过，总耗时 {:.1}s", "All passed in {:.1}s", t0.elapsed().as_secs_f64())
            );
        }
    }
    Ok(())
}
