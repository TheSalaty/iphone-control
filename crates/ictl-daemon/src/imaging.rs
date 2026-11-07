//! 截图缩放到点坐标、区域放大。与 toolkit 的 `imaging.py` 行为和说明文字一致。

use std::io::Cursor;

use ictl_device::ScreenSize;
use image::{DynamicImage, codecs::jpeg::JpegEncoder, imageops::FilterType};

pub const ZOOM_MIN_LONG_SIDE: f64 = 600.0;
pub const ZOOM_MAX_LONG_SIDE: f64 = 1200.0;

/// Python 的 `{:g}`：整数不带小数点，其余用最短表示
pub fn fmt_g(v: f64) -> String {
    format!("{v}")
}

/// Python 的 `{:.3g}`：3 位有效数字、去掉末尾的 0
pub fn fmt_3g(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let decimals = (2 - v.abs().log10().floor() as i32).max(0) as usize;
    let s = format!("{v:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s }
}

/// 截图和屏幕尺寸横竖不一致（HID 截图、取尺寸和截图之间刚好转了屏）就把宽高互换，按截图的方向来
pub fn oriented(img_w: u32, img_h: u32, screen: ScreenSize) -> (u32, u32) {
    let (w, h) = (screen.width.round() as u32, screen.height.round() as u32);
    if img_w != img_h && w != h && (img_w > img_h) != (w > h) { (h, w) } else { (w, h) }
}

/// 解码后马上转成 8 位 RGB：设备截图是 16 位 RGBA（1290×2796 解出来约 29MB），后面缩放、编码都按 RGB8 做，
/// 内存峰值和计算量都小得多；JPEG 本来也不要 alpha
fn decode(png: &[u8]) -> Result<DynamicImage, String> {
    let img = image::load_from_memory(png)
        .map_err(|e| ictl_l10n::trf!("截图解码失败：{e}", "Decoding the screenshot failed: {e}"))?;
    Ok(DynamicImage::ImageRgb8(img.into_rgb8()))
}

fn jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(img)
        .map_err(|e| ictl_l10n::trf!("JPEG 编码失败：{e}", "JPEG encoding failed: {e}"))?;
    Ok(out.into_inner())
}

/// 缩小用三角滤波（3:1 缩小足够清晰，比 Lanczos3 快几倍）；放大小区域用 CatmullRom 保住文字边缘
fn filter_for(from: (u32, u32), to: (u32, u32)) -> FilterType {
    if to.0 > from.0 || to.1 > from.1 { FilterType::CatmullRom } else { FilterType::Triangle }
}

/// 画面「指纹」：缩成 THUMB_W×THUMB_H 的灰度图。用来判断没有元素树的界面（游戏、地图）动画是否停了
pub const THUMB_W: u32 = 48;
pub const THUMB_H: u32 = 104;

pub fn thumbnail(png: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png)
        .map_err(|e| ictl_l10n::trf!("截图解码失败：{e}", "Decoding the screenshot failed: {e}"))?;
    // 最近邻足够：只比较前后两帧，不看画质；比三角滤波快一个数量级
    Ok(img.resize_exact(THUMB_W, THUMB_H, FilterType::Nearest).into_luma8().into_raw())
}

/// 两个指纹的平均逐像素差（0～255）。尺寸不同算完全不同
pub fn thumb_diff(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 255.0;
    }
    a.iter().zip(b).map(|(x, y)| f64::from(x.abs_diff(*y))).sum::<f64>() / a.len() as f64
}

/// 两帧之间的变化：整体平均差，和变化最大的一小块（8×8 像素，约 70×70 点）的平均差。
/// 加载圈这种小东西在整体平均里几乎看不出来，在小块里很明显
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    pub mean: f64,
    pub max_tile: f64,
}

const TILE: usize = 8;

/// 画面大部分是同一种颜色（白底/灰底的加载页、空白页）。加载圈和游戏、地图上的小动画单看画面变化分不开
/// （真机：小红书搜索加载圈和消消乐地图待机动画，变化最大的小块都在 4 左右），加载页几乎是空白的，游戏、地图满屏是图
pub fn mostly_blank(thumb: &[u8]) -> bool {
    if thumb.is_empty() {
        return false;
    }
    let mut hist = [0usize; 256];
    for &p in thumb {
        hist[p as usize] += 1;
    }
    let mode = (0..256).max_by_key(|&v| hist[v]).unwrap_or(0);
    let near: usize = hist[mode.saturating_sub(12)..=(mode + 12).min(255)].iter().sum();
    near as f64 >= thumb.len() as f64 * 0.7
}

pub fn thumb_motion(a: &[u8], b: &[u8]) -> Motion {
    let mean = thumb_diff(a, b);
    if a.len() != b.len() || a.len() != (THUMB_W * THUMB_H) as usize {
        return Motion { mean, max_tile: mean };
    }
    let (w, h) = (THUMB_W as usize, THUMB_H as usize);
    let mut max_tile = 0f64;
    for ty in (0..h).step_by(TILE) {
        for tx in (0..w).step_by(TILE) {
            let (mut sum, mut n) = (0u32, 0u32);
            for y in ty..(ty + TILE).min(h) {
                for x in tx..(tx + TILE).min(w) {
                    sum += u32::from(a[y * w + x].abs_diff(b[y * w + x]));
                    n += 1;
                }
            }
            max_tile = max_tile.max(f64::from(sum) / f64::from(n));
        }
    }
    Motion { mean, max_tile }
}

/// 整屏缩放到点尺寸：图像像素即点坐标。返回 (jpeg, 说明文字)
pub fn full_screen(png: &[u8], screen: ScreenSize, via: &str) -> Result<(Vec<u8>, String), String> {
    let img = decode(png)?;
    let (w, h) = oriented(img.width(), img.height(), screen);
    let out = jpeg(&img.resize_exact(w, h, filter_for((img.width(), img.height()), (w, h))), 80)?;
    Ok((
        out,
        ictl_l10n::trf!(
            "{w}x{h} 点,图像像素即点坐标(经 {via})",
            "{w}x{h} points, image pixels are point coordinates (via {via})"
        ),
    ))
}

/// 区域放大的几何计算（纯函数）：裁剪框（原图像素）、输出尺寸、放大倍数、裁剪后的点区域
#[derive(Debug, Clone, PartialEq)]
pub struct ZoomPlan {
    pub crop: (u32, u32, u32, u32),
    pub out: (u32, u32),
    pub zoom: f64,
    pub region: (f64, f64, f64, f64),
}

pub fn plan_zoom(
    img_w: u32,
    img_h: u32,
    screen: ScreenSize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<ZoomPlan, String> {
    let (w, h) = oriented(img_w, img_h, screen);
    let (w, h) = (w as f64, h as f64);
    let (x1, y1) = (x.max(0.0), y.max(0.0));
    let (x2, y2) = ((x + width).min(w), (y + height).min(h));
    if x2 - x1 < 1.0 || y2 - y1 < 1.0 {
        return Err(ictl_l10n::trf!(
            "放大区域不在屏幕内(屏幕 {}x{} 点)",
            "The zoom region is outside the screen (screen {}x{} points)",
            w,
            h
        ));
    }
    let px = img_w as f64 / w; // 每点对应的原图像素，一般是 3
    let (cx1, cy1) = ((x1 * px).round() as u32, (y1 * px).round() as u32);
    let (cx2, cy2) = ((x2 * px).round() as u32, (y2 * px).round() as u32);
    let (cw, ch) = (cx2 - cx1, cy2 - cy1);
    let long = cw.max(ch) as f64;
    let k = if long < ZOOM_MIN_LONG_SIDE { ZOOM_MIN_LONG_SIDE / long } else { (ZOOM_MAX_LONG_SIDE / long).min(1.0) };
    let out = ((cw as f64 * k).round() as u32, (ch as f64 * k).round() as u32);
    Ok(ZoomPlan { crop: (cx1, cy1, cw, ch), out, zoom: out.0 as f64 / (x2 - x1), region: (x1, y1, x2 - x1, y2 - y1) })
}

/// 从原始高清截图裁出一块并放大。返回 (jpeg, 说明文字，含坐标换算式)
pub fn region(
    png: &[u8],
    screen: ScreenSize,
    rect: (f64, f64, f64, f64),
    via: &str,
) -> Result<(Vec<u8>, String), String> {
    let img = decode(png)?;
    let p = plan_zoom(img.width(), img.height(), screen, rect.0, rect.1, rect.2, rect.3)?;
    let (cx, cy, cw, ch) = p.crop;
    let mut crop = img.crop_imm(cx, cy, cw, ch);
    if p.out != (cw, ch) {
        crop = crop.resize_exact(p.out.0, p.out.1, filter_for((cw, ch), p.out));
    }
    let (x1, y1, rw, rh) = p.region;
    let (x, y, z) = (fmt_g(x1), fmt_g(y1), fmt_3g(p.zoom));
    let caption = ictl_l10n::trf!(
        "区域 ({x},{y}) 起 {}x{} 点,放大 {z} 倍;图中像素 (u,v) 对应屏幕点 ({x}+u/{z}, {y}+v/{z})(经 {via})",
        "Region from ({x},{y}), {}x{} points, zoomed {z}x; image pixel (u,v) is screen point ({x}+u/{z}, {y}+v/{z}) (via {via})",
        fmt_g(rw),
        fmt_g(rh)
    );
    Ok((jpeg(&crop, 90)?, caption))
}

/// 读取 PNG 尺寸（只读文件头，不解码像素）
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png).into_dimensions().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };

    /// 只有一小块在变（加载圈）：整体平均差低于静止阈值，最大的小块很明显
    #[test]
    fn small_spinner_shows_up_in_tiles_only() {
        let len = (THUMB_W * THUMB_H) as usize;
        let a = vec![200u8; len];
        let mut b = a.clone();
        for (x, y) in [(24, 50), (25, 50), (24, 51), (25, 52)] {
            b[y * THUMB_W as usize + x] = 40;
        }
        let m = thumb_motion(&a, &b);
        assert!(m.mean < 3.0, "{m:?}");
        assert!(m.max_tile >= 4.0, "{m:?}");
        assert_eq!(thumb_motion(&a, &a), Motion { mean: 0.0, max_tile: 0.0 });
    }

    #[test]
    fn blank_page_vs_busy_picture() {
        let len = (THUMB_W * THUMB_H) as usize;
        let mut page = vec![245u8; len];
        page[..len / 5].fill(30);
        assert!(mostly_blank(&page));
        let busy: Vec<u8> = (0..len).map(|i| (i * 37 % 251) as u8).collect();
        assert!(!mostly_blank(&busy));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(w, h, image::Rgb([30, 60, 90]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn jpeg_size(j: &[u8]) -> (u32, u32) {
        image::load_from_memory(j).unwrap().to_rgb8().dimensions()
    }

    #[test]
    fn full_screen_is_scaled_to_points() {
        let (j, cap) = full_screen(&png(1290, 2796), SCREEN, "WDA").unwrap();
        assert_eq!(jpeg_size(&j), (430, 932));
        assert_eq!(cap, "430x932 点,图像像素即点坐标(经 WDA)");
    }

    #[test]
    fn landscape_screenshot_swaps_dimensions() {
        let (j, _) = full_screen(&png(2796, 1290), SCREEN, "HID").unwrap();
        assert_eq!(jpeg_size(&j), (932, 430));
    }

    /// iPad：屏幕尺寸还是横屏的 1024x768，截图已经是竖的，要按竖屏 768x1024 出图，不能压扁
    #[test]
    fn portrait_screenshot_with_landscape_size_is_not_squashed() {
        let land = ScreenSize { width: 1024.0, height: 768.0 };
        let (j, cap) = full_screen(&png(1536, 2048), land, "WDA").unwrap();
        assert_eq!(jpeg_size(&j), (768, 1024));
        assert_eq!(cap, "768x1024 点,图像像素即点坐标(经 WDA)");
        let (j, _) = full_screen(&png(2048, 1536), land, "WDA").unwrap();
        assert_eq!(jpeg_size(&j), (1024, 768));
    }

    #[test]
    fn wide_region_is_capped_at_1200() {
        let p = plan_zoom(1290, 2796, SCREEN, 0.0, 55.0, 430.0, 40.0).unwrap();
        assert_eq!(p.region, (0.0, 55.0, 430.0, 40.0));
        assert_eq!(p.out.0, 1200);
        assert!((p.zoom - 1200.0 / 430.0).abs() < 1e-9);
    }

    #[test]
    fn small_region_is_upscaled_to_600() {
        let p = plan_zoom(1290, 2796, SCREEN, 115.0, 108.0, 50.0, 20.0).unwrap();
        assert_eq!(p.out.0, 600);
        assert!((p.zoom - 12.0).abs() < 1e-9);
    }

    #[test]
    fn region_is_clamped_to_screen() {
        assert_eq!(plan_zoom(1290, 2796, SCREEN, -10.0, 900.0, 100.0, 100.0).unwrap().region, (0.0, 900.0, 90.0, 32.0));
    }

    #[test]
    fn region_outside_screen_errors() {
        assert_eq!(
            plan_zoom(1290, 2796, SCREEN, 500.0, 10.0, 50.0, 50.0).unwrap_err(),
            "放大区域不在屏幕内(屏幕 430x932 点)"
        );
    }

    /// 回归用例 #16：倍数保留 3 位有效数字（2 位会让右边缘差约 1.4 点）
    #[test]
    fn zoom_caption_uses_three_significant_digits() {
        let (j, cap) = region(&png(1290, 2796), SCREEN, (0.0, 55.0, 430.0, 40.0), "WDA").unwrap();
        assert_eq!(jpeg_size(&j).0, 1200);
        assert_eq!(
            cap,
            "区域 (0,55) 起 430x40 点,放大 2.79 倍;图中像素 (u,v) 对应屏幕点 (0+u/2.79, 55+v/2.79)(经 WDA)"
        );
    }

    #[test]
    fn python_like_number_formatting() {
        assert_eq!((fmt_g(215.0), fmt_g(0.3), fmt_g(10.5)), ("215".into(), "0.3".into(), "10.5".into()));
        assert_eq!(
            (fmt_3g(2.790697), fmt_3g(12.0), fmt_3g(3.0), fmt_3g(0.5)),
            ("2.79".into(), "12".into(), "3".into(), "0.5".into())
        );
    }

    #[test]
    fn thumbnails_tell_still_from_changed() {
        let a = thumbnail(&png(1290, 2796)).unwrap();
        assert_eq!(a.len(), (THUMB_W * THUMB_H) as usize);
        assert_eq!(thumb_diff(&a, &a), 0.0);
        let mut out = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(1290, 2796, image::Rgb([200, 200, 200]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        let b = thumbnail(&out.into_inner()).unwrap();
        assert!(thumb_diff(&a, &b) > 50.0);
        assert_eq!(thumb_diff(&a, &[]), 255.0);
    }

    #[test]
    fn png_size_reads_header() {
        assert_eq!(png_size(&png(12, 34)), Some((12, 34)));
    }
}
