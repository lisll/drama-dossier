//! 硬字幕擦除管线编排。
//!
//! 分两遍处理同一个输入（通过 `reopen` 重新打开源）：
//! 1. **定位**：读取前 `probe_frames` 帧，逐帧跑检测，跨帧聚合出稳定的字幕 ROI；
//! 2. **修复**：按 `clip_frames` 切片，逐片段生成本片段的掩码并交给修复后端，
//!    只有掩码覆盖的像素被改写，其余画面保持原样。
//!
//! 两遍设计避免了把整片帧缓存在内存里；代价是需要能重复打开输入源。

use std::io;

use thiserror::Error;

use crate::detect::{filter_by_score, stable_roi, Reliability, TextBox, TextDetector};
use crate::frames::FrameBuffer;
use crate::inpaint::Inpainter;
use crate::mask::{feather, BinaryMask, FrameSize, MaskRect};
use crate::patch::{crop_roi, paste_roi, ClipPatch};

/// 可读取的帧源。
pub trait FrameSource {
    fn read_frame(&mut self) -> io::Result<Option<FrameBuffer>>;
}

/// 可写入的帧目标。
pub trait FrameSink {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()>;
}

/// 管线配置。默认值面向「固定位置硬字幕」的常见情况。
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// 每个片段的帧数。E2FGVI 这类时序模型需要连续多帧，建议 10–60。
    pub clip_frames: usize,
    /// 定位阶段读取并检测的帧数。
    pub probe_frames: usize,
    /// 检测置信度下限。
    pub detect_min_score: f32,
    /// 跨帧聚合可信度。
    pub reliability: Reliability,
    /// 掩码膨胀半径，覆盖抗锯齿与描边。
    pub mask_dilate: u32,
    /// 回贴时的羽化半径。
    pub feather_radius: u32,
    /// ROI 相对检测框的外扩比例，给修复模型留上下文。
    ///
    /// 需要大于 `mask_dilate` 与 `feather_radius` 对应的边距，否则掩码会贴到 ROI 边界。
    pub context_scale: f64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            clip_frames: 30,
            probe_frames: 60,
            detect_min_score: 0.5,
            reliability: Reliability::default(),
            mask_dilate: 4,
            feather_radius: 2,
            context_scale: 1.6,
        }
    }
}

impl PipelineConfig {
    fn validate(&self) -> Result<(), PipelineError> {
        if self.clip_frames == 0 {
            return Err(PipelineError::Config("片段帧数必须大于 0".into()));
        }
        if self.probe_frames == 0 {
            return Err(PipelineError::Config("探测帧数必须大于 0".into()));
        }
        if !(self.context_scale.is_finite() && self.context_scale >= 1.0) {
            return Err(PipelineError::Config(
                "ROI 外扩比例必须是不小于 1 的有限数值".into(),
            ));
        }
        Ok(())
    }
}

impl<R: io::Read> FrameSource for crate::frames::FrameReader<R> {
    fn read_frame(&mut self) -> io::Result<Option<FrameBuffer>> {
        crate::frames::FrameReader::read_frame(self)
    }
}

impl FrameSource for crate::frames::FfmpegFrameSource {
    fn read_frame(&mut self) -> io::Result<Option<FrameBuffer>> {
        crate::frames::FfmpegFrameSource::read_frame(self)
    }
}

impl FrameSink for crate::frames::FfmpegFrameSink {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        crate::frames::FfmpegFrameSink::write_frame(self, frame)
    }
}

/// 内存收集器，便于测试与调试。
impl FrameSink for Vec<Vec<u8>> {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.push(frame.to_vec());
        Ok(())
    }
}

/// 管线执行结果。
#[derive(Debug, Clone, Default)]
pub struct PipelineReport {
    pub frames_in: u64,
    pub frames_out: u64,
    /// 聚合出的稳定字幕 ROI；`None` 表示没有检测到字幕。
    pub roi: Option<MaskRect>,
    pub clips: u64,
    /// 实际执行了修复的片段数。
    pub inpainted_clips: u64,
    /// 修复片段中掩码的平均覆盖比例（0.0–1.0）。
    pub mean_mask_coverage: f64,
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("帧读写失败：{0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Detect(#[from] crate::detect::DetectError),
    #[error(transparent)]
    Inpaint(#[from] crate::inpaint::InpaintError),
    #[error("管线配置非法：{0}")]
    Config(String),
}

/// 执行完整管线。
///
/// `reopen` 会在定位与修复两遍各调用一次，用于重新打开同一个视频源。
pub fn run_pipeline<S, F>(
    mut reopen: F,
    sink: &mut dyn FrameSink,
    size: FrameSize,
    detector: &dyn TextDetector,
    inpainter: &dyn Inpainter,
    config: &PipelineConfig,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut() -> io::Result<S>,
    S: FrameSource,
{
    config.validate()?;
    let mut report = PipelineReport::default();

    // ---- 第一遍：定位字幕区域 ----
    let mut source = reopen()?;
    let mut samples: Vec<Vec<TextBox>> = Vec::new();
    for _ in 0..config.probe_frames {
        let Some(frame) = source.read_frame()? else {
            break;
        };
        let boxes = detector.detect(&frame, size)?;
        samples.push(filter_by_score(boxes, config.detect_min_score));
    }
    drop(source);

    let roi = match stable_roi(&samples, &config.reliability)
        .and_then(|rect| rect.scale(config.context_scale, size))
        .filter(|rect| !rect.is_empty())
    {
        Some(roi) => roi,
        // 没检测到字幕：原样输出
        None => {
            copy_through(reopen()?, sink, &mut report)?;
            return Ok(report);
        }
    };
    report.roi = Some(roi);
    let roi_size = FrameSize::new(roi.width, roi.height);

    // ---- 第二遍：逐片段修复 ----
    let mut source = reopen()?;
    let mut clip: Vec<FrameBuffer> = Vec::with_capacity(config.clip_frames);
    let mut coverage_sum = 0.0;
    while let Some(frame) = source.read_frame()? {
        report.frames_in += 1;
        clip.push(frame);
        if clip.len() >= config.clip_frames {
            process_clip(
                clip.as_mut_slice(),
                size,
                roi,
                roi_size,
                detector,
                inpainter,
                config,
                sink,
                &mut report,
                &mut coverage_sum,
            )?;
            clip.clear();
        }
    }
    if !clip.is_empty() {
        process_clip(
            clip.as_mut_slice(),
            size,
            roi,
            roi_size,
            detector,
            inpainter,
            config,
            sink,
            &mut report,
            &mut coverage_sum,
        )?;
    }

    if report.inpainted_clips > 0 {
        report.mean_mask_coverage = coverage_sum / report.inpainted_clips as f64;
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn process_clip(
    clip: &mut [FrameBuffer],
    frame_size: FrameSize,
    roi: MaskRect,
    roi_size: FrameSize,
    detector: &dyn TextDetector,
    inpainter: &dyn Inpainter,
    config: &PipelineConfig,
    sink: &mut dyn FrameSink,
    report: &mut PipelineReport,
    coverage_sum: &mut f64,
) -> Result<(), PipelineError> {
    report.clips += 1;

    let mut mask = BinaryMask::empty(roi_size);
    for frame in clip.iter() {
        for box_ in filter_by_score(detector.detect(frame, frame_size)?, config.detect_min_score) {
            mask.paint_rect(roi, box_.rect());
        }
    }
    if !mask.is_empty() {
        mask.dilate(config.mask_dilate);
    }

    if mask.is_empty() {
        // 本片段没有字幕，直接输出
        for frame in clip.iter() {
            sink.write_frame(frame)?;
            report.frames_out += 1;
        }
        return Ok(());
    }

    let mut patch = ClipPatch::new(roi_size);
    for frame in clip.iter() {
        patch.push(crop_roi(frame, frame_size, roi));
    }
    inpainter.inpaint(&mut patch, &mask, config.feather_radius)?;

    let alpha = feather(&mask, config.feather_radius);
    for (index, frame) in clip.iter_mut().enumerate() {
        if let Some(inpainted) = patch.frames.get(index) {
            paste_roi(frame, frame_size, roi, inpainted, Some(&alpha));
        }
        sink.write_frame(frame)?;
        report.frames_out += 1;
    }

    report.inpainted_clips += 1;
    *coverage_sum += mask.coverage();
    Ok(())
}

fn copy_through<S: FrameSource>(
    mut source: S,
    sink: &mut dyn FrameSink,
    report: &mut PipelineReport,
) -> Result<(), PipelineError> {
    while let Some(frame) = source.read_frame()? {
        report.frames_in += 1;
        sink.write_frame(&frame)?;
        report.frames_out += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::FixedRoiDetector;
    use crate::frames::{frame_bytes, FrameReader};
    use crate::inpaint::{ColumnFillInpainter, InpaintError, Inpainter};
    use std::io::Cursor;

    /// 合成一段测试视频：背景为 40，底部有一条白色「字幕」条。
    fn synthetic_video(size: FrameSize, frames: usize, bar: MaskRect) -> Vec<u8> {
        let stride = (size.width as usize) * 3;
        let mut data = Vec::with_capacity(frame_bytes(size) * frames);
        for _ in 0..frames {
            let mut frame = vec![40_u8; stride * size.height as usize];
            for y in bar.y..bar.bottom() {
                let start = (y as usize) * stride + (bar.x as usize) * 3;
                let end = start + (bar.width as usize) * 3;
                frame[start..end].fill(250);
            }
            data.extend_from_slice(&frame);
        }
        data
    }

    fn run(
        data: &[u8],
        size: FrameSize,
        detector: &dyn TextDetector,
        config: &PipelineConfig,
    ) -> (PipelineReport, Vec<Vec<u8>>) {
        run_with(
            data,
            size,
            detector,
            &ColumnFillInpainter { sample_rows: 4 },
            config,
        )
    }

    fn run_with(
        data: &[u8],
        size: FrameSize,
        detector: &dyn TextDetector,
        inpainter: &dyn Inpainter,
        config: &PipelineConfig,
    ) -> (PipelineReport, Vec<Vec<u8>>) {
        let mut sink: Vec<Vec<u8>> = Vec::new();
        let report = run_pipeline(
            || Ok(FrameReader::new(Cursor::new(data.to_vec()), size)),
            &mut sink,
            size,
            detector,
            inpainter,
            config,
        )
        .unwrap();
        (report, sink)
    }

    /// 模拟真实时序模型：它重建整个 ROI，ROI 内所有像素都会变。
    ///
    /// 掩码之外的画面能否保住，完全取决于回贴阶段的 alpha，
    /// 因此这个桩用来验证「只改掩码像素」这条不变量。
    struct FullRoiRewriteInpainter;

    impl Inpainter for FullRoiRewriteInpainter {
        fn inpaint(
            &self,
            patch: &mut ClipPatch,
            _mask: &BinaryMask,
            _feather_radius: u32,
        ) -> Result<(), InpaintError> {
            for (index, frame) in patch.frames.iter_mut().enumerate() {
                for (i, value) in frame.iter_mut().enumerate() {
                    *value = ((index * 31 + i * 7) % 251) as u8;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn removes_fixed_subtitle_bar_and_keeps_other_pixels() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_video(size, 10, bar);
        let detector = FixedRoiDetector::new(bar);

        let config = PipelineConfig {
            clip_frames: 4,
            probe_frames: 3,
            context_scale: 2.0,
            mask_dilate: 1,
            feather_radius: 1,
            ..Default::default()
        };
        let (report, frames) = run(&data, size, &detector, &config);

        assert_eq!(report.frames_in, 10);
        assert_eq!(report.frames_out, 10);
        assert_eq!(report.clips, 3, "10 帧按 4 帧切片应为 3 个片段");
        assert_eq!(report.inpainted_clips, 3);
        assert!(report.mean_mask_coverage > 0.0);

        let roi = report.roi.unwrap();
        let stride = (size.width as usize) * 3;
        for frame in &frames {
            // 字幕条中心应被替换为背景色
            let y = (bar.y + bar.height / 2) as usize;
            let x = (bar.x + bar.width / 2) as usize;
            assert_eq!(frame[y * stride + x * 3], 40, "字幕像素应被背景色替换");
            // 远离 ROI 的像素必须逐字节保持原样
            for value in frame.iter().take(3) {
                assert_eq!(*value, 40, "ROI 之外不应被改写");
            }
            // ROI 之外的整块区域也不应变化
            let outside_row = (roi.y.saturating_sub(1)) as usize;
            for x in 0..size.width as usize {
                assert_eq!(frame[outside_row * stride + x * 3], 40);
            }
        }
    }

    /// 掩码之外的像素必须逐字节保持原样。
    ///
    /// 背景用逐像素渐变（而不是纯色），这样任何「误改掩码外像素」的行为
    /// 都会被字节级比较抓到；羽化过渡带允许存在，因此把断言限定在
    /// 「距掩码边界超过羽化半径」的区域。
    #[test]
    fn only_mask_region_changes_on_textured_background() {
        let size = FrameSize::new(96, 64);
        let bar = MaskRect::new(16, 48, 64, 8);
        let stride = size.width as usize * 3;
        let frame_len = frame_bytes(size);

        // 每帧背景 = 位置相关的伪随机纹理，帧间也略作变化
        let frames = 6;
        let mut data = Vec::with_capacity(frame_len * frames);
        let mut original: Vec<Vec<u8>> = Vec::with_capacity(frames);
        for f in 0..frames {
            let mut frame = vec![0_u8; frame_len];
            for y in 0..size.height as usize {
                for x in 0..size.width as usize {
                    let i = y * stride + x * 3;
                    frame[i] = ((x * 7 + y * 13 + f * 29) % 251) as u8;
                    frame[i + 1] = ((x * 5 + y * 3 + f * 11) % 241) as u8;
                    frame[i + 2] = ((x * 11 + y * 17 + f * 7) % 233) as u8;
                }
            }
            for y in bar.y..bar.bottom() {
                let start = y as usize * stride + bar.x as usize * 3;
                let end = start + bar.width as usize * 3;
                frame[start..end].fill(255);
            }
            original.push(frame.clone());
            data.extend_from_slice(&frame);
        }

        let config = PipelineConfig {
            clip_frames: 2,
            probe_frames: frames,
            context_scale: 2.0,
            mask_dilate: 2,
            feather_radius: 2,
            ..Default::default()
        };
        let detector = FixedRoiDetector::new(bar);
        let (report, out) = run_with(&data, size, &detector, &FullRoiRewriteInpainter, &config);
        assert_eq!(report.frames_out, frames as u64);
        assert!(report.roi.is_some(), "测试前提：应当检测到字幕区域");
        assert_eq!(report.inpainted_clips, 3, "测试前提：应当走到修复路径");
        // 桩会改写整个 ROI；掩码内像素确实被改过，说明修复路径真的执行了
        let cx = (bar.x + bar.width / 2) as usize;
        let cy = (bar.y + bar.height / 2) as usize;
        let center = cy * stride + cx * 3;
        assert_ne!(
            &out[0][center..center + 3],
            &original[0][center..center + 3],
            "测试前提：桩应当改动了 ROI 内像素"
        );

        // 掩码 = 检测框向外膨胀 mask_dilate，再向外留 feather_radius 过渡带
        let pad = config.mask_dilate + config.feather_radius + 1;
        let mx0 = bar.x.saturating_sub(pad);
        let my0 = bar.y.saturating_sub(pad);
        let mx1 = (bar.x + bar.width + pad).min(size.width);
        let my1 = (bar.y + bar.height + pad).min(size.height);

        for (f, frame) in out.iter().enumerate() {
            for y in 0..size.height as usize {
                for x in 0..size.width as usize {
                    let inside = (x as u32) >= mx0
                        && (x as u32) < mx1
                        && (y as u32) >= my0
                        && (y as u32) < my1;
                    if inside {
                        continue;
                    }
                    let i = y * stride + x * 3;
                    assert_eq!(
                        &frame[i..i + 3],
                        &original[f][i..i + 3],
                        "第 {f} 帧 ({x},{y}) 在掩码之外被改写"
                    );
                }
            }
        }
    }

    #[test]
    fn passes_through_when_no_subtitle_is_detected() {
        let size = FrameSize::new(32, 24);
        let data = synthetic_video(size, 6, MaskRect::new(0, 0, 0, 0));
        // 检测区域完全在画面外 → 聚合结果为空
        let detector = FixedRoiDetector::new(MaskRect::new(1000, 1000, 10, 10));

        let (report, frames) = run(&data, size, &detector, &PipelineConfig::default());
        assert!(report.roi.is_none());
        assert_eq!(report.frames_in, 6);
        assert_eq!(report.frames_out, 6);
        assert_eq!(report.inpainted_clips, 0);
        assert_eq!(frames.len(), 6);
        assert!(
            frames
                .iter()
                .all(|frame| frame.iter().all(|value| *value == 40)),
            "没有字幕时应原样输出"
        );
    }

    #[test]
    fn rejects_invalid_config() {
        let size = FrameSize::new(8, 8);
        let data = synthetic_video(size, 1, MaskRect::new(0, 0, 1, 1));
        let mut sink: Vec<Vec<u8>> = Vec::new();
        let error = run_pipeline(
            || Ok(FrameReader::new(Cursor::new(data.clone()), size)),
            &mut sink,
            size,
            &FixedRoiDetector::new(MaskRect::new(0, 0, 1, 1)),
            &ColumnFillInpainter::default(),
            &PipelineConfig {
                clip_frames: 0,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, PipelineError::Config(_)));

        let error = run_pipeline(
            || Ok(FrameReader::new(Cursor::new(data.clone()), size)),
            &mut sink,
            size,
            &FixedRoiDetector::new(MaskRect::new(0, 0, 1, 1)),
            &ColumnFillInpainter::default(),
            &PipelineConfig {
                context_scale: 0.5,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, PipelineError::Config(_)));
    }

    #[test]
    fn keeps_frame_count_when_video_is_shorter_than_clip() {
        let size = FrameSize::new(16, 16);
        let bar = MaskRect::new(2, 12, 12, 3);
        let data = synthetic_video(size, 2, bar);
        let config = PipelineConfig {
            clip_frames: 30,
            probe_frames: 30,
            context_scale: 2.0,
            ..Default::default()
        };
        let (report, frames) = run(&data, size, &FixedRoiDetector::new(bar), &config);
        assert_eq!(report.frames_in, 2);
        assert_eq!(report.frames_out, 2);
        assert_eq!(frames.len(), 2);
        assert_eq!(report.clips, 1);
    }

    #[test]
    fn detector_error_propagates() {
        struct Failing;
        impl TextDetector for Failing {
            fn detect(
                &self,
                _: &[u8],
                _: FrameSize,
            ) -> Result<Vec<TextBox>, crate::detect::DetectError> {
                Err(crate::detect::DetectError::Backend("模拟失败".into()))
            }
        }
        let size = FrameSize::new(8, 8);
        let data = synthetic_video(size, 1, MaskRect::new(0, 0, 1, 1));
        let mut sink: Vec<Vec<u8>> = Vec::new();
        let error = run_pipeline(
            || Ok(FrameReader::new(Cursor::new(data.clone()), size)),
            &mut sink,
            size,
            &Failing,
            &ColumnFillInpainter::default(),
            &PipelineConfig::default(),
        )
        .unwrap_err();
        assert!(matches!(error, PipelineError::Detect(_)));
    }
}
