//! macOS 自带 OCR（Vision 框架 `VNRecognizeTextRequest`）的实际调用。
//!
//! 与 Windows 侧同理：这是**零下载、零依赖**的一级，10.15+ 的系统都带 Vision，
//! 中文识别效果通常好于 tesseract。降级链把它放在第三位，兜住「什么都没装」的用户。
//!
//! 两个实现细节值得记下来：
//! 1. 图片直接用 `CIImage::initWithContentsOfURL` 读文件，不自己解码再重建——
//!    Vision 内部会处理色彩空间与缩放，少一层转换就少一处出错。
//! 2. `VNRecognizeTextRequest` 的语言要显式设成中文。不设的话系统按当前区域设置猜，
//!    英文系统上会漏掉中文。这里先查「本机支持哪些语言」，支持中文就设，不支持就
//!    用默认——**不因为语言不支持就整级放弃**，认不出中文至少还能认出数字与英文。

use std::path::Path;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_image::CIImage;
use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedTextObservation, VNRequest,
    VNRequestTextRecognitionLevel,
};

use crate::ocr::{OcrError, OcrLine};

/// 识别一张图片。
pub fn recognize(image: &Path) -> Result<Vec<OcrLine>, OcrError> {
    let path = image.to_str().ok_or_else(|| OcrError::RunFailed {
        command: "path".to_string(),
        message: format!("路径不是合法 UTF-8：{}", image.display()),
    })?;

    // SAFETY: 以下都是 objc2 生成的绑定，参数类型与 Apple 文档一致；所有对象都在
    // 同一线程内创建与使用，没有跨线程共享。
    unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let Some(ci_image) = CIImage::initWithContentsOfURL(CIImage::alloc(), &url) else {
            return Err(OcrError::RunFailed {
                command: "CIImage".to_string(),
                message: format!("读取图片失败：{path}"),
            });
        };

        let request = VNRecognizeTextRequest::new();
        // 精确模式对烧字幕这类清晰文字更准；快速模式会牺牲准确率换速度。
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        configure_languages(&request);

        let handler = VNImageRequestHandler::initWithCIImage_options(
            VNImageRequestHandler::alloc(),
            &ci_image,
            &NSDictionary::new(),
        );

        // 把 request 包成 NSArray<VNRequest>，这是 performRequests 的入参形态。
        let requests: Retained<NSArray<VNRequest>> =
            NSArray::from_retained_slice(&[Retained::cast_unchecked::<VNRequest>(request.clone())]);

        handler
            .performRequests_error(&requests)
            .map_err(|error| OcrError::RunFailed {
                command: "performRequests".to_string(),
                message: format!("Vision 识别失败：{error:?}"),
            })?;

        let results = request.results();
        let Some(results) = results else {
            // 没有结果不是错误：这帧本来就没有文字。
            return Ok(Vec::new());
        };

        // 归一化坐标换算成像素需要图像尺寸，从 CIImage 的 extent 取。
        let extent = ci_image.extent();
        let (image_width, image_height) = (extent.size.width, extent.size.height);

        let mut lines = Vec::new();
        for observation in results.iter() {
            let observation: &VNRecognizedTextObservation = &observation;
            // topCandidates 返回 Retained<NSArray<VNRecognizedText>>，空数组代表无候选。
            let candidates = observation.topCandidates(1);
            let count = candidates.len();
            if count == 0 {
                continue;
            }
            let Some(best) = candidates.firstObject() else {
                continue;
            };
            let text = best.string().to_string();
            if text.trim().is_empty() {
                continue;
            }
            // Vision 的 confidence 是 0~1，换算到与 tesseract 一致的 0~100。
            let confidence = f64::from(best.confidence()) * 100.0;
            let (left, top, width, height) =
                bounding_box_in_pixels(observation, image_width, image_height);
            lines.push(OcrLine {
                text,
                confidence,
                left,
                top,
                width,
                height,
            });
        }
        Ok(lines)
    }
}

/// 尽量把识别语言设成中文（简体优先），本机不支持时保持系统默认。
///
/// # Safety
/// 调用方必须处在 objc2 的 unsafe 上下文内。
unsafe fn configure_languages(request: &VNRecognizeTextRequest) {
    let Ok(supported) = request.supportedRecognitionLanguagesAndReturnError() else {
        return;
    };
    // 按优先级找：简体中文 → 繁体 → 英文。取第一个本机支持的。
    for want in ["zh-Hans", "zh-Hant", "en-US"] {
        let found = supported.iter().any(|item| {
            let value = item.to_string();
            value == want || value.starts_with(want)
        });
        if found {
            let languages = NSArray::from_retained_slice(&[NSString::from_str(want)]);
            request.setRecognitionLanguages(&languages);
            return;
        }
    }
}

/// 取观测框的像素坐标。
///
/// Vision 返回的是**归一化**坐标（0~1，原点在左下角）。为了让下游拿到的口径与
/// tesseract 一致（像素、原点左上），这里必须知道图像尺寸才能换算。尺寸在识别时
/// 由 `CIImage` 提供，所以由调用方传进来。
fn bounding_box_in_pixels(
    observation: &VNRecognizedTextObservation,
    image_width: f64,
    image_height: f64,
) -> (i64, i64, i64, i64) {
    // SAFETY: 只读访问一个刚从本线程的请求结果里拿到的对象。
    let rect = unsafe { observation.boundingBox() };
    let origin = rect.origin;
    let size = rect.size;
    // Vision 的原点在左下，图像坐标原点在左上，纵向要翻转。
    let left = origin.x * image_width;
    let top = (1.0 - origin.y - size.height) * image_height;
    let width = size.width * image_width;
    let height = size.height * image_height;
    (
        left.round() as i64,
        top.round() as i64,
        width.round() as i64,
        height.round() as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_image_is_reported() {
        // Vision 路径会先尝试读文件，不存在的路径必须明确报错而不是返回空。
        let result = recognize(Path::new("/definitely/not/here.png"));
        assert!(result.is_err(), "不存在的图片不能当成「没有文字」");
    }
}
