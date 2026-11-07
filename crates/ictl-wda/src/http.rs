//! 发往 WDA 的极简 HTTP/1.1。
//!
//! WDA 直接经 usbmuxd 连手机上的端口，本机不监听任何端口（docs/decisions.md D3）。
//! idevice 自带的 `WdaClient` 不支持 `excluded_attributes`、appium settings、W3C actions 等，
//! 所以这里自己发请求：每个请求一条新连接，读够一个完整响应（Content-Length / chunked 结束块）就关掉。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use idevice::provider::IdeviceProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 一次 HTTP 请求的传输。抽象出来便于单测里换成假的。
///
/// 错误的 kind 区分请求有没有送到 WDA（决定能不能退到 HID 重做）：
/// [`ErrorKind::TimedOut`](std::io::ErrorKind::TimedOut) 是发出后没在时限内应答，
/// [`AFTER_SENT`] 是发出后连接断了或响应不完整、看不懂；其他 kind 都表示请求没送到
pub trait Transport: Send + Sync {
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> impl Future<Output = std::io::Result<(u16, Vec<u8>)>> + Send;
}

/// 经 usbmuxd 直连手机上 WDA 端口
#[derive(Clone)]
pub struct DeviceTransport {
    provider: Arc<dyn IdeviceProvider>,
    port: u16,
}

impl DeviceTransport {
    pub fn new(provider: Arc<dyn IdeviceProvider>, port: u16) -> Self {
        Self { provider, port }
    }
}

/// 建立到 WDA 端口的连接的时限。连不上（或连接超时）算 WDA 不可用，调用方可以退到 HID；
/// 连上之后的超时是「请求已发出、手机可能还在执行」，不能退到 HID 重做一遍
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求已经整个发出、之后读响应出错的 kind：WDA 可能已经执行了这个操作
pub const AFTER_SENT: std::io::ErrorKind = std::io::ErrorKind::ConnectionAborted;

fn after_sent(e: std::io::Error) -> std::io::Error {
    std::io::Error::new(AFTER_SENT, e.to_string())
}

impl Transport for DeviceTransport {
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let t0 = std::time::Instant::now();
        let req = build_request(method, path, body.as_deref());
        let idevice = tokio::time::timeout(CONNECT_TIMEOUT, self.provider.connect(self.port))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    ictl_l10n::tr!("连接 WDA 端口超时", "connecting to the WDA port timed out"),
                )
            })?
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotConnected, e.to_string()))?;
        let mut sock = idevice.get_socket().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                ictl_l10n::tr!("连接没有套接字", "the connection has no socket"),
            )
        })?;
        let fut = async {
            // 没写完 WDA 不会处理（HTTP 请求不完整），算没送到
            let not_sent = |e: std::io::Error| std::io::Error::new(std::io::ErrorKind::NotConnected, e.to_string());
            sock.write_all(&req).await.map_err(not_sent)?;
            sock.flush().await.map_err(not_sent)?;
            // WDA 收到 `Connection: close` 也不关连接，不能读到 EOF：读够一个完整响应就停。
            // 头部只解析一次；正文按 Content-Length 判断，chunked 只在收到结束块后才完整解一次
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            let mut head: Option<(usize, Framing)> = None;
            loop {
                if head.is_none() {
                    head = parse_head(&buf);
                    if let Some((split, Framing::Length(n))) = head {
                        buf.reserve((split + n).saturating_sub(buf.len()));
                    }
                }
                if let Some((split, framing)) = head
                    && framing.complete(&buf[split..])
                {
                    break;
                }
                let n = sock.read(&mut chunk).await.map_err(after_sent)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            parse_response(buf).map_err(after_sent)
        };
        let r = tokio::time::timeout(timeout, fut).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                ictl_l10n::trf!("请求超时（{} 秒）", "request timed out ({} s)", timeout.as_secs()),
            )
        })?;
        // 路径截掉查询串（/source 的排除属性很长）
        let short = path.split('?').next().unwrap_or(path);
        match &r {
            Ok((status, _)) => tracing::debug!(
                "{}",
                ictl_l10n::trf!(
                    "wda {method} {short} → {status}（{:.2}s）",
                    "wda {method} {short} → {status} ({:.2}s)",
                    t0.elapsed().as_secs_f64()
                )
            ),
            Err(e) => tracing::debug!(
                "{}",
                ictl_l10n::trf!(
                    "wda {method} {short} 失败（{:.2}s）：{e}",
                    "wda {method} {short} failed ({:.2}s): {e}",
                    t0.elapsed().as_secs_f64()
                )
            ),
        }
        r
    }
}

pub(crate) fn build_request(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let body = body.unwrap_or_default();
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n");
    if method != "GET" || !body.is_empty() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    out
}

/// 响应正文怎么定长
#[derive(Debug, Clone, Copy, PartialEq)]
enum Framing {
    Length(usize),
    Chunked,
    /// 两者都没有：只能读到对方关连接
    UntilEof,
}

impl Framing {
    fn complete(self, body: &[u8]) -> bool {
        match self {
            Self::Length(n) => body.len() >= n,
            // 先看结尾是不是结束块，是了才完整解一遍（避免每读 64KB 就把整个正文解一遍）
            Self::Chunked => body.ends_with(b"0\r\n\r\n") && dechunk(body).is_some(),
            Self::UntilEof => false,
        }
    }
}

/// 头部收齐了就返回 (正文起点, 定长方式)。头部很短，只在前 64KB 里找结束标记
fn parse_head(raw: &[u8]) -> Option<(usize, Framing)> {
    let window = &raw[..raw.len().min(64 * 1024)];
    let split = window.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let mut framing = Framing::UntilEof;
    for l in head.split("\r\n").skip(1) {
        let Some((k, v)) = l.split_once(':') else { continue };
        match (k.trim(), v.trim()) {
            ("content-length", n) => {
                if let Ok(n) = n.parse() {
                    framing = Framing::Length(n);
                }
            }
            ("transfer-encoding", "chunked") => return Some((split + 4, Framing::Chunked)),
            _ => {}
        }
    }
    Some((split + 4, framing))
}

/// 已经收到一个完整的响应了吗：头部结束，并且正文按 Content-Length 收齐或 chunked 收到结束块。
/// 两者都没有时只能等对方关连接（返回 false 继续读）。读循环里直接用 [`parse_head`] + [`Framing::complete`]
#[cfg(test)]
fn response_complete(raw: &[u8]) -> bool {
    parse_head(raw).is_some_and(|(split, f)| f.complete(&raw[split..]))
}

/// 解析完整的 HTTP 响应：状态码和正文。支持 Content-Length、chunked 和读到 EOF 三种正文。
/// 拿走缓冲区，正文原地截取不再复制（截图正文约 11MB）
pub(crate) fn parse_response(raw: impl Into<Vec<u8>>) -> std::io::Result<(u16, Vec<u8>)> {
    let mut raw = raw.into();
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let (split, framing) = parse_head(&raw)
        .ok_or_else(|| bad(ictl_l10n::tr!("HTTP 响应没有头部结束标记", "HTTP response has no end of headers")))?;
    let status: u16 = std::str::from_utf8(&raw[..split])
        .map_err(|_| bad(ictl_l10n::tr!("HTTP 头不是 UTF-8", "HTTP headers are not UTF-8")))?
        .split("\r\n")
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad(ictl_l10n::tr!("HTTP 状态行无效", "invalid HTTP status line")))?;
    match framing {
        Framing::Chunked => Ok((
            status,
            dechunk(&raw[split..])
                .ok_or_else(|| bad(ictl_l10n::tr!("chunked 正文不完整", "incomplete chunked body")))?,
        )),
        Framing::Length(n) if raw.len() - split < n => Err(bad(ictl_l10n::tr!(
            "正文比 Content-Length 短，连接提前断了",
            "body shorter than Content-Length, the connection closed early"
        ))),
        Framing::Length(n) => {
            raw.truncate(split + n);
            raw.drain(..split);
            Ok((status, raw))
        }
        Framing::UntilEof => {
            raw.drain(..split);
            Ok((status, raw))
        }
    }
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_has_length_only_with_body() {
        let get = String::from_utf8(build_request("GET", "/status", None)).unwrap();
        assert!(get.starts_with("GET /status HTTP/1.1\r\n"));
        assert!(!get.contains("Content-Length"));
        let post = String::from_utf8(build_request("POST", "/session", Some(b"{}"))).unwrap();
        assert!(post.contains("Content-Length: 2\r\n") && post.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: x\r\n\r\nhelloEXTRA";
        assert_eq!(parse_response(raw).unwrap(), (200, b"hello".to_vec()));
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (404, b"abcde".to_vec()));
    }

    #[test]
    fn completeness_does_not_wait_for_eof() {
        // WDA 回 Content-Length 但不关连接：收齐正文就算完整
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n"));
        // 两者都没有：只能读到 EOF
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\n{}"));
    }

    #[test]
    fn body_until_eof_and_truncated_body() {
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n\r\n{}").unwrap(), (200, b"{}".to_vec()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse_response(b"garbage").is_err());
    }
}
