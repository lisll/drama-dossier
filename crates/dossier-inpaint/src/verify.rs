//! 擦除结果自动校验（确认流核心）。
//!
//! 修复完成后不能只看「有没有跑通」，还要判断**标识是否真的清除干净**、
//! **画面有没有伪影/闪烁**。本模块在输出帧上做五项可离线复算的量化检查：
//!
//! 1. **残留（residual）**：对输出帧重跑同一个文本检测器，统计掩码区域内
//!    仍被判为文本的像素比例；需要内容驱动的检测器（`fixed` 检测器不算）。
//! 2. **闪烁（flicker）**：掩码内亮度帧间差 与 掩码外参考带帧间差 的比值。
//!    修复结果若逐帧抖动，比值会显著高于自然运动。
//! 3. **接缝（seam）**：掩码边界内外亮度梯度 与 参考带梯度 的比值。
//!    掩码膨胀不足或羽化不够时，边界会留下一圈可见轮廓。
//! 4. **一致性（consistency）**：掩码内亮度相对「前若干帧中位数」的 PSNR。
//!    没有干净参考帧时，时序中位数是最接近「背景真值」的代理参考，
//!    修复结果越离谱、PSNR 越低。
//! 5. **结构相似度（ssim）**：同一组「当前帧 vs 历史中位数」的全局 SSIM。
//!    PSNR 只看逐像素误差，SSIM 还看均值/方差/协方差，能抓出「亮度对了但
//!    结构被涂抹成一片」的糊化修复（PSNR 依旧达标的情况）。
//!
//! 所有指标只读取亮度（Y）通道并且只采样掩码内外的小区域，因此可以边解码
//! 边流式计算，不需要把整片帧缓存在内存里。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::detect::{filter_by_score, TextDetector};
use crate::frames::CHANNELS;
use crate::mask::{BinaryMask, FrameSize, MaskRect};

/// 比值类指标（闪烁、接缝）的上限，避免除零产生 `inf`。
const RATIO_CAP: f64 = 100.0;
/// PSNR 在 MSE 为 0 时的取值（dB）。
const PSNR_MAX: f64 = 99.0;
/// 掩码内帧间差低于该值（亮度级）时视为无可感知闪烁。
const FLICKER_FLOOR: f64 = 1.0;
/// 一致性检查至少需要的历史帧数。
const MIN_HISTORY: usize = 3;
/// SSIM 的稳定常数（K1=0.01、K2=0.03，动态范围 255）。
const SSIM_C1: f64 = 6.5025;
const SSIM_C2: f64 = 58.5225;

/// 校验阈值。`*_warn` 触发「警告」，`*_fail` 触发「失败」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyConfig {
    /// 残留检测的置信度下限。
    pub residual_score: f32,
    /// 掩码内残留文本像素占比 警告阈值。
    pub residual_ratio_warn: f64,
    /// 掩码内残留文本像素占比 失败阈值。
    pub residual_ratio_fail: f64,
    /// 闪烁指数 平均值 警告阈值。
    pub flicker_index_warn: f64,
    /// 闪烁指数 峰值 失败阈值。
    pub flicker_index_fail: f64,
    /// 接缝分数 平均值 警告阈值。
    pub seam_score_warn: f64,
    /// 接缝分数 峰值 失败阈值。
    pub seam_score_fail: f64,
    /// 一致性 PSNR 平均值 警告阈值（dB，低于则警告）。
    pub psnr_warn_db: f64,
    /// 一致性 PSNR 最小值 失败阈值（dB，低于则失败）。
    pub psnr_fail_db: f64,
    /// 掩码内结构相似度（SSIM）平均值 警告阈值（低于则警告）。
    pub ssim_warn: f64,
    /// 掩码内结构相似度（SSIM）最小值 失败阈值（低于则失败）。
    pub ssim_fail: f64,
    /// 一致性检查使用的历史窗口帧数。
    pub history_frames: usize,
    /// 接缝检查的绝对梯度下限（亮度级）：低于该值认为边界不可见，
    /// 避免在平滑背景上把数值噪声放大成「伪接缝」。
    pub seam_min_grad: f64,
    /// 失败判定中「超阈值帧占比」上限。
    ///
    /// 单帧尖峰可能来自检测/度量的偶发抖动，因此闪烁与接缝的失败判定还要求
    /// 超阈值帧占比超过该值，或均值本身超过失败阈值。
    pub fail_frame_ratio: f64,
    /// 掩码外参考带宽度（像素）。
    pub band_width: u32,
    /// 每项指标最多采样的像素数，用于约束大分辨率素材的开销。
    pub sample_budget: usize,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            residual_score: 0.5,
            residual_ratio_warn: 0.02,
            residual_ratio_fail: 0.10,
            flicker_index_warn: 1.5,
            flicker_index_fail: 3.0,
            seam_score_warn: 1.5,
            seam_score_fail: 3.0,
            psnr_warn_db: 30.0,
            psnr_fail_db: 24.0,
            ssim_warn: 0.85,
            ssim_fail: 0.55,
            seam_min_grad: 3.0,
            fail_frame_ratio: 0.10,
            history_frames: 8,
            band_width: 4,
            sample_budget: 20_000,
        }
    }
}

impl VerifyConfig {
    /// 阈值必须有限且 `warn` 不严于 `fail`，否则判定逻辑会失去意义。
    pub fn validate(&self) -> Result<(), VerifyError> {
        let ratios = [
            ("residualRatioWarn", self.residual_ratio_warn),
            ("residualRatioFail", self.residual_ratio_fail),
            ("flickerIndexWarn", self.flicker_index_warn),
            ("flickerIndexFail", self.flicker_index_fail),
            ("seamScoreWarn", self.seam_score_warn),
            ("seamScoreFail", self.seam_score_fail),
            ("psnrWarnDb", self.psnr_warn_db),
            ("psnrFailDb", self.psnr_fail_db),
            ("ssimWarn", self.ssim_warn),
            ("ssimFail", self.ssim_fail),
        ];
        for (name, value) in ratios {
            if !value.is_finite() || value < 0.0 {
                return Err(VerifyError::Config(format!("{name} 必须是非负有限数值")));
            }
        }
        if self.residual_ratio_warn > self.residual_ratio_fail {
            return Err(VerifyError::Config(
                "residualRatioWarn 不能大于 residualRatioFail".into(),
            ));
        }
        if self.flicker_index_warn > self.flicker_index_fail {
            return Err(VerifyError::Config(
                "flickerIndexWarn 不能大于 flickerIndexFail".into(),
            ));
        }
        if self.seam_score_warn > self.seam_score_fail {
            return Err(VerifyError::Config(
                "seamScoreWarn 不能大于 seamScoreFail".into(),
            ));
        }
        if self.psnr_fail_db > self.psnr_warn_db {
            return Err(VerifyError::Config("psnrFailDb 不能大于 psnrWarnDb".into()));
        }
        if self.ssim_warn > 1.0 || self.ssim_fail > 1.0 {
            return Err(VerifyError::Config(
                "ssimWarn / ssimFail 必须是不大于 1 的相似度".into(),
            ));
        }
        if self.ssim_fail > self.ssim_warn {
            return Err(VerifyError::Config("ssimFail 不能大于 ssimWarn".into()));
        }
        if !self.residual_score.is_finite() || !(0.0..=1.0).contains(&self.residual_score) {
            return Err(VerifyError::Config(
                "residualScore 必须是 0–1 之间的有限数值".into(),
            ));
        }
        if !self.seam_min_grad.is_finite() || self.seam_min_grad < 0.0 {
            return Err(VerifyError::Config("seamMinGrad 必须是非负有限数值".into()));
        }
        if !self.fail_frame_ratio.is_finite() || !(0.0..=1.0).contains(&self.fail_frame_ratio) {
            return Err(VerifyError::Config(
                "failFrameRatio 必须是 0–1 之间的有限数值".into(),
            ));
        }
        if self.history_frames < MIN_HISTORY {
            return Err(VerifyError::Config(format!(
                "historyFrames 至少为 {MIN_HISTORY}"
            )));
        }
        if self.band_width == 0 {
            return Err(VerifyError::Config("bandWidth 必须大于 0".into()));
        }
        if self.sample_budget == 0 {
            return Err(VerifyError::Config("sampleBudget 必须大于 0".into()));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("输出帧字节数不符：期望 {} 字节，实际 {actual}", crate::frames::frame_bytes(*expected))]
    FrameBytes { expected: FrameSize, actual: usize },
    #[error(
        "掩码尺寸与 ROI 不符：掩码 {}x{}，ROI {}x{}",
        mask.width, mask.height, roi.width, roi.height
    )]
    MaskSize { mask: FrameSize, roi: FrameSize },
    #[error("校验配置非法：{0}")]
    Config(String),
    #[error(transparent)]
    Detect(#[from] crate::detect::DetectError),
}

/// 校验结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// 全部指标在警告阈值内，可自动确认。
    Pass,
    /// 触发了警告阈值，建议人工抽查。
    Warn,
    /// 触发了失败阈值，禁止自动确认，应重试或转人工复核。
    Fail,
}

impl Verdict {
    /// 取更严重的结论。
    pub fn merge(self, other: Verdict) -> Verdict {
        let rank = |value: Verdict| match value {
            Verdict::Pass => 0,
            Verdict::Warn => 1,
            Verdict::Fail => 2,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }

    pub fn is_fail(self) -> bool {
        matches!(self, Verdict::Fail)
    }
}

/// 一条校验发现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyIssue {
    /// 指标代码：`residual` / `flicker` / `seam` / `consistency`。
    pub code: String,
    pub severity: Verdict,
    pub message: String,
    /// 触发该发现的帧序号（面向失败阈值；无具体帧时为 `null`）。
    pub frame: Option<u64>,
}

impl VerifyIssue {
    fn new(code: &str, severity: Verdict, message: String, frame: Option<u64>) -> Self {
        Self {
            code: code.to_string(),
            severity,
            message,
            frame,
        }
    }
}

/// 残留指标。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResidualMetric {
    /// 是否真的执行了残留检测（`fixed` 检测器无法判断内容）。
    pub checked: bool,
    pub frames_with_residual: u64,
    pub mean_ratio: f64,
    pub max_ratio: f64,
    pub worst_frame: Option<u64>,
    /// 单帧最多残留框数。
    pub max_boxes: u64,
}

/// 闪烁指标。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickerMetric {
    pub samples: u64,
    /// 闪烁指数超过失败阈值的帧数。
    pub frames_over_fail: u64,
    pub mean_index: f64,
    pub max_index: f64,
    pub mean_inside_diff: f64,
    pub mean_outside_diff: f64,
}

/// 接缝指标。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeamMetric {
    pub samples: u64,
    /// 接缝分数超过失败阈值的帧数。
    pub frames_over_fail: u64,
    pub mean_score: f64,
    pub max_score: f64,
    pub mean_boundary_grad: f64,
    pub mean_reference_grad: f64,
}

/// 一致性指标（掩码内亮度 vs 前若干帧中位数）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsistencyMetric {
    pub samples: u64,
    /// 一致性 PSNR 低于失败阈值的帧数。
    pub frames_below_fail: u64,
    pub mean_psnr_db: f64,
    pub min_psnr_db: f64,
}

/// 结构相似度指标（掩码内 当前帧 vs 历史中位数 的全局 SSIM）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SsimMetric {
    pub samples: u64,
    /// SSIM 低于失败阈值的帧数。
    pub frames_below_fail: u64,
    pub mean_ssim: f64,
    pub min_ssim: f64,
}

impl Default for SsimMetric {
    fn default() -> Self {
        Self {
            samples: 0,
            frames_below_fail: 0,
            mean_ssim: 0.0,
            // 相似度的「未出现」上界是 1.0，而不是 0.0
            min_ssim: 1.0,
        }
    }
}

/// 结构化校验报告，可直接落盘为处理日志。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    /// 是否有可校验的内容（掩码非空且确实检查过帧）。
    pub applicable: bool,
    pub frames_checked: u64,
    pub masked_pixels: u64,
    pub roi: Option<MaskRect>,
    pub residual: ResidualMetric,
    pub flicker: FlickerMetric,
    pub seam: SeamMetric,
    pub consistency: ConsistencyMetric,
    #[serde(default)]
    pub ssim: SsimMetric,
    pub verdict: Verdict,
    pub issues: Vec<VerifyIssue>,
    /// 未能执行的检查与原因，便于复核者理解报告边界。
    pub notes: Vec<String>,
}

impl VerifyReport {
    /// 未执行校验时的空报告（例如素材里没有检测到标识）。
    pub fn not_applicable(note: impl Into<String>) -> Self {
        Self {
            applicable: false,
            frames_checked: 0,
            masked_pixels: 0,
            roi: None,
            residual: ResidualMetric::default(),
            flicker: FlickerMetric::default(),
            seam: SeamMetric::default(),
            consistency: ConsistencyMetric::default(),
            ssim: SsimMetric::default(),
            verdict: Verdict::Pass,
            issues: Vec::new(),
            notes: vec![note.into()],
        }
    }
}

/// 采样区域：把掩码换算成「掩码内 / 参考带 / 边界像素对」三类索引。
#[derive(Debug, Clone, Default)]
struct Region {
    marked: Vec<bool>,
    inside: Vec<usize>,
    band: Vec<usize>,
    boundary_pairs: Vec<(usize, usize)>,
    reference_pairs: Vec<(usize, usize)>,
    masked_pixels: u64,
    width: usize,
}

impl Region {
    fn build(mask: &BinaryMask, band_width: u32, budget: usize) -> Self {
        let width = mask.width as usize;
        let height = mask.height as usize;
        let count = width * height;
        let mut marked = vec![false; count];
        for y in 0..height {
            for x in 0..width {
                marked[y * width + x] = mask.get(x as u32, y as u32);
            }
        }
        let masked_pixels = marked.iter().filter(|value| **value).count() as u64;

        // 参考带 = 掩码按 band_width 膨胀后减去掩码本身。
        let mut dilated = mask.clone();
        dilated.dilate(band_width);
        let mut inside = Vec::new();
        let mut band = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                if marked[index] {
                    inside.push(index);
                } else if dilated.get(x as u32, y as u32) {
                    band.push(index);
                }
            }
        }

        // 边界像素对：掩码内像素与其右侧/下方相邻的掩码外像素。
        let mut boundary_pairs = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                if !marked[index] {
                    continue;
                }
                if x + 1 < width && !marked[index + 1] {
                    boundary_pairs.push((index, index + 1));
                }
                if y + 1 < height && !marked[index + width] {
                    boundary_pairs.push((index, index + width));
                }
            }
        }

        // 参考像素对：参考带内像素与其右侧/下方相邻的掩码外像素。
        let mut reference_pairs = Vec::new();
        for &index in band.iter() {
            let x = index % width;
            let y = index / width;
            if x + 1 < width && !marked[index + 1] {
                reference_pairs.push((index, index + 1));
            }
            if y + 1 < height && !marked[index + width] {
                reference_pairs.push((index, index + width));
            }
        }

        let mut region = Self {
            marked,
            inside,
            band,
            boundary_pairs,
            reference_pairs,
            masked_pixels,
            width,
        };
        region.subsample(budget);
        region
    }

    fn subsample(&mut self, budget: usize) {
        subsample_indices(&mut self.inside, budget);
        subsample_indices(&mut self.band, budget);
        subsample_indices(&mut self.boundary_pairs, budget);
        subsample_indices(&mut self.reference_pairs, budget);
    }

    fn is_usable(&self) -> bool {
        self.masked_pixels > 0 && !self.inside.is_empty()
    }
}

/// 等间隔抽稀索引列表，把每项指标的开销约束在 `budget` 以内。
fn subsample_indices<T>(list: &mut Vec<T>, budget: usize) {
    let len = list.len();
    if len <= budget {
        return;
    }
    let step = len.div_ceil(budget);
    let mut kept = Vec::with_capacity(budget + 1);
    for (index, value) in std::mem::take(list).into_iter().enumerate() {
        if index % step == 0 {
            kept.push(value);
        }
    }
    *list = kept;
}

/// 流式校验器：逐帧送入输出帧，最后产出报告。
///
/// ROI 在整条管线里是固定的，掩码按片段变化，因此用 [`Verifier::set_mask`]
/// 在每个片段开始时更新；跨片段的时序指标（闪烁、一致性）保持连续。
pub struct Verifier {
    config: VerifyConfig,
    frame_size: FrameSize,
    roi: MaskRect,
    region: Region,
    masked_seen: bool,
    max_masked_pixels: u64,
    prev_luma: Option<Vec<u8>>,
    history: VecDeque<Vec<u8>>,
    frames_checked: u64,
    residual_checked: bool,
    // 残留
    residual_frames_with_hits: u64,
    residual_sum_ratio: f64,
    residual_max_ratio: f64,
    residual_worst_frame: Option<u64>,
    residual_max_boxes: u64,
    // 闪烁
    flicker_samples: u64,
    flicker_frames_over_fail: u64,
    flicker_sum_index: f64,
    flicker_max_index: f64,
    flicker_sum_inside: f64,
    flicker_sum_outside: f64,
    // 接缝
    seam_samples: u64,
    seam_frames_over_fail: u64,
    seam_sum_score: f64,
    seam_max_score: f64,
    seam_sum_boundary: f64,
    seam_sum_reference: f64,
    // 一致性
    consistency_samples: u64,
    consistency_frames_below_fail: u64,
    consistency_sum_psnr: f64,
    consistency_min_psnr: f64,
    // 结构相似度
    ssim_samples: u64,
    ssim_frames_below_fail: u64,
    ssim_sum: f64,
    ssim_min: f64,
    hit_scratch: Vec<bool>,
    notes: Vec<String>,
}

impl Verifier {
    pub fn new(
        config: VerifyConfig,
        frame_size: FrameSize,
        roi: MaskRect,
    ) -> Result<Self, VerifyError> {
        config.validate()?;
        let Some(roi) = roi.clip(frame_size) else {
            return Err(VerifyError::Config("ROI 完全在画面外".into()));
        };
        let mut verifier = Self {
            config,
            frame_size,
            roi,
            region: Region::default(),
            masked_seen: false,
            max_masked_pixels: 0,
            prev_luma: None,
            history: VecDeque::new(),
            frames_checked: 0,
            residual_checked: false,
            residual_frames_with_hits: 0,
            residual_sum_ratio: 0.0,
            residual_max_ratio: 0.0,
            residual_worst_frame: None,
            residual_max_boxes: 0,
            flicker_samples: 0,
            flicker_frames_over_fail: 0,
            flicker_sum_index: 0.0,
            flicker_max_index: 0.0,
            flicker_sum_inside: 0.0,
            flicker_sum_outside: 0.0,
            seam_samples: 0,
            seam_frames_over_fail: 0,
            seam_sum_score: 0.0,
            seam_max_score: 0.0,
            seam_sum_boundary: 0.0,
            seam_sum_reference: 0.0,
            consistency_samples: 0,
            consistency_frames_below_fail: 0,
            consistency_sum_psnr: 0.0,
            consistency_min_psnr: PSNR_MAX,
            ssim_samples: 0,
            ssim_frames_below_fail: 0,
            ssim_sum: 0.0,
            ssim_min: 1.0,
            hit_scratch: Vec::new(),
            notes: Vec::new(),
        };
        verifier.hit_scratch = vec![false; (roi.width as usize) * (roi.height as usize)];
        Ok(verifier)
    }

    pub fn roi(&self) -> MaskRect {
        self.roi
    }

    /// 更新本片段实际使用的掩码（ROI 局部坐标）。
    pub fn set_mask(&mut self, mask: &BinaryMask) -> Result<(), VerifyError> {
        if mask.size() != FrameSize::new(self.roi.width, self.roi.height) {
            return Err(VerifyError::MaskSize {
                mask: mask.size(),
                roi: FrameSize::new(self.roi.width, self.roi.height),
            });
        }
        self.region = Region::build(mask, self.config.band_width, self.config.sample_budget);
        if self.region.masked_pixels > 0 {
            self.masked_seen = true;
            self.max_masked_pixels = self.max_masked_pixels.max(self.region.masked_pixels);
        }
        self.hit_scratch = vec![false; (self.roi.width as usize) * (self.roi.height as usize)];
        Ok(())
    }

    /// 丢弃跨帧状态：片段之间没有掩码（没有修复）时调用，
    /// 避免把相隔很远的两帧当作相邻帧比较。
    pub fn reset_temporal(&mut self) {
        self.prev_luma = None;
        self.history.clear();
    }

    /// 送入一帧**修复后**的输出帧。
    pub fn observe(
        &mut self,
        frame_index: u64,
        frame: &[u8],
        detector: &dyn TextDetector,
    ) -> Result<(), VerifyError> {
        let expected = crate::frames::frame_bytes(self.frame_size);
        if frame.len() < expected {
            return Err(VerifyError::FrameBytes {
                expected: self.frame_size,
                actual: frame.len(),
            });
        }
        self.frames_checked += 1;
        if !self.region.is_usable() {
            return Ok(());
        }

        let luma = roi_luma(frame, self.frame_size, self.roi);
        self.check_residual(frame_index, frame, detector)?;
        self.check_flicker(&luma);
        self.check_seam(&luma);
        self.check_consistency(&luma);
        self.prev_luma = Some(luma);
        Ok(())
    }

    fn check_residual(
        &mut self,
        frame_index: u64,
        frame: &[u8],
        detector: &dyn TextDetector,
    ) -> Result<(), VerifyError> {
        if !detector.is_content_based() {
            self.note_once("检测器不是内容驱动的（如 fixed），无法判断残留文本，已跳过残留检查");
            return Ok(());
        }
        self.residual_checked = true;
        // 残留框统一换算成 ROI 局部坐标，便于直接和掩码对齐
        let local_rects = self.residual_rects(frame, detector)?;
        self.residual_max_boxes = self.residual_max_boxes.max(local_rects.len() as u64);
        if local_rects.is_empty() {
            return Ok(());
        }

        self.hit_scratch.iter_mut().for_each(|value| *value = false);
        let width = self.region.width;
        for rect in &local_rects {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    let index = (y as usize) * width + (x as usize);
                    if self.region.marked[index] {
                        self.hit_scratch[index] = true;
                    }
                }
            }
        }
        let hits = self.hit_scratch.iter().filter(|value| **value).count() as f64;
        let ratio = hits / self.region.masked_pixels as f64;
        if hits > 0.0 {
            self.residual_frames_with_hits += 1;
        }
        self.residual_sum_ratio += ratio;
        if ratio > self.residual_max_ratio {
            self.residual_max_ratio = ratio;
            self.residual_worst_frame = Some(frame_index);
        }
        Ok(())
    }

    /// 在输出帧上重跑检测，返回 ROI 局部坐标的残留框。
    ///
    /// 支持 ROI 局部检测的后端（DBNet worker）直接吃 ROI 裁剪图，
    /// 与修复阶段保持一致，避免校验再扫一遍整帧。
    fn residual_rects(
        &self,
        frame: &[u8],
        detector: &dyn TextDetector,
    ) -> Result<Vec<MaskRect>, VerifyError> {
        let roi_size = FrameSize::new(self.roi.width, self.roi.height);
        let covers_frame = self.roi.x == 0
            && self.roi.y == 0
            && self.roi.width == self.frame_size.width
            && self.roi.height == self.frame_size.height;

        if detector.prefer_roi_detection() && !covers_frame {
            let crop = crate::patch::crop_roi(frame, self.frame_size, self.roi);
            return Ok(filter_by_score(
                detector.detect(&crop, roi_size)?,
                self.config.residual_score,
            )
            .into_iter()
            .filter_map(|box_| box_.rect().clip(roi_size))
            .collect());
        }

        Ok(filter_by_score(
            detector.detect(frame, self.frame_size)?,
            self.config.residual_score,
        )
        .into_iter()
        .filter_map(|box_| {
            box_.rect().intersect(&self.roi).map(|overlap| {
                MaskRect::new(
                    overlap.x - self.roi.x,
                    overlap.y - self.roi.y,
                    overlap.width,
                    overlap.height,
                )
            })
        })
        .collect())
    }

    fn check_flicker(&mut self, luma: &[u8]) {
        let Some(prev) = self.prev_luma.as_ref() else {
            return;
        };
        if prev.len() != luma.len() {
            return;
        }
        let inside_diff = mean_abs_diff(luma, prev, &self.region.inside);
        let outside_diff = mean_abs_diff(luma, prev, &self.region.band);
        let index = if inside_diff <= FLICKER_FLOOR {
            0.0
        } else if outside_diff > f64::EPSILON {
            (inside_diff / outside_diff).min(RATIO_CAP)
        } else {
            RATIO_CAP
        };
        self.flicker_samples += 1;
        if index > self.config.flicker_index_fail {
            self.flicker_frames_over_fail += 1;
        }
        self.flicker_sum_index += index;
        self.flicker_max_index = self.flicker_max_index.max(index);
        self.flicker_sum_inside += inside_diff;
        self.flicker_sum_outside += outside_diff;
    }

    fn check_seam(&mut self, luma: &[u8]) {
        if self.region.boundary_pairs.is_empty() || self.region.reference_pairs.is_empty() {
            return;
        }
        let boundary = mean_pair_diff(luma, &self.region.boundary_pairs);
        let reference = mean_pair_diff(luma, &self.region.reference_pairs);
        let score = if boundary < self.config.seam_min_grad {
            // 边界梯度本身低于可见下限：平滑背景上的比值没有意义。
            0.0
        } else if reference > f64::EPSILON {
            (boundary / reference).min(RATIO_CAP)
        } else {
            RATIO_CAP
        };
        self.seam_samples += 1;
        if score > self.config.seam_score_fail {
            self.seam_frames_over_fail += 1;
        }
        self.seam_sum_score += score;
        self.seam_max_score = self.seam_max_score.max(score);
        self.seam_sum_boundary += boundary;
        self.seam_sum_reference += reference;
    }

    fn check_consistency(&mut self, luma: &[u8]) {
        if self.history.len() >= MIN_HISTORY {
            let current: Vec<u8> = self.region.inside.iter().map(|&i| luma[i]).collect();
            let mut reference: Vec<f64> = Vec::with_capacity(current.len());
            let mut mse = 0.0_f64;
            let mut window: Vec<u8> = Vec::with_capacity(self.history.len());
            for (position, value) in current.iter().enumerate() {
                window.clear();
                for frame in &self.history {
                    window.push(frame[position]);
                }
                window.sort_unstable();
                let median = window[window.len() / 2] as f64;
                let delta = f64::from(*value) - median;
                mse += delta * delta;
                reference.push(median);
            }
            mse /= current.len() as f64;
            let psnr = if mse <= f64::EPSILON {
                PSNR_MAX
            } else {
                (10.0 * (255.0_f64 * 255.0 / mse).log10()).min(PSNR_MAX)
            };
            self.consistency_samples += 1;
            if psnr < self.config.psnr_fail_db {
                self.consistency_frames_below_fail += 1;
            }
            self.consistency_sum_psnr += psnr;
            self.consistency_min_psnr = self.consistency_min_psnr.min(psnr);

            // 结构相似度：PSNR 只看逐像素误差，SSIM 还看均值/方差/协方差，
            // 因此能抓出「亮度对了但结构糊成一片」的修复。
            let ssim = global_ssim(&current, &reference);
            self.ssim_samples += 1;
            if ssim < self.config.ssim_fail {
                self.ssim_frames_below_fail += 1;
            }
            self.ssim_sum += ssim;
            self.ssim_min = self.ssim_min.min(ssim);

            self.history.push_back(current);
        } else {
            self.history
                .push_back(self.region.inside.iter().map(|&i| luma[i]).collect());
        }
        while self.history.len() > self.config.history_frames {
            self.history.pop_front();
        }
    }

    fn note_once(&mut self, note: &str) {
        if !self.notes.iter().any(|existing| existing == note) {
            self.notes.push(note.to_string());
        }
    }

    /// 生成结构化报告与判定结论。
    pub fn finish(self) -> VerifyReport {
        let cfg = self.config.clone();
        let applicable = self.masked_seen && self.frames_checked > 0;
        let mut issues = Vec::new();
        let mut verdict = Verdict::Pass;
        let mut notes = self.notes.clone();

        if applicable {
            if self.residual_checked && self.residual_max_ratio > 0.0 {
                if self.residual_max_ratio > cfg.residual_ratio_fail {
                    verdict = verdict.merge(Verdict::Fail);
                    issues.push(VerifyIssue::new(
                        "residual",
                        Verdict::Fail,
                        format!(
                            "掩码内仍有 {:.1}% 像素被判为文本，超过失败阈值 {:.1}%",
                            self.residual_max_ratio * 100.0,
                            cfg.residual_ratio_fail * 100.0
                        ),
                        self.residual_worst_frame,
                    ));
                } else if self.residual_max_ratio > cfg.residual_ratio_warn {
                    verdict = verdict.merge(Verdict::Warn);
                    issues.push(VerifyIssue::new(
                        "residual",
                        Verdict::Warn,
                        format!(
                            "掩码内残留文本像素 {:.1}%，超过警告阈值 {:.1}%",
                            self.residual_max_ratio * 100.0,
                            cfg.residual_ratio_warn * 100.0
                        ),
                        self.residual_worst_frame,
                    ));
                }
            }

            if self.flicker_samples > 0 {
                let over_ratio = self.flicker_frames_over_fail as f64 / self.flicker_samples as f64;
                if over_ratio > cfg.fail_frame_ratio || self.flicker_mean() > cfg.flicker_index_fail
                {
                    verdict = verdict.merge(Verdict::Fail);
                    issues.push(VerifyIssue::new(
                        "flicker",
                        Verdict::Fail,
                        format!(
                            "闪烁指数均值 {:.2}（超阈值帧 {:.0}%，峰值 {:.2}，失败阈值 {:.2}）",
                            self.flicker_mean(),
                            over_ratio * 100.0,
                            self.flicker_max_index,
                            cfg.flicker_index_fail
                        ),
                        None,
                    ));
                } else if self.flicker_mean() > cfg.flicker_index_warn {
                    verdict = verdict.merge(Verdict::Warn);
                    issues.push(VerifyIssue::new(
                        "flicker",
                        Verdict::Warn,
                        format!(
                            "闪烁指数均值 {:.2}，超过警告阈值 {:.2}",
                            self.flicker_mean(),
                            cfg.flicker_index_warn
                        ),
                        None,
                    ));
                }
            } else {
                notes.push("闪烁检查样本不足（至少需要 2 帧掩码内数据）".into());
            }

            if self.seam_samples > 0 {
                let over_ratio = self.seam_frames_over_fail as f64 / self.seam_samples as f64;
                if over_ratio > cfg.fail_frame_ratio || self.seam_mean() > cfg.seam_score_fail {
                    verdict = verdict.merge(Verdict::Fail);
                    issues.push(VerifyIssue::new(
                        "seam",
                        Verdict::Fail,
                        format!(
                            "接缝分数均值 {:.2}（超阈值帧 {:.0}%，峰值 {:.2}，失败阈值 {:.2}）",
                            self.seam_mean(),
                            over_ratio * 100.0,
                            self.seam_max_score,
                            cfg.seam_score_fail
                        ),
                        None,
                    ));
                } else if self.seam_mean() > cfg.seam_score_warn {
                    verdict = verdict.merge(Verdict::Warn);
                    issues.push(VerifyIssue::new(
                        "seam",
                        Verdict::Warn,
                        format!(
                            "接缝分数均值 {:.2}，超过警告阈值 {:.2}",
                            self.seam_mean(),
                            cfg.seam_score_warn
                        ),
                        None,
                    ));
                }
            } else {
                notes.push("接缝检查样本不足（掩码边界或参考带为空）".into());
            }

            if self.consistency_samples > 0 {
                let below_ratio =
                    self.consistency_frames_below_fail as f64 / self.consistency_samples as f64;
                if below_ratio > cfg.fail_frame_ratio || self.consistency_mean() < cfg.psnr_fail_db
                {
                    verdict = verdict.merge(Verdict::Fail);
                    issues.push(VerifyIssue::new(
                        "consistency",
                        Verdict::Fail,
                        format!(
                            "掩码内一致性 PSNR 均值 {:.1} dB（低于阈值帧 {:.0}%，最低 {:.1} dB，失败阈值 {:.1} dB）",
                            self.consistency_mean(),
                            below_ratio * 100.0,
                            self.consistency_min_psnr,
                            cfg.psnr_fail_db
                        ),
                        None,
                    ));
                } else if self.consistency_mean() < cfg.psnr_warn_db {
                    verdict = verdict.merge(Verdict::Warn);
                    issues.push(VerifyIssue::new(
                        "consistency",
                        Verdict::Warn,
                        format!(
                            "掩码内一致性 PSNR 均值 {:.1} dB，低于警告阈值 {:.1} dB",
                            self.consistency_mean(),
                            cfg.psnr_warn_db
                        ),
                        None,
                    ));
                }
            } else {
                notes.push(format!(
                    "一致性检查样本不足（需要 {MIN_HISTORY} 帧以上历史，实际 {} 帧）",
                    self.frames_checked
                ));
            }

            if self.ssim_samples > 0 {
                let below_ratio = self.ssim_frames_below_fail as f64 / self.ssim_samples as f64;
                if below_ratio > cfg.fail_frame_ratio || self.ssim_mean() < cfg.ssim_fail {
                    verdict = verdict.merge(Verdict::Fail);
                    issues.push(VerifyIssue::new(
                        "ssim",
                        Verdict::Fail,
                        format!(
                            "掩码内结构相似度均值 {:.3}（低于阈值帧 {:.0}%，最低 {:.3}，失败阈值 {:.2}）",
                            self.ssim_mean(),
                            below_ratio * 100.0,
                            self.ssim_min,
                            cfg.ssim_fail
                        ),
                        None,
                    ));
                } else if self.ssim_mean() < cfg.ssim_warn {
                    verdict = verdict.merge(Verdict::Warn);
                    issues.push(VerifyIssue::new(
                        "ssim",
                        Verdict::Warn,
                        format!(
                            "掩码内结构相似度均值 {:.3}，低于警告阈值 {:.2}",
                            self.ssim_mean(),
                            cfg.ssim_warn
                        ),
                        None,
                    ));
                }
            }
        } else {
            notes.push("没有需要校验的掩码区域（流水线未执行修复）".into());
        }

        self.into_report(applicable, verdict, issues, notes)
    }

    fn flicker_mean(&self) -> f64 {
        if self.flicker_samples == 0 {
            0.0
        } else {
            self.flicker_sum_index / self.flicker_samples as f64
        }
    }

    fn seam_mean(&self) -> f64 {
        if self.seam_samples == 0 {
            0.0
        } else {
            self.seam_sum_score / self.seam_samples as f64
        }
    }

    fn ssim_mean(&self) -> f64 {
        if self.ssim_samples == 0 {
            0.0
        } else {
            self.ssim_sum / self.ssim_samples as f64
        }
    }

    fn consistency_mean(&self) -> f64 {
        if self.consistency_samples == 0 {
            0.0
        } else {
            self.consistency_sum_psnr / self.consistency_samples as f64
        }
    }

    fn into_report(
        self,
        applicable: bool,
        verdict: Verdict,
        issues: Vec<VerifyIssue>,
        notes: Vec<String>,
    ) -> VerifyReport {
        let frames = self.frames_checked.max(1) as f64;
        VerifyReport {
            applicable,
            frames_checked: self.frames_checked,
            masked_pixels: self.max_masked_pixels,
            roi: Some(self.roi),
            residual: ResidualMetric {
                checked: self.residual_checked,
                frames_with_residual: self.residual_frames_with_hits,
                mean_ratio: self.residual_sum_ratio / frames,
                max_ratio: self.residual_max_ratio,
                worst_frame: self.residual_worst_frame,
                max_boxes: self.residual_max_boxes,
            },
            flicker: FlickerMetric {
                samples: self.flicker_samples,
                frames_over_fail: self.flicker_frames_over_fail,
                mean_index: self.flicker_mean(),
                max_index: self.flicker_max_index,
                mean_inside_diff: if self.flicker_samples == 0 {
                    0.0
                } else {
                    self.flicker_sum_inside / self.flicker_samples as f64
                },
                mean_outside_diff: if self.flicker_samples == 0 {
                    0.0
                } else {
                    self.flicker_sum_outside / self.flicker_samples as f64
                },
            },
            seam: SeamMetric {
                samples: self.seam_samples,
                frames_over_fail: self.seam_frames_over_fail,
                mean_score: self.seam_mean(),
                max_score: self.seam_max_score,
                mean_boundary_grad: if self.seam_samples == 0 {
                    0.0
                } else {
                    self.seam_sum_boundary / self.seam_samples as f64
                },
                mean_reference_grad: if self.seam_samples == 0 {
                    0.0
                } else {
                    self.seam_sum_reference / self.seam_samples as f64
                },
            },
            consistency: ConsistencyMetric {
                samples: self.consistency_samples,
                frames_below_fail: self.consistency_frames_below_fail,
                mean_psnr_db: self.consistency_mean(),
                min_psnr_db: self.consistency_min_psnr,
            },
            ssim: SsimMetric {
                samples: self.ssim_samples,
                frames_below_fail: self.ssim_frames_below_fail,
                mean_ssim: self.ssim_mean(),
                min_ssim: self.ssim_min,
            },
            verdict,
            issues,
            notes,
        }
    }
}

/// 全局 SSIM：把候选像素与参考像素当成两段序列，用标准 SSIM 公式比较
/// 均值/方差/协方差（`sample_budget` 已经限制了序列长度）。
///
/// 用全局而不是滑窗，是因为掩码区域常常只有几十个像素宽，滑窗会把窗口
/// 缩到没有统计意义；全局量在这里更稳定，也更容易解释。
fn global_ssim(current: &[u8], reference: &[f64]) -> f64 {
    let count = current.len().min(reference.len());
    if count == 0 {
        return 1.0;
    }
    let count_f = count as f64;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for index in 0..count {
        sum_x += f64::from(current[index]);
        sum_y += reference[index];
    }
    let mean_x = sum_x / count_f;
    let mean_y = sum_y / count_f;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    let mut covariance = 0.0;
    for index in 0..count {
        let dx = f64::from(current[index]) - mean_x;
        let dy = reference[index] - mean_y;
        var_x += dx * dx;
        var_y += dy * dy;
        covariance += dx * dy;
    }
    let divisor = (count_f - 1.0).max(1.0);
    var_x /= divisor;
    var_y /= divisor;
    covariance /= divisor;

    let numerator = (2.0 * mean_x * mean_y + SSIM_C1) * (2.0 * covariance + SSIM_C2);
    let denominator = (mean_x * mean_x + mean_y * mean_y + SSIM_C1) * (var_x + var_y + SSIM_C2);
    if denominator.abs() <= f64::EPSILON {
        return 1.0;
    }
    (numerator / denominator).clamp(-1.0, 1.0)
}

/// 抽取 ROI 局部亮度（Y）平面。
fn roi_luma(frame: &[u8], frame_size: FrameSize, roi: MaskRect) -> Vec<u8> {
    let mut out = Vec::with_capacity((roi.width as usize) * (roi.height as usize));
    for y in roi.y..roi.bottom() {
        for x in roi.x..roi.right() {
            let base = (y as usize * frame_size.width as usize + x as usize) * CHANNELS;
            let (b, g, r) = (
                frame.get(base).copied().unwrap_or(0),
                frame.get(base + 1).copied().unwrap_or(0),
                frame.get(base + 2).copied().unwrap_or(0),
            );
            out.push(((77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b)) >> 8) as u8);
        }
    }
    out
}

fn mean_abs_diff(current: &[u8], previous: &[u8], indices: &[usize]) -> f64 {
    if indices.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0_f64;
    for &index in indices {
        let delta = f64::from(current[index]) - f64::from(previous[index]);
        sum += delta.abs();
    }
    sum / indices.len() as f64
}

fn mean_pair_diff(luma: &[u8], pairs: &[(usize, usize)]) -> f64 {
    if pairs.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0_f64;
    for &(a, b) in pairs {
        sum += (f64::from(luma[a]) - f64::from(luma[b])).abs();
    }
    sum / pairs.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{DetectError, TextBox};

    /// 内容驱动检测桩：给定区域的平均亮度超过阈值时判为「有文本」。
    struct BrightDetector {
        rect: MaskRect,
        threshold: f32,
    }

    impl BrightDetector {
        fn new(rect: MaskRect, threshold: f32) -> Self {
            Self { rect, threshold }
        }
    }

    impl TextDetector for BrightDetector {
        fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            let stride = size.width as usize * CHANNELS;
            let mut sum = 0.0_f64;
            let mut count = 0_u64;
            for y in self.rect.y..self.rect.bottom() {
                for x in self.rect.x..self.rect.right() {
                    let base = y as usize * stride + x as usize * CHANNELS;
                    let luma = (77 * u32::from(frame[base + 2])
                        + 150 * u32::from(frame[base + 1])
                        + 29 * u32::from(frame[base]))
                        >> 8;
                    sum += f64::from(luma);
                    count += 1;
                }
            }
            let mean = if count == 0 { 0.0 } else { sum / count as f64 };
            Ok(if mean > f64::from(self.threshold) {
                vec![TextBox::new(
                    self.rect.x,
                    self.rect.y,
                    self.rect.width,
                    self.rect.height,
                    0.99,
                )]
            } else {
                Vec::new()
            })
        }
    }

    /// 只在 ROI 裁剪图上检测的内容驱动后端（模拟 DBNet worker）。
    struct RoiCropDetector {
        expected: FrameSize,
        threshold: f32,
        calls: std::cell::Cell<usize>,
    }

    impl RoiCropDetector {
        fn new(expected: FrameSize, threshold: f32) -> Self {
            Self {
                expected,
                threshold,
                calls: std::cell::Cell::new(0),
            }
        }
    }

    impl TextDetector for RoiCropDetector {
        fn prefer_roi_detection(&self) -> bool {
            true
        }

        fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            assert_eq!(size, self.expected, "应只收到 ROI 裁剪图");
            self.calls.set(self.calls.get() + 1);
            let mean = frame.iter().map(|value| f64::from(*value)).sum::<f64>()
                / frame.len().max(1) as f64;
            Ok(if mean > f64::from(self.threshold) {
                vec![TextBox::new(0, 0, size.width, size.height, 0.99)]
            } else {
                Vec::new()
            })
        }
    }

    /// `--detector fixed` 那样的非内容驱动后端。
    struct AlwaysDetector(MaskRect);

    impl TextDetector for AlwaysDetector {
        fn is_content_based(&self) -> bool {
            false
        }

        fn detect(&self, _frame: &[u8], _size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            Ok(vec![TextBox::new(
                self.0.x,
                self.0.y,
                self.0.width,
                self.0.height,
                1.0,
            )])
        }
    }

    /// 生成一帧：`outside` 填整幅画面，`inside` 填掩码矩形。
    fn frame(size: FrameSize, mask: MaskRect, outside: u8, inside: u8) -> Vec<u8> {
        let stride = size.width as usize * CHANNELS;
        let mut data = vec![outside; stride * size.height as usize];
        for y in mask.y..mask.bottom() {
            for x in mask.x..mask.right() {
                let base = y as usize * stride + x as usize * CHANNELS;
                data[base..base + 3].fill(inside);
            }
        }
        data
    }

    /// 生成一帧：掩码矩形内是「列交替」的高对比结构（模拟有纹理的背景），
    /// 其余位置填 `outside`。
    fn structured_frame(size: FrameSize, mask: MaskRect, outside: u8, lo: u8, hi: u8) -> Vec<u8> {
        let stride = size.width as usize * CHANNELS;
        let mut data = vec![outside; stride * size.height as usize];
        for y in mask.y..mask.bottom() {
            for x in mask.x..mask.right() {
                let value = if (x - mask.x).is_multiple_of(2) {
                    lo
                } else {
                    hi
                };
                let base = y as usize * stride + x as usize * CHANNELS;
                data[base..base + 3].fill(value);
            }
        }
        data
    }

    fn rect_mask(size: FrameSize, mask: MaskRect) -> BinaryMask {
        let mut out = BinaryMask::empty(size);
        out.paint_rect(MaskRect::new(0, 0, size.width, size.height), mask);
        out
    }

    fn run(
        size: FrameSize,
        mask: MaskRect,
        frames: &[Vec<u8>],
        detector: &dyn TextDetector,
        config: VerifyConfig,
    ) -> VerifyReport {
        let roi = MaskRect::new(0, 0, size.width, size.height);
        run_in_roi(size, roi, mask, frames, detector, config)
    }

    /// 与 [`run`] 相同，但校验 ROI 可以小于整帧（掩码用整帧坐标给出）。
    fn run_in_roi(
        size: FrameSize,
        roi: MaskRect,
        mask: MaskRect,
        frames: &[Vec<u8>],
        detector: &dyn TextDetector,
        config: VerifyConfig,
    ) -> VerifyReport {
        let mut verifier = Verifier::new(config, size, roi).expect("校验器应能创建");
        let local = MaskRect::new(mask.x - roi.x, mask.y - roi.y, mask.width, mask.height);
        verifier
            .set_mask(&rect_mask(FrameSize::new(roi.width, roi.height), local))
            .expect("掩码尺寸应匹配");
        for (index, frame) in frames.iter().enumerate() {
            verifier
                .observe(index as u64, frame, detector)
                .expect("观察帧不应失败");
        }
        verifier.finish()
    }

    #[test]
    fn config_validation_rejects_inconsistent_thresholds() {
        let config = VerifyConfig {
            residual_ratio_warn: 0.5,
            residual_ratio_fail: 0.1,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = VerifyConfig {
            psnr_warn_db: 20.0,
            psnr_fail_db: 30.0,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = VerifyConfig {
            history_frames: 1,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = VerifyConfig {
            ssim_warn: 0.3,
            ssim_fail: 0.7,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        assert!(VerifyConfig::default().validate().is_ok());
    }

    #[test]
    fn clean_output_passes_all_metrics() {
        let size = FrameSize::new(64, 32);
        let mask = MaskRect::new(16, 16, 32, 8);
        let frames: Vec<Vec<u8>> = (0..6).map(|_| frame(size, mask, 120, 120)).collect();
        let detector = BrightDetector::new(mask, 200.0);

        let report = run(size, mask, &frames, &detector, VerifyConfig::default());

        assert!(report.applicable);
        assert_eq!(report.frames_checked, 6);
        assert_eq!(report.verdict, Verdict::Pass, "报告：{report:?}");
        assert!(report.issues.is_empty());
        assert!(report.residual.checked);
        assert_eq!(report.residual.frames_with_residual, 0);
        assert_eq!(report.flicker.samples, 5);
        assert!(report.consistency.samples >= 1);
        assert!(report.consistency.mean_psnr_db > 30.0);
        assert!(report.ssim.samples >= 1);
        assert!(
            report.ssim.mean_ssim > 0.99,
            "静止画面的一致性应接近 1.0：{}",
            report.ssim.mean_ssim
        );
    }

    #[test]
    fn global_ssim_is_one_for_identical_sequences() {
        let sequence: Vec<u8> = (0..64).map(|index| (index * 7 % 251) as u8).collect();
        let reference: Vec<f64> = sequence.iter().map(|value| f64::from(*value)).collect();
        assert!((global_ssim(&sequence, &reference) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn global_ssim_collapses_when_structure_is_flattened() {
        let mut structured = Vec::new();
        let mut flat = Vec::new();
        for index in 0..64 {
            let value = if index % 2 == 0 { 110_u8 } else { 130_u8 };
            structured.push(value);
            flat.push(120_u8);
        }
        let reference: Vec<f64> = structured.iter().map(|value| f64::from(*value)).collect();
        let emptied: Vec<f64> = flat.iter().map(|value| f64::from(*value)).collect();
        // 均值相同、方差被抹平：PSNR 看起来还行，但结构已经没了
        assert!(global_ssim(&flat, &reference) < 0.5);
        assert!(global_ssim(&flat, &emptied) > 0.99);
    }

    /// 抹平式修复：PSNR 只是「警告级」，但结构的丢失会被 SSIM 抓成失败。
    #[test]
    fn ssim_catches_smearing_that_psnr_accepts() {
        let size = FrameSize::new(64, 32);
        let mask = MaskRect::new(16, 16, 32, 8);
        let mut frames: Vec<Vec<u8>> = (0..5)
            .map(|_| structured_frame(size, mask, 120, 110, 130))
            .collect();
        // 最后一帧被抹平；同时把掩码外也抬高 10，避免闪烁指标先于 SSIM 命中
        frames.push(frame(size, mask, 130, 120));
        let detector = BrightDetector::new(mask, 200.0);

        let report = run(size, mask, &frames, &detector, VerifyConfig::default());

        assert!(
            report.consistency.mean_psnr_db > 24.0,
            "PSNR 均值仍达标：{:.1} dB",
            report.consistency.mean_psnr_db
        );
        assert!(
            report.ssim.mean_ssim < 0.85,
            "SSIM 应明显下降：{:.3}",
            report.ssim.mean_ssim
        );
        assert!(report.ssim.frames_below_fail >= 1);
        assert!(
            report.issues.iter().any(|issue| issue.code == "ssim"),
            "应产生 ssim 发现：{:?}",
            report.issues
        );
        assert_eq!(report.verdict, Verdict::Fail);
    }

    #[test]
    fn residual_text_fails_the_check() {
        let size = FrameSize::new(64, 32);
        let mask = MaskRect::new(16, 16, 32, 8);
        // 输出帧里字幕条仍在 → 掩码内平均亮度 250
        let frames: Vec<Vec<u8>> = (0..6).map(|_| frame(size, mask, 120, 250)).collect();
        let detector = BrightDetector::new(mask, 200.0);

        let report = run(size, mask, &frames, &detector, VerifyConfig::default());

        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report.residual.max_ratio > 0.9);
        assert_eq!(report.residual.worst_frame, Some(0));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "residual" && issue.severity == Verdict::Fail));
    }

    #[test]
    fn flicker_inside_mask_fails_the_check() {
        let size = FrameSize::new(64, 32);
        let mask = MaskRect::new(16, 16, 32, 8);
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|index| frame(size, mask, 120, if index % 2 == 0 { 0 } else { 200 }))
            .collect();
        let detector = BrightDetector::new(mask, 250.0);

        let report = run(size, mask, &frames, &detector, VerifyConfig::default());

        assert_eq!(report.flicker.samples, 5);
        assert_eq!(report.flicker.max_index, RATIO_CAP);
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report.issues.iter().any(|issue| issue.code == "flicker"));
    }

    #[test]
    fn seam_discontinuity_fails_the_check() {
        let size = FrameSize::new(64, 32);
        let mask = MaskRect::new(16, 16, 32, 8);
        // 掩码内被填成纯黑，与周围 200 的亮度形成硬边
        let frames: Vec<Vec<u8>> = (0..6).map(|_| frame(size, mask, 200, 0)).collect();
        let detector = BrightDetector::new(mask, 250.0);

        let report = run(size, mask, &frames, &detector, VerifyConfig::default());

        assert!(report.seam.mean_score > 3.0, "接缝分数 {report:?}");
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report.issues.iter().any(|issue| issue.code == "seam"));
    }

    #[test]
    fn fixed_detector_skips_residual_check() {
        let size = FrameSize::new(64, 32);
        let mask = MaskRect::new(16, 16, 32, 8);
        let frames: Vec<Vec<u8>> = (0..6).map(|_| frame(size, mask, 120, 120)).collect();

        let report = run(
            size,
            mask,
            &frames,
            &AlwaysDetector(mask),
            VerifyConfig::default(),
        );

        assert!(!report.residual.checked);
        assert_eq!(report.residual.frames_with_residual, 0);
        assert_eq!(report.verdict, Verdict::Pass);
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("无法判断残留文本")));
    }

    #[test]
    fn residual_check_uses_roi_crop_for_roi_aware_detectors() {
        let size = FrameSize::new(64, 32);
        let roi = MaskRect::new(16, 16, 32, 8);
        let mask = MaskRect::new(20, 18, 24, 4);
        let detector = RoiCropDetector::new(FrameSize::new(roi.width, roi.height), 100.0);

        // 修复后掩码内仍是亮的 → 判定为残留
        let frames: Vec<Vec<u8>> = (0..3).map(|_| frame(size, mask, 40, 250)).collect();
        let report = run_in_roi(size, roi, mask, &frames, &detector, VerifyConfig::default());
        assert!(detector.calls.get() >= 3, "每帧都应在 ROI 裁剪图上检测");
        assert!(report.residual.max_ratio > 0.9, "报告：{report:?}");

        // 修复干净 → 无残留
        let detector = RoiCropDetector::new(FrameSize::new(roi.width, roi.height), 100.0);
        let frames: Vec<Vec<u8>> = (0..3).map(|_| frame(size, mask, 40, 40)).collect();
        let report = run_in_roi(size, roi, mask, &frames, &detector, VerifyConfig::default());
        assert_eq!(report.residual.max_ratio, 0.0);
    }

    #[test]
    fn empty_mask_is_not_applicable() {
        let size = FrameSize::new(32, 16);
        let mut verifier = Verifier::new(
            VerifyConfig::default(),
            size,
            MaskRect::new(0, 0, size.width, size.height),
        )
        .unwrap();
        verifier
            .set_mask(&BinaryMask::empty(size))
            .expect("空掩码尺寸应匹配");
        let frame = frame(size, MaskRect::new(0, 0, 1, 1), 10, 10);
        verifier
            .observe(
                0,
                &frame,
                &BrightDetector::new(MaskRect::new(0, 0, 1, 1), 1.0),
            )
            .unwrap();

        let report = verifier.finish();
        assert!(!report.applicable);
        assert_eq!(report.masked_pixels, 0);
        assert_eq!(report.verdict, Verdict::Pass);
    }

    #[test]
    fn mask_size_mismatch_is_rejected() {
        let size = FrameSize::new(32, 16);
        let mut verifier =
            Verifier::new(VerifyConfig::default(), size, MaskRect::new(0, 0, 16, 8)).unwrap();
        let error = verifier
            .set_mask(&BinaryMask::empty(FrameSize::new(8, 4)))
            .unwrap_err();
        assert!(matches!(error, VerifyError::MaskSize { .. }));
    }
}
