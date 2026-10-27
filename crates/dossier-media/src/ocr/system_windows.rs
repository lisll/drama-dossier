//! Windows 自带 OCR（`Windows.Media.Ocr`）的实际调用。
//!
//! 为什么值得接：这是**零下载、零依赖**的一级。用户什么都不装，只要系统装了中文
//! OCR 语言包，就能出画面文字。降级链把它放在第三位，正是为了兜住「什么都没装」
//! 的大多数人。
//!
//! 几个容易踩的坑，这里都处理了：
//! 1. **只认中文**。`OcrEngine` 按语言创建，英文界面的机器即使装了中文语言包，
//!    「按界面语言创建」也会得到英文引擎、认不出中文。本工具处理的是中文短剧，
//!    所以只用中文引擎；没有中文语言包就判为这一级不可用，交给 tesseract。
//! 2. **图片尺寸有上限**。`OcrEngine::MaxImageDimension()` 以外的图会被拒收。抽帧
//!    默认放大 2 倍，横屏 1920 宽的视频会到 3840，所以超限时在解码阶段等比缩小，
//!    识别出的坐标再按比例换算回原图。
//! 3. `RecognizeAsync` 等都是 WinRT 异步操作，必须 `join()` 等它完成。
//!
//! 可用性判断（[`available`]）与实际识别用的是同一个接口：先前用 PowerShell 的
//! `Get-WindowsCapability` 查询，那个命令通常要管理员权限，普通用户永远判成不可用。

use std::path::Path;

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
    ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::{FileAccessMode, StorageFile};

use crate::ocr::system::fit_within;
use crate::ocr::{OcrError, OcrLine};

/// 系统里是否有可用的中文识别语言（不需要管理员权限）。
pub fn available() -> bool {
    chinese_language().is_some()
}

/// 识别一张图片。
pub fn recognize(image: &Path) -> Result<Vec<OcrLine>, OcrError> {
    let engine = create_engine()?;
    let run_failed = |command: &str, message: String| OcrError::RunFailed {
        command: command.to_string(),
        message,
    };

    let path = HSTRING::from(image.as_os_str());
    let file = StorageFile::GetFileFromPathAsync(&path)
        .and_then(|op| op.join())
        .map_err(|error| run_failed("StorageFile", format!("打开图片失败：{error}")))?;
    let stream = file
        .OpenAsync(FileAccessMode::Read)
        .and_then(|op| op.join())
        .map_err(|error| run_failed("OpenAsync", format!("读取图片流失败：{error}")))?;
    let decoder = BitmapDecoder::CreateAsync(&stream)
        .and_then(|op| op.join())
        .map_err(|error| run_failed("BitmapDecoder", format!("解码 PNG 失败：{error}")))?;

    let width = decoder
        .PixelWidth()
        .map_err(|error| run_failed("PixelWidth", error.to_string()))?;
    let height = decoder
        .PixelHeight()
        .map_err(|error| run_failed("PixelHeight", error.to_string()))?;
    let max_dimension = OcrEngine::MaxImageDimension()
        .map_err(|error| run_failed("MaxImageDimension", error.to_string()))?;
    let (scaled_width, scaled_height, scale) = fit_within(width, height, max_dimension);

    // 统一转成 BGRA8：OcrEngine 对像素格式有要求，灰度图直接送进去会被拒。
    // 需要缩小时用 Fant 插值（缩小时文字边缘最干净）。
    let transform =
        BitmapTransform::new().map_err(|error| run_failed("BitmapTransform", error.to_string()))?;
    if scale < 1.0 {
        transform
            .SetScaledWidth(scaled_width)
            .and_then(|()| transform.SetScaledHeight(scaled_height))
            .and_then(|()| transform.SetInterpolationMode(BitmapInterpolationMode::Fant))
            .map_err(|error| run_failed("BitmapTransform", error.to_string()))?;
    }
    let bitmap = decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
        .and_then(|op| op.join())
        .map_err(|error| run_failed("GetSoftwareBitmap", format!("转换位图失败：{error}")))?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|op| op.join())
        .map_err(|error| run_failed("RecognizeAsync", format!("识别失败：{error}")))?;
    let lines = result
        .Lines()
        .map_err(|error| run_failed("Lines", format!("读取识别结果失败：{error}")))?;

    let mut out = Vec::new();
    for index in 0..lines.Size().unwrap_or(0) {
        let Ok(line) = lines.GetAt(index) else {
            continue;
        };
        let Ok(text) = line.Text() else { continue };
        let text = text.to_string();
        if text.trim().is_empty() {
            continue;
        }
        // Windows OCR 不给逐行置信度，记为 -1（「未知」），下游用 `OcrLine::passes`
        // 放行；坐标按缩放比例换算回原图。
        let (left, top, width, height) = bounding_box(&line, scale);
        out.push(OcrLine {
            text,
            confidence: -1.0,
            left,
            top,
            width,
            height,
        });
    }
    Ok(out)
}

/// 取一行文字的外接矩形，换算回原图像素。取不到时全记 0，不影响文本本身。
fn bounding_box(line: &windows::Media::Ocr::OcrLine, scale: f64) -> (i64, i64, i64, i64) {
    let Ok(words) = line.Words() else {
        return (0, 0, 0, 0);
    };
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for index in 0..words.Size().unwrap_or(0) {
        let Ok(word) = words.GetAt(index) else {
            continue;
        };
        let Ok(rect) = word.BoundingRect() else {
            continue;
        };
        min_x = min_x.min(f64::from(rect.X));
        min_y = min_y.min(f64::from(rect.Y));
        max_x = max_x.max(f64::from(rect.X + rect.Width));
        max_y = max_y.max(f64::from(rect.Y + rect.Height));
    }
    if min_x > max_x || min_y > max_y {
        return (0, 0, 0, 0);
    }
    let back = |value: f64| (value / scale).round() as i64;
    (
        back(min_x),
        back(min_y),
        back(max_x - min_x),
        back(max_y - min_y),
    )
}

/// 系统已装的识别语言里挑一个中文的：简体优先，其次任意中文。
fn chinese_language() -> Option<Language> {
    let languages = OcrEngine::AvailableRecognizerLanguages().ok()?;
    let mut fallback = None;
    for index in 0..languages.Size().unwrap_or(0) {
        let Ok(language) = languages.GetAt(index) else {
            continue;
        };
        let Ok(tag) = language.LanguageTag() else {
            continue;
        };
        let tag = tag.to_string().to_ascii_lowercase();
        if tag.starts_with("zh-hans") || tag == "zh-cn" {
            return Some(language);
        }
        if tag.starts_with("zh") && fallback.is_none() {
            fallback = Some(language);
        }
    }
    fallback
}

/// 建中文识别引擎；没有中文语言包时明确报错，让上层降级。
fn create_engine() -> Result<OcrEngine, OcrError> {
    let missing = || {
        OcrError::ToolMissing(
            "Windows 系统 OCR 没有中文识别语言：请在「设置 → 时间和语言 → 语言」\
             添加中文(简体)，并勾选「光学字符识别」"
                .to_string(),
        )
    };
    let language = chinese_language().ok_or_else(missing)?;
    OcrEngine::TryCreateFromLanguage(&language).map_err(|_| missing())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_and_engine_creation_agree() {
        // 测试机可能装了也可能没装中文语言包，但两处结论必须一致，且失败要说清楚。
        match create_engine() {
            Ok(engine) => {
                assert!(available(), "能建出中文引擎时 available 必须为真");
                let tag = engine
                    .RecognizerLanguage()
                    .and_then(|language| language.LanguageTag())
                    .map(|tag| tag.to_string())
                    .unwrap_or_default();
                assert!(
                    tag.to_ascii_lowercase().starts_with("zh"),
                    "引擎应为中文：{tag}"
                );
            }
            Err(error) => {
                assert!(!available(), "建不出中文引擎时 available 必须为假");
                assert!(error.to_string().contains("中文"), "{error}");
            }
        }
    }
}
