//! 守护进程：唯一持有设备的进程。替代 toolkit 的 supervisor.sh + 重签定时器 + notify.sh。
//!
//! - 状态机（[`machine::DeviceState`]，docs/architecture.md §3）每 10 秒推进一次（[`supervise`]）
//! - usbmuxd 作为子进程看护；容器无 udev，靠 sysfs 发现漏掉的设备并重启它
//! - 控制接口：Unix socket，每行一个 JSON（[`protocol`]），MCP 和 CLI 都通过它访问设备
//! - 并发模型见 [`core`]

pub mod config;
pub mod core;
pub mod devices;
mod doctor;
pub mod eventlog;
pub mod imaging;
pub mod machine;
pub mod notify;
mod supervise;
mod tools;
mod wda_pkg;

pub use ictl_protocol as protocol;

use std::sync::Arc;
use std::time::Instant;

use anyhow::bail;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::{JoinHandle, JoinSet};

pub use config::Config;
pub use core::{Daemon, Snapshot};

/// 一行请求的长度上限（正常不到 1KB；输入的文字也远小于这个）
const MAX_REQUEST_LINE: u64 = 1024 * 1024;

/// 运行守护进程直到收到 SIGTERM / SIGINT
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    cfg.prepare_dirs()?;
    let socket = cfg.socket.clone();
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 已有守护进程在用这个 socket 就退出：两个守护进程会争 usbmuxd、WDA 和 HID
    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        bail!(
            "{}",
            ictl_l10n::trf!("已经有守护进程在运行（{}）", "A daemon is already running ({})", socket.display())
        );
    }
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    // 处理设备的 future 不必是 Send（plume_core / 泛型 async 的 Send 推断有限制），全部跑在一个 LocalSet 里
    let local = tokio::task::LocalSet::new();
    let r = local.run_until(serve(cfg, listener)).await;
    let _ = std::fs::remove_file(&socket);
    r
}

async fn serve(cfg: Config, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
    let daemon = Daemon::new(cfg);
    daemon.log.log(ictl_l10n::tr!("守护进程启动", "Daemon started"));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tick = tokio::time::interval(core::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 每个连接一个任务；请求在任务里等设备锁，主循环本身从不阻塞，停止信号随时能处理
    let mut conns = JoinSet::new();
    let mut ticking: Option<JoinHandle<()>> = None;
    loop {
        tokio::select! {
            biased;
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            r = listener.accept() => match r {
                Ok((stream, _)) => {
                    conns.spawn_local(serve_conn(daemon.clone(), stream));
                }
                Err(e) => tracing::warn!("{}", ictl_l10n::trf!("控制接口 accept 失败：{e}", "Accepting on the control socket failed: {e}")),
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            _ = tick.tick() => {
                // 上一轮还没跑完（例如在重启 usbmuxd）就不叠加
                if ticking.as_ref().is_none_or(JoinHandle::is_finished) {
                    ticking = Some(tokio::task::spawn_local(daemon.clone().tick()));
                }
            }
        }
    }
    // 先取消排队和执行中的请求、巡检（放掉设备锁），再停 HID 媒体流、runner、usbmuxd。
    // 自动解锁被打断时停用标记会留着（ictl-unlock），不会因此输错第二次
    conns.abort_all();
    while conns.join_next().await.is_some() {}
    if let Some(t) = ticking {
        t.abort();
        let _ = t.await;
    }
    daemon.shutdown().await;
    Ok(())
}

async fn serve_conn(daemon: Arc<Daemon>, stream: tokio::net::UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_REQUEST_LINE + 1).read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let queued_at = Instant::now();
        let resp = if line.len() as u64 > MAX_REQUEST_LINE {
            protocol::Response::error(ictl_l10n::tr!("请求太长", "request too long"))
        } else {
            match parse_request(&line) {
                Ok((req, false, expect_udid, lang)) => {
                    let handling = daemon.handle(req, queued_at, core::CallCtx { gone: None, expect_udid });
                    ictl_l10n::scope(lang, handling).await
                }
                Ok((req, true, expect_udid, lang)) => {
                    // 调用方断开（取消、超时）就告诉 handle：还在排队的话不再执行
                    let (gone_tx, gone_rx) = tokio::sync::oneshot::channel();
                    let mut gone_tx = Some(gone_tx);
                    let handling = ictl_l10n::scope(
                        lang,
                        daemon.handle(req, queued_at, core::CallCtx { gone: Some(gone_rx), expect_udid }),
                    );
                    tokio::pin!(handling);
                    let resp = loop {
                        tokio::select! {
                            r = &mut handling => break r,
                            () = wait_closed(&mut reader), if gone_tx.is_some() => {
                                let _ = gone_tx.take().map(|t| t.send(()));
                            }
                        }
                    };
                    if gone_tx.is_none() {
                        break;
                    }
                    resp
                }
                Err(e) => protocol::Response::error(ictl_l10n::trf!("请求格式错误：{e}", "malformed request: {e}")),
            }
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() || line.len() as u64 > MAX_REQUEST_LINE {
            break;
        }
    }
}

/// 解析一行请求，并取出 [`protocol::CANCEL_ON_CLOSE`]、[`protocol::EXPECT_UDID`]、[`protocol::LANG`]。
/// No language (an older client) means Chinese, which is what those clients expect and parse
fn parse_request(line: &[u8]) -> serde_json::Result<(protocol::Request, bool, Option<String>, ictl_l10n::Lang)> {
    let mut v: serde_json::Value = serde_json::from_slice(line)?;
    let (cancel, expect, lang) = match v.as_object_mut() {
        Some(o) => (
            o.remove(protocol::CANCEL_ON_CLOSE).is_some_and(|c| c == true),
            o.remove(protocol::EXPECT_UDID).and_then(|u| u.as_str().map(str::to_owned)),
            o.remove(protocol::LANG).and_then(|l| l.as_str().and_then(ictl_l10n::parse)),
        ),
        None => (false, None, None),
    };
    Ok((serde_json::from_value(v)?, cancel, expect, lang.unwrap_or(ictl_l10n::Lang::Zh)))
}

/// 等到调用方关掉连接（读到 EOF 或出错）。带 cancel_on_close 的客户端一个连接只发一个请求，之后不会再写
async fn wait_closed<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) {
    let mut buf = [0u8; 64];
    while let Ok(n) = reader.read(&mut buf).await
        && n > 0
    {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_options_are_taken_out_before_parsing() {
        use ictl_l10n::Lang;
        let (req, cancel, expect, lang) =
            parse_request(br#"{"op":"tap","x":1.0,"y":2.0,"cancel_on_close":true,"expect_udid":"U1","lang":"en"}"#)
                .unwrap();
        assert_eq!(req, protocol::Request::Tap { x: 1.0, y: 2.0 });
        assert!(cancel);
        assert_eq!(expect.as_deref(), Some("U1"));
        assert_eq!(lang, Lang::En);
        // 旧版本客户端：没有这几个键；没说语言就是中文（它们按中文的固定写法认结果）
        let (_, cancel, expect, lang) = parse_request(br#"{"op":"status"}"#).unwrap();
        assert!(!cancel && expect.is_none());
        assert_eq!(lang, Lang::Zh);
        let (_, _, _, lang) = parse_request(br#"{"op":"status","lang":"klingon"}"#).unwrap();
        assert_eq!(lang, Lang::Zh);
        assert!(parse_request(b"{").is_err());
    }
}
