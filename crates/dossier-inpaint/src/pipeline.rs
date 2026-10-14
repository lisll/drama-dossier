//! 硬字幕擦除管线编排。
//!
//! 分两遍处理同一个输入（通过 `reopen` 重新打开源）：
//! 1. **定位**：读取时间窗内前 `probe_frames` 帧，逐帧跑检测，跨帧聚合出
//!    稳定的标识区域（可以有多个，例如「底部字幕 + 角落角标」）；
//!    也可以用 `regions` 直接给定区域，跳过第一遍；
//! 2. **修复**：按 `clip_frames` 切片，逐**帧**生成掩码（再做时序平滑），
//!    交给修复后端；只有掩码覆盖的像素被改写，其余画面保持原样。
//!
//! 两遍设计避免了把整片帧缓存在内存里；代价是需要能重复打开输入源。
//!
//! 可选的时间窗（`start_frame` / `end_frame`）用来只处理「有标识的那一段」，
//! 窗口外帧原样输出，既省算力也避免对无标识画面做无谓重编码。

use std::io;
use std::path::PathBuf;

use dossier_core::progress::{ProgressError, ProgressEventKind, ProgressRecorder, TaskProgress};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::detect::{
    filter_by_regions, filter_by_score, offset_box, stable_rois, union_rects, DetectError,
    Reliability, TextBox, TextDetector,
};
use crate::frames::FrameBuffer;
use crate::inpaint::{union_masks, Inpainter};
use crate::mask::{feather, BinaryMask, FrameSize, MaskRect};
use crate::patch::{crop_roi, paste_roi, ClipPatch};
use crate::verify::{Verifier, VerifyConfig, VerifyReport};

/// 进度流的阶段总数（`stage_index` 从 0 开始）：0 = 定位，1 = 修复。
///
/// 为什么只有两段：逐帧校验是**跟着修复一起做**的（每个片段修完立即度量），
/// 封装与裁决在 `main.rs` 里、管线之外，所以不在这里虚报第三个阶段。
pub const PROGRESS_STAGE_TOTAL: u32 = 2;

/// 定位阶段在进度流里的序号与名字（0）。
pub const LOCATE_STAGE: u32 = 0;
/// 定位阶段的阶段名。
pub const LOCATE_STAGE_NAME: &str = "locate";
/// 修复阶段在进度流里的序号与名字（1）。
pub const INPAINT_STAGE: u32 = 1;
/// 修复阶段的阶段名。
pub const INPAINT_STAGE_NAME: &str = "inpaint";

/// 进度流落点与身份（Tier 3：长任务进度流）。
///
/// 擦除是逐帧长任务，跑到一半时使用者只能看到一个静止的 `RUNNING`。给一个文件路径后，
/// 管线会把「定位 / 修复」两个阶段与逐片段进度按 `dossier-core::progress` 的 JSONL 格式
/// 追加写进去：桌面端与云端 tail 这个文件就能显示真实进度，进程被杀也只丢最后半行。
///
/// **默认不写**（`None`）：不产生任何行为变化，也不需要额外的文件权限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressStream {
    /// JSONL 输出路径；每次运行**覆盖**（同一次运行的进度只有一份）。
    pub path: PathBuf,
    /// 任务标识（云端是 task id，桌面端可以是输出文件名）。
    pub task_id: String,
    /// 管线标识，例如 `drama.logo.erase`。
    pub pipeline: String,
    /// 修复阶段的**帧数估计**（单位 = 帧），由调用方按 `时长 × 帧率` 与处理窗口算出。
    ///
    /// 为什么是估计：擦除按时间顺序流式读帧，开始前拿不到真实帧数（`nb_frames` 大量素材
    /// 不写）。估计值只用来算百分比分母，**偏小不会让进度失真**——管线每次上报都用
    /// `max(估计, 实际已完成)` 当分母，所以真实帧数超过估计时进度条继续正常增长，
    /// 不会卡在 100% 也不会写出一条自相矛盾的进度（`0` 表示完全未知）。
    pub est_total_frames: u64,
}

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
    /// 擦除结果自动校验；`None` 表示不校验（只做修复）。
    pub verify: Option<VerifyConfig>,
    /// 显式区域列表（CLI 可多次给出 `--roi`）。
    ///
    /// 非空时跳过定位阶段，直接在这些区域内生成掩码；
    /// 为空时由检测结果跨帧聚合出稳定区域（可能不止一个）。
    pub regions: Vec<MaskRect>,
    /// 逐帧检测框必须落在区域内的判定外扩量（像素）。
    pub region_pad: u32,
    /// 掩码时序平滑半径（帧）：相邻帧的掩码互相取并集。
    ///
    /// 作用是让「某帧漏检的标识」也被修复，避免擦除结果逐帧闪烁。
    pub temporal_smooth: u32,
    /// 只处理帧序号 `>= start_frame` 的帧。
    pub start_frame: Option<u64>,
    /// 只处理帧序号 `< end_frame` 的帧。
    pub end_frame: Option<u64>,
    /// 区域分组阈值：合并后的外扩 ROI 面积最多是各组单独外扩面积之和的多少倍。
    ///
    /// 两处标识挨得近时合并几乎不浪费面积，放进同一组可以少一次模型调用；
    /// 离得远时合并会让 ROI 覆盖大片无关画面，此时应拆成多组分别裁剪。
    /// 默认 2.0：合并浪费的面积只有在「不超过各自独立处理之和的两倍」时才划算。
    /// `<= 1.0` 表示不分组（整个外接 ROI 一次送模型），等于旧行为。
    pub region_group_area_ratio: f64,
    /// 进度流（`None` = 不写）。见 [`ProgressStream`]。
    pub progress: Option<ProgressStream>,
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
            verify: None,
            regions: Vec::new(),
            region_pad: 8,
            temporal_smooth: 1,
            start_frame: None,
            end_frame: None,
            region_group_area_ratio: 2.0,
            progress: None,
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
        if let Some(verify) = &self.verify {
            verify
                .validate()
                .map_err(|error| PipelineError::Config(error.to_string()))?;
        }
        if let (Some(start), Some(end)) = (self.start_frame, self.end_frame) {
            if start >= end {
                return Err(PipelineError::Config(format!(
                    "时间窗非法：起始帧 {start} 必须小于结束帧 {end}"
                )));
            }
        }
        if !self.region_group_area_ratio.is_finite() || self.region_group_area_ratio < 0.0 {
            return Err(PipelineError::Config(
                "区域分组比例必须是非负的有限数值（<= 1 表示不分组）".into(),
            ));
        }
        if self.temporal_smooth as usize > self.clip_frames {
            return Err(PipelineError::Config(
                "掩码时序平滑半径不能大于片段帧数".into(),
            ));
        }
        Ok(())
    }

    /// 帧序号是否落在处理窗口内。
    pub fn in_window(&self, index: u64) -> bool {
        if self.start_frame.is_some_and(|start| index < start) {
            return false;
        }
        if self.end_frame.is_some_and(|end| index >= end) {
            return false;
        }
        true
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

/// 管线执行结果（可序列化为处理日志）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineReport {
    pub frames_in: u64,
    pub frames_out: u64,
    /// 聚合出的稳定字幕 ROI；`None` 表示没有检测到字幕。
    pub roi: Option<MaskRect>,
    /// 稳定标识区域列表（可能是字幕 + 角标等多个）。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub regions: Vec<MaskRect>,
    pub clips: u64,
    /// 实际执行了修复的片段数。
    pub inpainted_clips: u64,
    /// 区域分组后，实际提交给修复后端的批次数（≥ 修复片段数）。
    #[serde(default)]
    pub inpaint_batches: u64,
    /// 区域分组数量（1 表示所有标识共用一次裁剪）。
    #[serde(default)]
    pub roi_jobs: u64,
    /// 修复片段中掩码的平均覆盖比例（0.0–1.0）。
    pub mean_mask_coverage: f64,
    /// 自动校验报告；`None` 表示本次未启用校验。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub verification: Option<VerifyReport>,
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
    #[error("输出校验失败：{0}")]
    Verify(String),
    #[error("进度流写入失败：{0}")]
    Progress(#[from] ProgressError),
}

/// 写一条进度事件；没有配进度流时是空操作。
#[allow(clippy::too_many_arguments)]
fn record_progress(
    recorder: &mut Option<ProgressRecorder>,
    stream: Option<&ProgressStream>,
    kind: ProgressEventKind,
    stage_index: u32,
    stage: &str,
    unit_done: u64,
    unit_total: u64,
    message: &str,
) -> Result<(), PipelineError> {
    let (Some(recorder), Some(stream)) = (recorder.as_mut(), stream) else {
        return Ok(());
    };
    let mut item = TaskProgress::new(
        stream.task_id.clone(),
        stream.pipeline.clone(),
        stage,
        stage_index,
        PROGRESS_STAGE_TOTAL,
    )
    .with_units(unit_done, unit_total);
    if !message.is_empty() {
        item = item.with_message(message);
    }
    recorder.record(kind, &item)?;
    Ok(())
}

/// 修复阶段的分母：估计值与实际已完成帧数取大，再用 1 兜底。
///
/// 这样「估计偏小」只会让分母随真实进度一起涨，绝不会写出 `unit_done > unit_total`
/// 这类会被 `ProgressRecorder` 直接拒收的事件。
fn inpaint_total(stream: Option<&ProgressStream>, done: u64) -> u64 {
    let estimate = stream.map(|item| item.est_total_frames).unwrap_or(0);
    estimate.max(done).max(1)
}

/// 没有可修复区域时的收尾：把进度流推到底，否则文件会永远停在「运行中」。
///
/// `done` 用真实写出的帧数（`copy_through` 也是逐帧输出，只是没改像素）。
fn finish_without_inpaint(
    recorder: &mut Option<ProgressRecorder>,
    stream: Option<&ProgressStream>,
    done: u64,
    reason: &str,
) -> Result<(), PipelineError> {
    let total = inpaint_total(stream, done);
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Progress,
        INPAINT_STAGE,
        INPAINT_STAGE_NAME,
        done,
        total,
        reason,
    )?;
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Finished,
        INPAINT_STAGE,
        INPAINT_STAGE_NAME,
        done,
        total,
        reason,
    )
}

/// 每处理完一个片段上报一次。
///
/// 单位是「已输出帧数」而不是「已处理片段数」：片段长度会变（尾部残片、离开时间窗的
/// 冲刷），按帧数上报才能和定位阶段、和最终报告里的 `framesOut` 对得上。
fn clip_progress(
    recorder: &mut Option<ProgressRecorder>,
    stream: Option<&ProgressStream>,
    report: &PipelineReport,
) -> Result<(), PipelineError> {
    let total = inpaint_total(stream, report.frames_out);
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Progress,
        INPAINT_STAGE,
        INPAINT_STAGE_NAME,
        report.frames_out,
        total,
        "片段修复完成",
    )
}

/// 一个「裁剪—修复」作业：一组标识共用的 ROI 与其内部区域。
#[derive(Debug, Clone)]
struct RoiJob {
    roi: MaskRect,
    roi_size: FrameSize,
    regions: Vec<MaskRect>,
}

/// 把稳定区域分组成若干作业：每组单独裁剪、单独送模型。
///
/// 两处标识离得远时，它们的**外接** ROI 会把中间大片无关画面一起框进来：
/// 模型上下文变脏，耗时也随面积上涨。分组规则是「合并后的外扩 ROI 面积不得超过
/// 各组单独外扩面积之和的 `max_group_area_ratio` 倍」，即只有本来就挨得近
/// （合并几乎不浪费面积）的区域才会放进同一组。
///
/// `max_group_area_ratio <= 1.0` 时不分组，等价于旧行为（一次外接 ROI）。
fn plan_roi_jobs(
    regions: &[MaskRect],
    context_scale: f64,
    max_group_area_ratio: f64,
    size: FrameSize,
) -> Vec<RoiJob> {
    let single = |rects: &[MaskRect]| -> Option<RoiJob> {
        let roi = union_rects(rects)
            .and_then(|rect| rect.scale(context_scale, size))
            .filter(|rect| !rect.is_empty())?;
        Some(RoiJob {
            roi,
            roi_size: FrameSize::new(roi.width, roi.height),
            regions: rects.to_vec(),
        })
    };

    if regions.is_empty() {
        return Vec::new();
    }
    if regions.len() == 1 || max_group_area_ratio <= 1.0 {
        return single(regions).into_iter().collect();
    }

    let mut sorted = regions.to_vec();
    sorted.sort_by_key(|rect| (rect.x, rect.y));

    let scaled_area = |rect: MaskRect| -> u64 {
        rect.scale(context_scale, size)
            .map(|scaled| scaled.area())
            .unwrap_or(0)
    };

    let mut groups: Vec<Vec<MaskRect>> = Vec::new();
    for rect in sorted {
        let keep_separate = match groups.last() {
            None => true,
            Some(group) => {
                let merged_area = union_rects(&[group.as_slice(), &[rect]].concat())
                    .and_then(|union| union.scale(context_scale, size))
                    .map(|scaled| scaled.area())
                    .unwrap_or(u64::MAX);
                let separate_area =
                    group.iter().copied().map(scaled_area).sum::<u64>() + scaled_area(rect);
                merged_area as f64 > separate_area as f64 * max_group_area_ratio
            }
        };
        if keep_separate {
            groups.push(vec![rect]);
        } else if let Some(group) = groups.last_mut() {
            group.push(rect);
        }
    }

    groups.iter().filter_map(|group| single(group)).collect()
}

/// 执行完整管线。
///
/// `reopen` 会在定位与修复两遍各调用一次，用于重新打开同一个视频源。
///
/// 配了 [`PipelineConfig::progress`] 时，全过程按 `dossier-core` 的 JSONL 进度格式落盘：
/// `locate`（逐帧检测）与 `inpaint`（逐片段修复）两个阶段，每处理完一个片段追加一条，
/// 进程被杀也只丢最后半行。
pub fn run_pipeline<S, F>(
    reopen: F,
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
    let stream = config.progress.as_ref();
    // 覆盖写：一次运行对应一份进度。给了路径却建不出文件属于配置问题，显式失败。
    let mut recorder = match stream {
        Some(item) => Some(ProgressRecorder::create(&item.path)?),
        None => None,
    };

    let result = run_pipeline_inner(
        reopen,
        sink,
        size,
        detector,
        inpainter,
        config,
        &mut recorder,
        stream,
    );

    // 失败也必须留下终态，否则 tailer 会一直以为任务在跑；但这条写入是尽力而为——
    // 需要保留的错误是管线错误本身，不能被「进度文件写不进去」顶替掉。
    if result.is_err() {
        if let Some(active) = recorder.as_mut() {
            if let Some(last) = active.last_event().map(|event| event.progress.clone()) {
                let _ = active.record(ProgressEventKind::Failed, &last);
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline_inner<S, F>(
    mut reopen: F,
    sink: &mut dyn FrameSink,
    size: FrameSize,
    detector: &dyn TextDetector,
    inpainter: &dyn Inpainter,
    config: &PipelineConfig,
    recorder: &mut Option<ProgressRecorder>,
    stream: Option<&ProgressStream>,
) -> Result<PipelineReport, PipelineError>
where
    F: FnMut() -> io::Result<S>,
    S: FrameSource,
{
    let mut report = PipelineReport::default();

    // ---- 第一遍：确定标识区域（显式区域优先） ----
    let mut regions: Vec<MaskRect> = config
        .regions
        .iter()
        .filter_map(|rect| rect.clip(size))
        .filter(|rect| !rect.is_empty())
        .collect();

    // 定位阶段的单位是「采样的帧数」；显式给了区域就只剩一个单元（解析区域）。
    let auto_locate = regions.is_empty();
    let locate_units = if auto_locate {
        config.probe_frames.max(1) as u64
    } else {
        1
    };
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Started,
        LOCATE_STAGE,
        LOCATE_STAGE_NAME,
        0,
        locate_units,
        "定位标识区域",
    )?;

    if auto_locate {
        // 每 1/10 阶段上报一次：逐帧检测是最慢的一段，只在首尾上报会看起来像卡死。
        let step = (locate_units / 10).max(1);
        let mut source = reopen()?;
        let mut samples: Vec<Vec<TextBox>> = Vec::new();
        let mut index = 0_u64;
        while samples.len() < config.probe_frames {
            let Some(frame) = source.read_frame()? else {
                break;
            };
            // 只采样时间窗内的帧：窗口外的画面不参与区域判定
            if config.in_window(index) {
                let boxes = detector.detect(&frame, size)?;
                samples.push(filter_by_score(boxes, config.detect_min_score));
            }
            index += 1;
            if index.is_multiple_of(step) {
                record_progress(
                    recorder,
                    stream,
                    ProgressEventKind::Progress,
                    LOCATE_STAGE,
                    LOCATE_STAGE_NAME,
                    samples.len() as u64,
                    locate_units,
                    "逐帧检测标识",
                )?;
            }
        }
        drop(source);
        regions = stable_rois(&samples, &config.reliability)
            .into_iter()
            .filter_map(|rect| rect.clip(size))
            .filter(|rect| !rect.is_empty())
            .collect();
    }

    let locate_done_message = if auto_locate {
        format!("聚合出 {} 个标识区域", regions.len())
    } else {
        "使用显式区域，跳过自动定位".to_string()
    };
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Progress,
        LOCATE_STAGE,
        LOCATE_STAGE_NAME,
        locate_units,
        locate_units,
        &locate_done_message,
    )?;

    if regions.is_empty() {
        // 没检测到标识：原样输出
        copy_through(reopen()?, sink, &mut report)?;
        finish_without_inpaint(
            recorder,
            stream,
            report.frames_out,
            "未检测到标识，全部帧原样输出",
        )?;
        return Ok(report);
    }
    report.regions = regions.clone();

    // 裁剪 ROI = 所有区域外接框外扩 context_scale，给修复模型留上下文
    let roi = match union_rects(&regions)
        .and_then(|rect| rect.scale(config.context_scale, size))
        .filter(|rect| !rect.is_empty())
    {
        Some(roi) => roi,
        None => {
            copy_through(reopen()?, sink, &mut report)?;
            finish_without_inpaint(
                recorder,
                stream,
                report.frames_out,
                "标识区域无法形成有效 ROI，全部帧原样输出",
            )?;
            return Ok(report);
        }
    };
    report.roi = Some(roi);
    let roi_size = FrameSize::new(roi.width, roi.height);
    // 标识离得远时拆成多个「裁剪—修复」作业，避免为一个外接 ROI 付出整片
    // 无关画面的模型开销（详见 plan_roi_jobs）。
    let jobs = plan_roi_jobs(
        &regions,
        config.context_scale,
        config.region_group_area_ratio,
        size,
    );
    if jobs.is_empty() {
        copy_through(reopen()?, sink, &mut report)?;
        finish_without_inpaint(
            recorder,
            stream,
            report.frames_out,
            "未形成有效修复作业，全部帧原样输出",
        )?;
        return Ok(report);
    }
    report.roi_jobs = jobs.len() as u64;
    let mut verifier = match &config.verify {
        Some(verify_config) => Some(
            Verifier::new(verify_config.clone(), size, roi)
                .map_err(|error| PipelineError::Config(error.to_string()))?,
        ),
        None => None,
    };

    // ---- 第二遍：逐帧掩码 + 逐片段修复 ----
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Started,
        INPAINT_STAGE,
        INPAINT_STAGE_NAME,
        0,
        inpaint_total(stream, 0),
        "逐片段修复",
    )?;
    let mut source = reopen()?;
    let mut clip: Vec<FrameBuffer> = Vec::with_capacity(config.clip_frames);
    let mut coverage_sum = 0.0;
    let mut index = 0_u64;
    while let Some(frame) = source.read_frame()? {
        report.frames_in += 1;
        if !config.in_window(index) {
            // 离开时间窗：先冲刷窗口内的片段，保证输出帧序不乱
            if !clip.is_empty() {
                process_clip(
                    clip.as_mut_slice(),
                    size,
                    roi,
                    roi_size,
                    &jobs,
                    detector,
                    inpainter,
                    config,
                    sink,
                    &mut report,
                    &mut coverage_sum,
                    verifier.as_mut(),
                )?;
                clip.clear();
                clip_progress(recorder, stream, &report)?;
            }
            if let Some(verifier) = verifier.as_mut() {
                verifier.reset_temporal();
            }
            sink.write_frame(&frame)?;
            report.frames_out += 1;
            index += 1;
            continue;
        }

        clip.push(frame);
        index += 1;
        if clip.len() >= config.clip_frames {
            process_clip(
                clip.as_mut_slice(),
                size,
                roi,
                roi_size,
                &jobs,
                detector,
                inpainter,
                config,
                sink,
                &mut report,
                &mut coverage_sum,
                verifier.as_mut(),
            )?;
            clip.clear();
            clip_progress(recorder, stream, &report)?;
        }
    }
    if !clip.is_empty() {
        process_clip(
            clip.as_mut_slice(),
            size,
            roi,
            roi_size,
            &jobs,
            detector,
            inpainter,
            config,
            sink,
            &mut report,
            &mut coverage_sum,
            verifier.as_mut(),
        )?;
        clip_progress(recorder, stream, &report)?;
    }

    if report.inpainted_clips > 0 {
        report.mean_mask_coverage = coverage_sum / report.inpainted_clips as f64;
    }
    report.verification = verifier.map(|verifier| verifier.finish());
    record_progress(
        recorder,
        stream,
        ProgressEventKind::Finished,
        INPAINT_STAGE,
        INPAINT_STAGE_NAME,
        report.frames_out,
        inpaint_total(stream, report.frames_out),
        "擦除完成",
    )?;
    Ok(report)
}

/// 处理一个片段：逐作业生成掩码 → 时序平滑 → 裁剪修复 → 回贴 → 校验。
///
/// 修复按 `jobs` 分组进行：每组只把「自己那块 ROI」送进模型；掩码同时并进
/// 一张**并集 ROI** 的掩码，供校验与覆盖率统计使用（校验的语义因此与
/// 「一次外接 ROI」时完全一致）。
#[allow(clippy::too_many_arguments)]
fn process_clip(
    clip: &mut [FrameBuffer],
    frame_size: FrameSize,
    union_roi: MaskRect,
    union_size: FrameSize,
    jobs: &[RoiJob],
    detector: &dyn TextDetector,
    inpainter: &dyn Inpainter,
    config: &PipelineConfig,
    sink: &mut dyn FrameSink,
    report: &mut PipelineReport,
    coverage_sum: &mut f64,
    mut verifier: Option<&mut Verifier>,
) -> Result<(), PipelineError> {
    report.clips += 1;

    let mut merged_masks: Vec<BinaryMask> = vec![BinaryMask::empty(union_size); clip.len()];

    for job in jobs {
        let mut masks = build_job_masks(clip, frame_size, job, detector, config)?;
        if config.temporal_smooth > 0 {
            smooth_masks_temporal(&mut masks, config.temporal_smooth);
        }
        if masks.iter().all(|mask| mask.is_empty()) {
            continue;
        }
        for (index, mask) in masks.iter().enumerate() {
            merge_mask_into(&mut merged_masks[index], union_roi, job.roi, mask);
        }

        let mut patch = ClipPatch::new(job.roi_size);
        for frame in clip.iter() {
            patch.push(crop_roi(frame, frame_size, job.roi));
        }
        inpainter.inpaint_frames(&mut patch, &masks, config.feather_radius)?;

        for (index, frame) in clip.iter_mut().enumerate() {
            if let (Some(inpainted), Some(per_frame)) = (patch.frames.get(index), masks.get(index))
            {
                if !per_frame.is_empty() {
                    let alpha = feather(per_frame, config.feather_radius);
                    paste_roi(frame, frame_size, job.roi, inpainted, Some(&alpha));
                }
            }
        }
        report.inpaint_batches += 1;
    }

    let Some(union_mask) = union_masks(&merged_masks) else {
        // 本片段没有标识，直接输出；同时丢弃时序状态，避免把相隔很远的
        // 两帧当作相邻帧做闪烁/一致性比较。
        if let Some(verifier) = verifier.as_mut() {
            verifier.reset_temporal();
        }
        for frame in clip.iter() {
            sink.write_frame(frame)?;
            report.frames_out += 1;
        }
        return Ok(());
    };

    if let Some(verifier) = verifier.as_mut() {
        verifier
            .set_mask(&union_mask)
            .map_err(|error| PipelineError::Verify(error.to_string()))?;
    }

    for frame in clip.iter_mut() {
        if let Some(verifier) = verifier.as_mut() {
            verifier
                .observe(report.frames_out, &frame[..], detector)
                .map_err(|error| PipelineError::Verify(error.to_string()))?;
        }
        sink.write_frame(frame)?;
        report.frames_out += 1;
    }

    report.inpainted_clips += 1;
    *coverage_sum += union_mask.coverage();
    Ok(())
}

/// 为一个作业生成逐帧掩码（只在作业自己的区域内采纳检测框）。
fn build_job_masks(
    clip: &[FrameBuffer],
    frame_size: FrameSize,
    job: &RoiJob,
    detector: &dyn TextDetector,
    config: &PipelineConfig,
) -> Result<Vec<BinaryMask>, PipelineError> {
    let mut masks: Vec<BinaryMask> = Vec::with_capacity(clip.len());
    for frame in clip.iter() {
        let boxes = filter_by_regions(
            &filter_by_score(
                detect_boxes(detector, frame, frame_size, job.roi)?,
                config.detect_min_score,
            ),
            &job.regions,
            config.region_pad,
            frame_size,
        );
        let mut mask = BinaryMask::empty(job.roi_size);
        for box_ in boxes {
            mask.paint_rect(job.roi, box_.rect());
        }
        if !mask.is_empty() {
            mask.dilate(config.mask_dilate);
        }
        masks.push(mask);
    }
    Ok(masks)
}

/// 把作业坐标系的掩码并进并集 ROI 坐标系的掩码。
fn merge_mask_into(
    dest: &mut BinaryMask,
    union_roi: MaskRect,
    job_roi: MaskRect,
    src: &BinaryMask,
) {
    if dest.size() != FrameSize::new(union_roi.width, union_roi.height) {
        return;
    }
    let offset_x = job_roi.x.saturating_sub(union_roi.x);
    let offset_y = job_roi.y.saturating_sub(union_roi.y);
    let size = src.size();
    for y in 0..size.height {
        for x in 0..size.width {
            if src.get(x, y) {
                dest.set(x + offset_x, y + offset_y, true);
            }
        }
    }
}

/// 按后端偏好决定检测输入：ROI 裁剪图（省算力）或整帧。
///
/// 返回的框一律是整帧坐标。
fn detect_boxes(
    detector: &dyn TextDetector,
    frame: &[u8],
    frame_size: FrameSize,
    roi: MaskRect,
) -> Result<Vec<TextBox>, DetectError> {
    let covers_frame = roi.x == 0
        && roi.y == 0
        && roi.width == frame_size.width
        && roi.height == frame_size.height;
    if !detector.prefer_roi_detection() || covers_frame {
        return detector.detect(frame, frame_size);
    }
    let crop = crop_roi(frame, frame_size, roi);
    let roi_size = FrameSize::new(roi.width, roi.height);
    let boxes = detector.detect(&crop, roi_size)?;
    Ok(boxes
        .iter()
        .filter_map(|box_| offset_box(box_, roi.x, roi.y, frame_size))
        .collect())
}

/// 掩码时序平滑：把半径 `radius` 内的相邻帧掩码并入当前帧掩码。
///
/// 逐帧检测难免有漏检；漏检那一帧的标识不会被修复，回放时看起来就像一个
/// 闪一下的残影。把相邻帧的掩码互相取并集，可以让同一块标识在整段里都被
/// 稳定修复，代价是掩码略大。
fn smooth_masks_temporal(masks: &mut [BinaryMask], radius: u32) {
    if radius == 0 || masks.len() < 2 {
        return;
    }
    let original: Vec<BinaryMask> = masks.to_vec();
    for (index, mask) in masks.iter_mut().enumerate() {
        let start = index.saturating_sub(radius as usize);
        let end = (index + radius as usize).min(original.len() - 1);
        let target = mask.data_mut();
        for neighbour in &original[start..=end] {
            for (slot, value) in target.iter_mut().zip(neighbour.data().iter()) {
                if *value > 0 {
                    *slot = 255;
                }
            }
        }
    }
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
    use crate::detect::{DetectError, FixedRoiDetector, FixedRoisDetector, TextBox};
    use crate::frames::{frame_bytes, FrameReader, CHANNELS};
    use crate::inpaint::{ColumnFillInpainter, InpaintError, Inpainter};
    use crate::verify::{Verdict, VerifyConfig};
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
    /// 内容驱动的文字检测桩：给定区域平均亮度超过阈值时返回该区域。
    struct BarDetector {
        bar: MaskRect,
        threshold: f32,
    }

    impl BarDetector {
        fn new(bar: MaskRect, threshold: f32) -> Self {
            Self { bar, threshold }
        }
    }

    impl TextDetector for BarDetector {
        fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            let stride = size.width as usize * CHANNELS;
            let mut sum = 0.0_f64;
            let mut count = 0_u64;
            for y in self.bar.y..self.bar.bottom() {
                for x in self.bar.x..self.bar.right() {
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
                    self.bar.x,
                    self.bar.y,
                    self.bar.width,
                    self.bar.height,
                    0.99,
                )]
            } else {
                Vec::new()
            })
        }
    }

    /// 什么都不做的修复桩：模拟「修复失败、字幕原样保留」。
    struct NoOpInpainter;

    impl Inpainter for NoOpInpainter {
        fn inpaint(
            &self,
            _patch: &mut ClipPatch,
            _mask: &BinaryMask,
            _feather_radius: u32,
        ) -> Result<(), InpaintError> {
            Ok(())
        }
    }

    fn verifying_config() -> PipelineConfig {
        PipelineConfig {
            clip_frames: 4,
            probe_frames: 4,
            context_scale: 2.0,
            mask_dilate: 2,
            feather_radius: 1,
            verify: Some(VerifyConfig::default()),
            ..Default::default()
        }
    }

    #[test]
    fn verification_passes_for_clean_repair() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_video(size, 8, bar);
        let detector = BarDetector::new(bar, 200.0);

        let (report, _frames) = run(&data, size, &detector, &verifying_config());
        let verification = report.verification.expect("应产出校验报告");
        assert!(verification.applicable, "报告：{verification:?}");
        assert!(verification.residual.checked);
        assert!(
            verification.residual.frames_with_residual <= 1,
            "残留帧过多：{verification:?}"
        );
        assert_eq!(
            verification.verdict,
            Verdict::Pass,
            "报告：{verification:?}"
        );
    }

    #[test]
    fn verification_fails_when_subtitle_survives() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_video(size, 8, bar);
        let detector = BarDetector::new(bar, 200.0);

        let (report, _frames) =
            run_with(&data, size, &detector, &NoOpInpainter, &verifying_config());
        let verification = report.verification.expect("应产出校验报告");
        assert_eq!(report.inpainted_clips, 2, "测试前提：两段都应走修复路径");
        assert!(verification.residual.max_ratio > 0.5);
        assert_eq!(verification.verdict, Verdict::Fail);
        assert!(verification
            .issues
            .iter()
            .any(|issue| issue.code == "residual"));
    }

    #[test]
    fn verification_is_absent_when_disabled() {
        let size = FrameSize::new(32, 24);
        let bar = MaskRect::new(4, 16, 24, 4);
        let data = synthetic_video(size, 4, bar);
        let config = PipelineConfig {
            clip_frames: 2,
            probe_frames: 2,
            context_scale: 2.0,
            ..Default::default()
        };
        let (report, _) = run(&data, size, &FixedRoiDetector::new(bar), &config);
        assert!(report.verification.is_none());
    }

    #[test]
    fn verification_report_round_trips_as_json() {
        let size = FrameSize::new(32, 24);
        let bar = MaskRect::new(4, 16, 24, 4);
        let data = synthetic_video(size, 4, bar);
        let config = PipelineConfig {
            clip_frames: 2,
            probe_frames: 2,
            context_scale: 2.0,
            verify: Some(VerifyConfig::default()),
            ..Default::default()
        };
        let (report, _) = run(&data, size, &FixedRoiDetector::new(bar), &config);
        let json = serde_json::to_string(&report).expect("报告应可序列化");
        let restored: PipelineReport = serde_json::from_str(&json).expect("报告应可反序列化");
        assert_eq!(restored, report);
    }

    /// 只把给定区域中「确实是亮块」的框返回，用于多区域场景。
    struct BrightRegionsDetector {
        regions: Vec<MaskRect>,
        threshold: f32,
    }

    impl BrightRegionsDetector {
        fn new(regions: Vec<MaskRect>, threshold: f32) -> Self {
            Self { regions, threshold }
        }
    }

    impl TextDetector for BrightRegionsDetector {
        fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            let stride = size.width as usize * CHANNELS;
            let mut boxes = Vec::new();
            for region in &self.regions {
                let mut sum = 0.0_f64;
                let mut count = 0_u64;
                for y in region.y..region.bottom() {
                    for x in region.x..region.right() {
                        let base = y as usize * stride + x as usize * CHANNELS;
                        sum += f64::from(frame[base]);
                        count += 1;
                    }
                }
                let mean = if count == 0 { 0.0 } else { sum / count as f64 };
                if mean > f64::from(self.threshold) {
                    boxes.push(TextBox::new(
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        0.99,
                    ));
                }
            }
            Ok(boxes)
        }
    }

    /// 交替返回检测框，用来验证逐帧掩码与时序平滑。
    struct AlternatingDetector {
        rect: MaskRect,
        calls: std::cell::Cell<usize>,
    }

    impl AlternatingDetector {
        fn new(rect: MaskRect) -> Self {
            Self {
                rect,
                calls: std::cell::Cell::new(0),
            }
        }
    }

    impl TextDetector for AlternatingDetector {
        fn detect(&self, _frame: &[u8], _size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            Ok(if call.is_multiple_of(2) {
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

    /// 统计调用次数，用来验证「显式区域时跳过定位阶段」。
    struct CountingDetector {
        rect: MaskRect,
        calls: std::cell::Cell<usize>,
    }

    impl TextDetector for CountingDetector {
        fn detect(&self, _frame: &[u8], _size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            self.calls.set(self.calls.get() + 1);
            Ok(vec![TextBox::new(
                self.rect.x,
                self.rect.y,
                self.rect.width,
                self.rect.height,
                0.99,
            )])
        }
    }

    /// 构造 64x48 的合成视频：`bars` 里的每个矩形都是亮块。
    fn synthetic_bars(size: FrameSize, frames: usize, bars: &[MaskRect]) -> Vec<u8> {
        let stride = size.width as usize * 3;
        let mut data = Vec::with_capacity(frame_bytes(size) * frames);
        for _ in 0..frames {
            let mut frame = vec![40_u8; stride * size.height as usize];
            for bar in bars {
                for y in bar.y..bar.bottom() {
                    let start = y as usize * stride + bar.x as usize * 3;
                    let end = start + bar.width as usize * 3;
                    frame[start..end].fill(250);
                }
            }
            data.extend_from_slice(&frame);
        }
        data
    }

    fn luma_at(frame: &[u8], size: FrameSize, x: u32, y: u32) -> u8 {
        let base = (y as usize * size.width as usize + x as usize) * 3;
        frame[base]
    }

    #[test]
    fn time_window_only_repairs_frames_inside_it() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_bars(size, 10, &[bar]);
        let config = PipelineConfig {
            clip_frames: 4,
            probe_frames: 2,
            context_scale: 2.0,
            mask_dilate: 1,
            feather_radius: 1,
            temporal_smooth: 0,
            start_frame: Some(2),
            end_frame: Some(6),
            ..Default::default()
        };
        let detector = BrightRegionsDetector::new(vec![bar], 200.0);

        let (report, frames) = run(&data, size, &detector, &config);

        assert_eq!(report.frames_in, 10);
        assert_eq!(report.frames_out, 10);
        assert_eq!(report.clips, 1, "窗口内 4 帧应合成一个片段");
        assert_eq!(report.inpainted_clips, 1);
        let x = bar.x + bar.width / 2;
        let y = bar.y + bar.height / 2;
        for (index, frame) in frames.iter().enumerate() {
            let value = luma_at(frame, size, x, y);
            if (2..6).contains(&index) {
                assert_eq!(value, 40, "第 {index} 帧在时间窗内，应被修复");
            } else {
                assert_eq!(value, 250, "第 {index} 帧在时间窗外，应原样保留");
            }
        }
    }

    #[test]
    fn multiple_regions_are_repaired_independently() {
        let size = FrameSize::new(64, 48);
        let subtitle = MaskRect::new(4, 36, 56, 6);
        let logo = MaskRect::new(48, 4, 12, 8);
        let data = synthetic_bars(size, 6, &[subtitle, logo]);
        let config = PipelineConfig {
            clip_frames: 3,
            probe_frames: 2,
            context_scale: 1.6,
            mask_dilate: 1,
            feather_radius: 1,
            temporal_smooth: 0,
            regions: vec![subtitle, logo],
            ..Default::default()
        };
        let detector = BrightRegionsDetector::new(vec![subtitle, logo], 200.0);

        let (report, frames) = run(&data, size, &detector, &config);

        assert_eq!(report.regions, vec![subtitle, logo]);
        assert_eq!(report.inpainted_clips, 2);
        let center = |rect: MaskRect| (rect.x + rect.width / 2, rect.y + rect.height / 2);
        for frame in &frames {
            let (sx, sy) = center(subtitle);
            let (lx, ly) = center(logo);
            assert_eq!(luma_at(frame, size, sx, sy), 40, "字幕区域应被修复");
            assert_eq!(luma_at(frame, size, lx, ly), 40, "角标区域应被修复");
            // 两个区域之间是正常画面，必须保持原样（40 背景 + 未修复像素）
            assert_eq!(luma_at(frame, size, 32, 20), 40);
        }
    }

    #[test]
    fn per_frame_masks_track_detections_and_smoothing_fills_gaps() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 20, 48, 8);
        let data = synthetic_bars(size, 6, &[bar]);
        let x = bar.x + bar.width / 2;
        let y = bar.y + bar.height / 2;

        // 不平滑：只有被检测到的帧（偶数帧）才修复
        let config = PipelineConfig {
            clip_frames: 6,
            probe_frames: 1,
            context_scale: 2.0,
            mask_dilate: 1,
            feather_radius: 0,
            temporal_smooth: 0,
            regions: vec![bar],
            ..Default::default()
        };
        let (report, frames) = run(&data, size, &AlternatingDetector::new(bar), &config);
        assert_eq!(report.inpainted_clips, 1);
        for (index, frame) in frames.iter().enumerate() {
            let value = luma_at(frame, size, x, y);
            if index.is_multiple_of(2) {
                assert_eq!(value, 40, "第 {index} 帧有检测，应被修复");
            } else {
                assert_eq!(value, 250, "第 {index} 帧漏检，掩码为空，应保持原样");
            }
        }

        // 时序平滑半径 1：相邻帧掩码取并集，漏检帧也被修复
        let config = PipelineConfig {
            temporal_smooth: 1,
            ..config
        };
        let (_report, frames) = run(&data, size, &AlternatingDetector::new(bar), &config);
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(
                luma_at(frame, size, x, y),
                40,
                "第 {index} 帧经时序平滑后应被修复"
            );
        }
    }

    #[test]
    fn explicit_regions_skip_the_probing_pass() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_bars(size, 4, &[bar]);

        let detector = CountingDetector {
            rect: bar,
            calls: std::cell::Cell::new(0),
        };
        let config = PipelineConfig {
            clip_frames: 2,
            probe_frames: 2,
            context_scale: 2.0,
            regions: vec![bar],
            temporal_smooth: 0,
            ..Default::default()
        };
        let (_report, _frames) = run(&data, size, &detector, &config);
        assert_eq!(
            detector.calls.get(),
            4,
            "显式区域应跳过探测，只按帧检测 4 次"
        );

        // 不指定区域时才需要探测：探测 2 帧 + 逐帧 4 次
        let detector = CountingDetector {
            rect: bar,
            calls: std::cell::Cell::new(0),
        };
        let config = PipelineConfig {
            regions: Vec::new(),
            probe_frames: 2,
            ..config
        };
        let (_report, _frames) = run(&data, size, &detector, &config);
        assert_eq!(detector.calls.get(), 6, "探测阶段应额外检测 2 帧");
    }

    /// 只在 ROI 裁剪图上检测的后端（模拟 DBNet worker 的省算力路径）。
    struct RoiLocalDetector {
        expected: FrameSize,
        calls: std::cell::Cell<usize>,
    }

    impl TextDetector for RoiLocalDetector {
        fn prefer_roi_detection(&self) -> bool {
            true
        }

        fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            assert_eq!(size, self.expected, "管线应只把 ROI 裁剪图交给后端");
            self.calls.set(self.calls.get() + 1);
            // 裁剪图是 64x48 帧里的字幕条外扩 ROI：亮块位于裁剪图内部
            let stride = size.width as usize * CHANNELS;
            let mut bright_rows = Vec::new();
            for y in 0..size.height {
                let row = &frame[y as usize * stride..(y as usize + 1) * stride];
                let mean = row.iter().map(|value| f64::from(*value)).sum::<f64>()
                    / row.len().max(1) as f64;
                if mean > 150.0 {
                    bright_rows.push(y);
                }
            }
            if bright_rows.is_empty() {
                return Ok(Vec::new());
            }
            let top = bright_rows[0];
            let bottom = bright_rows[bright_rows.len() - 1];
            Ok(vec![TextBox::new(
                0,
                top,
                size.width,
                bottom - top + 1,
                0.99,
            )])
        }
    }

    #[test]
    fn roi_aware_detector_only_sees_the_crop_and_boxes_are_offset_back() {
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_bars(size, 4, &[bar]);
        let config = PipelineConfig {
            clip_frames: 2,
            probe_frames: 2,
            context_scale: 2.0,
            mask_dilate: 1,
            feather_radius: 1,
            temporal_smooth: 0,
            regions: vec![bar],
            ..Default::default()
        };
        let roi = bar.scale(2.0, size).unwrap();
        let detector = RoiLocalDetector {
            expected: FrameSize::new(roi.width, roi.height),
            calls: std::cell::Cell::new(0),
        };

        let (report, frames) = run(&data, size, &detector, &config);

        assert_eq!(detector.calls.get(), 4, "探测阶段应被跳过，只按帧检测");
        assert_eq!(report.inpainted_clips, 2);
        let x = bar.x + bar.width / 2;
        let y = bar.y + bar.height / 2;
        for frame in &frames {
            assert_eq!(
                luma_at(frame, size, x, y),
                40,
                "裁剪坐标下的检测框应被正确平移回整帧坐标并修复"
            );
        }
    }
    /// 记录每次检测收到的画面尺寸，并返回「高亮像素」的外接框（裁剪坐标）。
    ///
    /// 它模拟真实检测器的关键性质：**只看到传进来的那幅画面**，不知道区域配置。
    struct SizeRecordingDetector {
        seen: std::cell::RefCell<Vec<(u32, u32)>>,
    }

    impl SizeRecordingDetector {
        fn new() -> Self {
            Self {
                seen: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl TextDetector for SizeRecordingDetector {
        fn prefer_roi_detection(&self) -> bool {
            true
        }

        fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
            self.seen.borrow_mut().push((size.width, size.height));
            let stride = size.width as usize * 3;
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0_u32, 0_u32);
            let mut found = false;
            for y in 0..size.height {
                for x in 0..size.width {
                    let index = (y as usize) * stride + (x as usize) * 3;
                    if frame[index] >= 200 {
                        found = true;
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                    }
                }
            }
            Ok(if found {
                vec![TextBox::new(
                    min_x,
                    min_y,
                    max_x - min_x + 1,
                    max_y - min_y + 1,
                    0.9,
                )]
            } else {
                Vec::new()
            })
        }
    }

    fn two_bars(size: FrameSize, frames: usize, bars: &[MaskRect]) -> Vec<u8> {
        let stride = (size.width as usize) * 3;
        let mut data = Vec::with_capacity(frame_bytes(size) * frames);
        for _ in 0..frames {
            let mut frame = vec![40_u8; stride * size.height as usize];
            for bar in bars {
                for y in bar.y..bar.bottom() {
                    let start = (y as usize) * stride + (bar.x as usize) * 3;
                    let end = start + (bar.width as usize) * 3;
                    frame[start..end].fill(250);
                }
            }
            data.extend_from_slice(&frame);
        }
        data
    }

    #[test]
    fn nearby_regions_share_one_group_and_distant_regions_split() {
        let size = FrameSize::new(320, 180);
        let left = MaskRect::new(10, 10, 30, 12);
        let near = MaskRect::new(60, 12, 30, 12);
        let far = MaskRect::new(260, 150, 30, 12);

        assert_eq!(
            plan_roi_jobs(&[left, near], 1.6, 2.0, size).len(),
            1,
            "挨得近的两处标识应共用一次裁剪"
        );
        assert_eq!(
            plan_roi_jobs(&[left, far], 1.6, 2.0, size).len(),
            2,
            "对角线两端的标识应拆成两个作业"
        );
        assert_eq!(
            plan_roi_jobs(&[left, far], 1.6, 0.0, size).len(),
            1,
            "比例为 0 时应等价于旧行为（一次外接 ROI）"
        );
    }

    #[test]
    fn each_group_only_sees_its_own_roi_and_both_are_repaired() {
        let size = FrameSize::new(320, 180);
        let logo = MaskRect::new(10, 10, 30, 12);
        let subtitle = MaskRect::new(80, 150, 160, 12);
        let data = two_bars(size, 4, &[logo, subtitle]);
        let detector = SizeRecordingDetector::new();
        let config = PipelineConfig {
            clip_frames: 4,
            probe_frames: 1,
            context_scale: 1.6,
            mask_dilate: 1,
            feather_radius: 1,
            temporal_smooth: 0,
            regions: vec![logo, subtitle],
            region_pad: 8,
            ..Default::default()
        };

        let (report, frames) = run(&data, size, &detector, &config);

        assert_eq!(report.roi_jobs, 2, "两处标识离得远，应拆成两个作业");
        assert_eq!(report.inpaint_batches, 2, "两个作业各提交一次修复");
        assert_eq!(report.inpainted_clips, 1, "但仍然只算一个片段");

        let union = report.roi.unwrap();
        for (width, height) in detector.seen.borrow().iter() {
            assert!(
                *width < union.width && *height < union.height,
                "分组后每次检测的画面 {width}x{height} 应小于外接 ROI {}x{}",
                union.width,
                union.height
            );
        }
        for frame in &frames {
            assert_eq!(
                luma_at(frame, size, logo.x + 5, logo.y + 5),
                40,
                "角标应被清除"
            );
            assert_eq!(
                luma_at(frame, size, subtitle.x + 5, subtitle.y + 5),
                40,
                "字幕应被清除"
            );
        }
    }

    #[test]
    fn merged_masks_stay_aligned_with_the_union_roi() {
        let union_roi = MaskRect::new(0, 0, 64, 48);
        let job_roi = MaskRect::new(10, 20, 8, 4);
        let mut dest = BinaryMask::empty(FrameSize::new(union_roi.width, union_roi.height));
        let mut src = BinaryMask::empty(FrameSize::new(job_roi.width, job_roi.height));
        src.set(0, 0, true);
        src.set(7, 3, true);

        merge_mask_into(&mut dest, union_roi, job_roi, &src);

        assert!(dest.get(10, 20), "作业原点应平移到并集坐标系");
        assert!(dest.get(17, 23), "作业右下角也应平移");
        assert!(!dest.get(0, 0), "作业之外不应被涂上");
    }

    fn progress_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dossier-inpaint-progress-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn progress_stream(path: &std::path::Path, est_total_frames: u64) -> ProgressStream {
        ProgressStream {
            path: path.to_path_buf(),
            task_id: "ep01".into(),
            pipeline: "drama.logo.erase".into(),
            est_total_frames,
        }
    }

    /// 成功路径：两阶段、seq 连续、阶段不回退、逐片段计数单调，最后落到终态。
    #[test]
    fn progress_stream_records_two_stages_and_ends_terminal() {
        let dir = progress_dir("ok");
        let path = dir.join("progress.jsonl");
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_video(size, 10, bar);
        let detector = FixedRoiDetector::new(bar);
        let config = PipelineConfig {
            clip_frames: 4,
            probe_frames: 5,
            context_scale: 2.0,
            mask_dilate: 1,
            feather_radius: 1,
            progress: Some(progress_stream(&path, 10)),
            ..Default::default()
        };
        let (report, _) = run(&data, size, &detector, &config);
        assert_eq!(report.frames_out, 10);

        let log = dossier_core::progress::load_progress_log(&path).unwrap();
        assert_eq!(log.skipped_lines, 0);
        assert!(!log.truncated_tail);
        assert!(log.is_terminal(), "跑完必须留下终态");
        assert_eq!(log.events[0].kind, ProgressEventKind::Started);
        assert_eq!(log.events[0].progress.stage, LOCATE_STAGE_NAME);
        assert_eq!(log.events[0].progress.task_id, "ep01");
        assert_eq!(log.events[0].progress.pipeline, "drama.logo.erase");

        let mut stage = 0;
        let mut done = 0;
        for (index, event) in log.events.iter().enumerate() {
            assert_eq!(event.seq, index as u64 + 1, "seq 必须连续递增");
            assert!(event.progress.stage_index >= stage, "阶段序号不允许回退");
            if event.progress.stage_index > stage {
                stage = event.progress.stage_index;
                done = 0;
            }
            if event.kind == ProgressEventKind::Progress {
                assert!(event.progress.unit_done >= done, "阶段内计数不允许回退");
                done = event.progress.unit_done;
            }
        }
        assert_eq!(log.last().unwrap().kind, ProgressEventKind::Finished);
        let last = log.current().unwrap();
        assert_eq!(last.stage, INPAINT_STAGE_NAME);
        assert_eq!(
            last.unit_done, report.frames_out,
            "末条进度要对得上真实帧数"
        );
        assert_eq!(last.unit_done, last.unit_total);

        // 自动定位期间要有中间进度：只在阶段首尾各报一条，界面上与「卡死」无法区分。
        let locate: Vec<_> = log
            .events
            .iter()
            .filter(|event| event.progress.stage_index == LOCATE_STAGE)
            .collect();
        assert!(
            locate.len() >= 3,
            "定位阶段应逐帧上报，实得 {}",
            locate.len()
        );
        assert!(locate
            .last()
            .unwrap()
            .progress
            .message
            .contains("聚合出 1 个标识区域"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没检测到标识（整条原样输出）也必须留下终态，否则进度文件永远停在「运行中」。
    #[test]
    fn progress_stream_is_terminal_when_nothing_is_detected() {
        let dir = progress_dir("none");
        let path = dir.join("progress.jsonl");
        let size = FrameSize::new(64, 48);
        let data = synthetic_video(size, 6, MaskRect::new(8, 36, 48, 6));
        let config = PipelineConfig {
            probe_frames: 3,
            progress: Some(progress_stream(&path, 6)),
            ..Default::default()
        };
        let (report, _) = run_with(
            &data,
            size,
            &FixedRoisDetector::new(Vec::new()),
            &ColumnFillInpainter { sample_rows: 4 },
            &config,
        );
        assert!(report.roi.is_none(), "前提：这个检测器什么都不报");
        assert_eq!(report.frames_out, 6);

        let log = dossier_core::progress::load_progress_log(&path).unwrap();
        assert!(log.is_terminal());
        let last = log.current().unwrap();
        assert_eq!(last.stage, INPAINT_STAGE_NAME, "收尾要落在第二个阶段上");
        assert_eq!(last.unit_done, 6, "原样输出同样算作真实处理的帧数");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct FailingInpainter;

    impl Inpainter for FailingInpainter {
        fn inpaint(
            &self,
            _patch: &mut ClipPatch,
            _mask: &BinaryMask,
            _feather_radius: u32,
        ) -> Result<(), InpaintError> {
            Err(InpaintError::Backend("测试用失败".into()))
        }
    }

    /// 失败必须留下 Failed 终态，否则 tailer 会一直以为任务还在跑。
    #[test]
    fn progress_stream_records_failed_terminal() {
        let dir = progress_dir("failed");
        let path = dir.join("progress.jsonl");
        let size = FrameSize::new(64, 48);
        let bar = MaskRect::new(8, 36, 48, 6);
        let data = synthetic_video(size, 4, bar);
        let detector = FixedRoiDetector::new(bar);
        let config = PipelineConfig {
            clip_frames: 2,
            probe_frames: 2,
            context_scale: 2.0,
            mask_dilate: 1,
            feather_radius: 1,
            progress: Some(progress_stream(&path, 4)),
            ..Default::default()
        };
        let mut sink: Vec<Vec<u8>> = Vec::new();
        let error = run_pipeline(
            || Ok(FrameReader::new(Cursor::new(data.clone()), size)),
            &mut sink,
            size,
            &detector,
            &FailingInpainter,
            &config,
        )
        .expect_err("修复后端失败时管线必须报错");
        assert!(matches!(error, PipelineError::Inpaint(_)));

        let log = dossier_core::progress::load_progress_log(&path).unwrap();
        assert!(log.is_terminal());
        assert_eq!(log.last().unwrap().kind, ProgressEventKind::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
