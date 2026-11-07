//! 未签名的 WDA 包。镜像里自带（docker/Dockerfile 的 wda 阶段）；从源码运行时没有它，
//! 第一次签名前按同一版本下载 Appium 官方构建的真机包、核对 sha256，重新打成 ipa 放进数据目录。
//! 版本和校验和必须与 Dockerfile 一致（单测核对）。

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub const WDA_VERSION: &str = "v16.15.3";
pub const WDA_SHA256: &str = "2176793538d902f6b1e61291e9997f68d306541f222ea715d256a7c6ce9d4fc5";
/// 镜像里自带的那份
pub const IMAGE_PATH: &str = "/usr/share/iphone-control/WDA-unsigned.ipa";

fn url() -> String {
    format!("https://github.com/appium/WebDriverAgent/releases/download/{WDA_VERSION}/WebDriverAgentRunner-Runner.zip")
}

/// `dest` 不存在就下载并转换。先写临时文件再改名，中途失败不会留下半个包
pub async fn ensure(dest: &Path) -> Result<(), String> {
    if dest.is_file() {
        return Ok(());
    }
    let dir = dest
        .parent()
        .ok_or(ictl_l10n::tr!("WDA 包路径没有上级目录", "the WDA package path has no parent directory"))?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let url = url();
    tracing::info!("{}", ictl_l10n::trf!("下载未签名的 WDA {url}", "Downloading the unsigned WDA {url}"));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| ictl_l10n::trf!("下载 WDA 失败：{e}", "Downloading WDA failed: {e}"))?;
    let bytes = client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| ictl_l10n::trf!("下载 WDA 失败：{e}", "Downloading WDA failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| ictl_l10n::trf!("下载 WDA 失败：{e}", "Downloading WDA failed: {e}"))?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != WDA_SHA256 {
        return Err(ictl_l10n::trf!(
            "下载的 WDA 校验和不对（{got}，应为 {WDA_SHA256}）",
            "The downloaded WDA has the wrong checksum ({got}, expected {WDA_SHA256})"
        ));
    }
    let zip = dir.join(".WDA-download.zip.part");
    let ipa = dir.join(".WDA-unsigned.ipa.part");
    let r = std::fs::write(&zip, &bytes)
        .and_then(|()| ictl_sign::ipa::from_app_zip(&zip, &ipa))
        .and_then(|()| std::fs::rename(&ipa, dest));
    let _ = std::fs::remove_file(&zip);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&ipa);
        return Err(ictl_l10n::trf!("转换 WDA 包失败：{e}", "Converting the WDA package failed: {e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_dockerfile() {
        let dockerfile = include_str!("../../../docker/Dockerfile");
        assert!(dockerfile.contains(&format!("ARG WDA_VERSION={WDA_VERSION}\n")), "WDA_VERSION 和 Dockerfile 不一致");
        assert!(dockerfile.contains(&format!("ARG WDA_SHA256={WDA_SHA256}\n")), "WDA_SHA256 和 Dockerfile 不一致");
        assert!(dockerfile.contains(&format!("COPY --from=wda /out/WDA-unsigned.ipa {IMAGE_PATH}\n")));
    }
}
