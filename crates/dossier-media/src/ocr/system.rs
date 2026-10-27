//! 系统自带 OCR：Windows 的 `Windows.Media.Ocr` 与 macOS 的 Vision。
//!
//! 这一级的意义是「用户什么都不装也能出一点结果」，所以它是降级链里最该保住的
//! 一环。两套实现分别放在 `system_windows.rs` / `system_macos.rs`，用 `cfg`
//! 隔离——Linux 构建既看不到它们，也不需要它们的依赖。
//!
//! 本模块只做分发与「不支持时把原因说清楚」。**不支持不是静默跳过**：调用方需要
//! 知道为什么这一级没起作用，否则用户只会看到「OCR 未启用」而不知道下一步做什么。

use std::path::Path;

use dossier_runtime::CommandRunner;

use super::{OcrError, OcrLine};

/// 用系统 OCR 识别一张图。
///
/// `runner` 在 Windows/macOS 上当前用不到（两套实现都是进程内调用），保留参数
/// 是为了与其他后端保持同一签名。
pub fn recognize(image: &Path, runner: &dyn CommandRunner) -> Result<Vec<OcrLine>, OcrError> {
    if !image.is_file() {
        return Err(OcrError::InputMissing(image.to_path_buf()));
    }
    let _ = runner;
    platform::recognize(image)
}

/// 系统 OCR 现在能不能用来认中文。
///
/// - Windows：系统已装的识别语言里有没有中文（与实际识别用同一个接口，不需要管理员权限）；
/// - macOS：Vision 随系统提供，视为可用；
/// - 其他平台：不可用。
pub fn available() -> bool {
    platform::available()
}

/// 把 `width × height` 等比缩到长边不超过 `max_dimension`。
///
/// 返回 `(缩放后宽, 缩放后高, 缩放比例)`；不需要缩时比例为 1.0、尺寸原样返回。
/// `max_dimension` 为 0（读不到上限）时不缩放，交给识别接口自己报错。
pub fn fit_within(width: u32, height: u32, max_dimension: u32) -> (u32, u32, f64) {
    let longest = width.max(height);
    if max_dimension == 0 || longest <= max_dimension {
        return (width, height, 1.0);
    }
    let scale = f64::from(max_dimension) / f64::from(longest);
    let shrink = |value: u32| ((f64::from(value) * scale).floor() as u32).clamp(1, max_dimension);
    (shrink(width), shrink(height), scale)
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;

    pub fn recognize(image: &Path) -> Result<Vec<OcrLine>, OcrError> {
        crate::ocr::system_windows::recognize(image)
    }

    pub fn available() -> bool {
        crate::ocr::system_windows::available()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn recognize(image: &Path) -> Result<Vec<OcrLine>, OcrError> {
        crate::ocr::system_macos::recognize(image)
    }

    pub fn available() -> bool {
        true
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    use super::*;

    pub fn recognize(_image: &Path) -> Result<Vec<OcrLine>, OcrError> {
        Err(OcrError::ToolMissing(
            "当前系统没有自带 OCR（仅 Windows 与 macOS 提供）".to_string(),
        ))
    }

    pub fn available() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::SystemRunner;

    #[test]
    fn missing_image_is_reported_before_platform_check() {
        let runner = SystemRunner::new();
        let error = recognize(Path::new("/definitely/not/here.png"), &runner).unwrap_err();
        assert!(matches!(error, OcrError::InputMissing(_)));
    }

    #[test]
    fn failures_always_carry_a_reason() {
        let dir = crate::ocr::test_support::unique_dir("system-reason");
        let image = crate::ocr::test_support::write_png(&dir, "frame.png");
        match recognize(&image, &SystemRunner::new()) {
            // 真跑通了：纯白小图本来就没有文字。
            Ok(lines) => assert!(lines.is_empty(), "纯色小图不应识别出文字：{lines:?}"),
            Err(error) => {
                let text = error.to_string();
                if cfg!(any(target_os = "windows", target_os = "macos")) {
                    assert!(
                        text.len() > "OCR 工具不可用：".len(),
                        "失败要说明原因：{text}"
                    );
                } else {
                    assert!(text.contains("没有自带 OCR"), "{text}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_platforms_are_never_available() {
        if !cfg!(any(target_os = "windows", target_os = "macos")) {
            assert!(!available());
        }
    }

    #[test]
    fn small_images_are_left_alone() {
        assert_eq!(fit_within(1440, 280, 2600), (1440, 280, 1.0));
        assert_eq!(
            fit_within(2600, 100, 2600),
            (2600, 100, 1.0),
            "等于上限不缩"
        );
        assert_eq!(
            fit_within(3840, 560, 0),
            (3840, 560, 1.0),
            "读不到上限时不缩"
        );
    }

    /// 回归：横屏 1920 宽的视频抽帧放大 2 倍后是 3840 宽，超过 Windows OCR 的上限。
    #[test]
    fn oversized_images_shrink_proportionally() {
        let (width, height, scale) = fit_within(3840, 560, 2600);
        assert_eq!(width, 2600);
        assert!(width.max(height) <= 2600);
        assert!((scale - 2600.0 / 3840.0).abs() < 1e-9);
        // 高按同一比例缩：560 × 0.677 ≈ 379
        assert_eq!(height, 379);
        // 竖长图同样以长边为准
        let (w, h, _) = fit_within(100, 5200, 2600);
        assert_eq!((w, h), (50, 2600));
    }

    #[test]
    fn extreme_aspect_ratio_never_produces_zero_size() {
        let (width, height, _) = fit_within(10_000, 1, 2600);
        assert_eq!(width, 2600);
        assert_eq!(height, 1, "缩小后最小保留 1 像素");
    }
}
