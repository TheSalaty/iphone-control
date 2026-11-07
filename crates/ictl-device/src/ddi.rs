//! 开发者镜像（iOS 17+ 的 Cryptex DDI）下载与缓存。
//!
//! 苹果的文件，不能随项目分发，运行时下载到数据目录。默认来源与 pymobiledevice3 相同：
//! 社区维护的 `doronz88/DeveloperDiskImage` 仓库；也可以指向自己从 Xcode 提取的目录（`ICTL_DDI_DIR`）。
//! 目录结构就是 `Cryptex1Assets::load` 要的 Restore 目录：BuildManifest.plist + 4 个载荷。

use std::path::{Path, PathBuf};

use crate::{DeviceError, Result};

pub const DEFAULT_BASE_URL: &str =
    "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Cryptex";

/// 缓存目录里必须有的文件。载荷名由上游仓库重写后的 BuildManifest 声明，固定不变
pub const FILES: [&str; 5] =
    ["BuildManifest.plist", "Image.dmg", "Image.dmg.trustcache", "Image.dmg.cryptex_info", "Image.dmg.root_hash"];

/// 记录缓存来自哪个下载地址：换了地址就整套重新下载，免得新旧文件混在一起
const SOURCE_STAMP: &str = ".source";

/// 缓存是否完整
pub fn cache_complete(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).is_file())
}

/// 用户自己提供的目录（`ICTL_DDI_DIR`）：不下载、不改动，只检查文件齐不齐
pub fn require_complete(dir: &Path) -> Result<PathBuf> {
    let missing: Vec<&str> = FILES.iter().copied().filter(|f| !dir.join(f).is_file()).collect();
    if missing.is_empty() {
        return Ok(dir.to_owned());
    }
    Err(DeviceError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        ictl_l10n::trf!(
            "ICTL_DDI_DIR（{}）里缺少 {}",
            "ICTL_DDI_DIR ({}) is missing {}",
            dir.display(),
            missing.join(ictl_l10n::tr!("、", ", "))
        ),
    )))
}

/// 删掉缓存（挂载反复失败时调用，下次重新下载）。只用于守护进程自己下载的缓存目录
pub fn invalidate(dir: &Path) {
    for f in FILES.iter().chain([&SOURCE_STAMP]) {
        let _ = std::fs::remove_file(dir.join(f));
    }
}

/// 确保 `dir` 里有完整的 DDI，缺的从 `base_url` 下载；返回该目录。
/// 下载先写临时文件再改名，中途失败不会留下半个文件。
pub async fn ensure_cache(dir: &Path, base_url: &str) -> Result<PathBuf> {
    let stamp = std::fs::read_to_string(dir.join(SOURCE_STAMP)).ok();
    if stamp.as_deref().is_some_and(|s| s.trim() != base_url) {
        tracing::info!(
            "{}",
            ictl_l10n::tr!(
                "开发者镜像下载地址变了，重新下载",
                "The developer disk image download URL changed, downloading again"
            )
        );
        invalidate(dir);
    }
    if cache_complete(dir) {
        if stamp.is_none() {
            // 早期版本下载的缓存没有记录来源：认作当前地址
            let _ = std::fs::write(dir.join(SOURCE_STAMP), base_url);
        }
        return Ok(dir.to_owned());
    }
    tokio::fs::create_dir_all(dir).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| DeviceError::Download(e.to_string()))?;
    for name in FILES {
        let dest = dir.join(name);
        if dest.is_file() {
            continue;
        }
        let url = format!("{}/{name}", base_url.trim_end_matches('/'));
        tracing::info!("{}", ictl_l10n::trf!("下载开发者镜像 {url}", "Downloading the developer disk image {url}"));
        let resp = client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| DeviceError::Download(format!("{name}: {e}")))?;
        let bytes = resp.bytes().await.map_err(|e| DeviceError::Download(format!("{name}: {e}")))?;
        let tmp = dir.join(format!(".{name}.part"));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &dest).await?;
    }
    tokio::fs::write(dir.join(SOURCE_STAMP), base_url).await?;
    Ok(dir.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_complete_needs_all_five_files() {
        let dir = std::env::temp_dir().join(format!("ictl-ddi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in &FILES[..4] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert!(!cache_complete(&dir));
        std::fs::write(dir.join(FILES[4]), b"x").unwrap();
        assert!(cache_complete(&dir));
        assert!(require_complete(&dir).is_ok());
        invalidate(&dir);
        assert!(!cache_complete(&dir));
        let e = require_complete(&dir).unwrap_err().to_string();
        assert!(e.contains("BuildManifest.plist") && e.contains("Image.dmg.root_hash"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
