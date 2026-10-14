//! 字幕文本检测：结果类型、跨帧聚合与后端抽象。
//!
//! DBNet 本体放在 Python worker（`tools/model_worker`）里运行，Rust 侧只负责
//! 结果建模与「把逐帧检测收敛成稳定 ROI」的聚合逻辑。这样既可以用真实模型，
//! 也可以用固定 ROI 或桩实现做完全离线的单元测试。

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mask::{FrameSize, MaskRect};
use crate::worker::WorkerSession;

/// 检测到的一行文本（画面像素坐标）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// DBNet 置信度，0.0–1.0。
    #[serde(default = "default_score")]
    pub score: f32,
}

fn default_score() -> f32 {
    1.0
}

impl TextBox {
    pub fn new(x: u32, y: u32, width: u32, height: u32, score: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            score,
        }
    }

    pub fn rect(&self) -> MaskRect {
        MaskRect::new(self.x, self.y, self.width, self.height)
    }
}

#[derive(Debug, Error)]
pub enum DetectError {
    #[error("检测后端调用失败：{0}")]
    Backend(String),
    #[error("检测结果格式不符合预期：{0}")]
    Output(String),
    #[error("检测后端 I/O 失败：{0}")]
    Io(#[from] std::io::Error),
}

/// 文本检测后端。
pub trait TextDetector {
    /// 返回该帧的文本行框；无字幕时返回空列表。
    fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError>;

    /// 检测是否由画面内容驱动。
    ///
    /// 「固定 ROI」这类后端无论画面内容如何都返回同一个框，因此不能用来
    /// 判断修复后是否还有残留文本；`verify` 模块会据此跳过残留检查。
    fn is_content_based(&self) -> bool {
        true
    }

    /// 是否希望只在 ROI 裁剪图上检测。
    ///
    /// 内容驱动的模型（DBNet）在 640×64 的 ROI 上比整帧 1080p 便宜近一个数量级，
    /// 而且位置已经确定，没有理由每帧再扫全图。只有真正按内容检测的后端应返回
    /// `true`：判定为 `true` 时管线会用 ROI 尺寸调用 [`TextDetector::detect`]，
    /// 并把返回的框从裁剪坐标平移回整帧坐标。
    ///
    /// 默认 `false`（整帧检测），避免依赖画面绝对坐标的后端（如固定区域）被误裁。
    fn prefer_roi_detection(&self) -> bool {
        false
    }
}

/// 把 ROI 裁剪坐标下的检测框平移回整帧坐标；完全落在画面外时返回 `None`。
pub fn offset_box(box_: &TextBox, dx: u32, dy: u32, frame: FrameSize) -> Option<TextBox> {
    let rect = MaskRect::new(
        box_.x.saturating_add(dx),
        box_.y.saturating_add(dy),
        box_.width,
        box_.height,
    )
    .clip(frame)?;
    Some(TextBox::new(
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        box_.score,
    ))
}

/// 固定 ROI 检测器：把使用者给定的区域当作每帧的字幕区域。
///
/// 适用场景：字幕位置固定（绝大多数短剧），且不必为每帧跑模型；
/// 也是没有模型时的可用回退方案。
#[derive(Debug, Clone, Copy)]
pub struct FixedRoiDetector {
    rect: MaskRect,
}

impl FixedRoiDetector {
    pub fn new(rect: MaskRect) -> Self {
        Self { rect }
    }
}

impl TextDetector for FixedRoiDetector {
    fn is_content_based(&self) -> bool {
        false
    }

    fn detect(&self, _frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
        Ok(match self.rect.clip(size) {
            Some(rect) if !rect.is_empty() => {
                vec![TextBox::new(rect.x, rect.y, rect.width, rect.height, 1.0)]
            }
            _ => Vec::new(),
        })
    }
}

/// 多区域固定检测器：把若干个使用者给定的区域当作每帧的标识区域。
///
/// 与 [`FixedRoiDetector`] 的关系是「一个区域 vs 多个区域」；短剧里常见
/// 「底部硬字幕 + 右上角台标」两个互不相邻的标识，各自需要有独立的掩码。
#[derive(Debug, Clone)]
pub struct FixedRoisDetector {
    rects: Vec<MaskRect>,
}

impl FixedRoisDetector {
    pub fn new(rects: Vec<MaskRect>) -> Self {
        Self { rects }
    }

    pub fn rects(&self) -> &[MaskRect] {
        &self.rects
    }
}

impl TextDetector for FixedRoisDetector {
    fn is_content_based(&self) -> bool {
        false
    }

    fn detect(&self, _frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
        Ok(self
            .rects
            .iter()
            .filter_map(|rect| rect.clip(size))
            .filter(|rect| !rect.is_empty())
            .map(|rect| TextBox::new(rect.x, rect.y, rect.width, rect.height, 1.0))
            .collect())
    }
}

/// 通过外部命令检测（例如 Python worker）的通用配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerConfig {
    /// 可执行文件，如 `python3`。
    pub program: PathBuf,
    /// 传给可执行文件的参数，如 `tools/model_worker/worker.py`。
    #[serde(default)]
    pub args: Vec<String>,
}

/// 通过外部进程做文本检测的后端（DBNet）。
///
/// 交换协议（全部在 `work_dir` 内，便于人工复现）：
/// - 输入：`in/frame.raw`（整帧 bgr24）、`request.json`
/// - 输出：`boxes.json`，形如 `{"boxes":[{"x":..,"y":..,"width":..,"height":..,"score":..}]}`
///
/// 之所以传整帧而不是 ROI：DBNet 是逐帧的，位置抖动需要由 Rust 侧聚合处理。
pub struct WorkerTextDetector {
    pub config: WorkerConfig,
    pub work_dir: PathBuf,
    /// 可选的常驻 worker 会话；`None` 时每次检测单独起进程。
    session: Option<Arc<Mutex<WorkerSession>>>,
}

impl WorkerTextDetector {
    pub fn new(config: WorkerConfig, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            config,
            work_dir: work_dir.into(),
            session: None,
        }
    }

    /// 复用外部传入的常驻 worker（与修复后端共享同一个进程）。
    pub fn with_session(mut self, session: Arc<Mutex<WorkerSession>>) -> Self {
        self.session = Some(session);
        self
    }

    /// 把 `work_dir` 里的请求交给后端：常驻会话优先，否则一次性起进程。
    fn invoke_backend(&self) -> Result<(), DetectError> {
        if let Some(session) = &self.session {
            let mut guard = session
                .lock()
                .map_err(|_| DetectError::Backend("常驻 worker 会话已中毒".into()))?;
            return guard
                .run(&self.work_dir)
                .map_err(|error| DetectError::Backend(error.to_string()));
        }
        let output = Command::new(&self.config.program)
            .args(&self.config.args)
            .arg(&self.work_dir)
            .output()
            .map_err(|error| {
                DetectError::Backend(format!(
                    "无法启动检测后端 {}：{error}",
                    self.config.program.display()
                ))
            })?;
        if !output.status.success() {
            return Err(DetectError::Backend(format!(
                "检测后端退出码 {:?}：{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct BoxesFile {
    #[serde(default)]
    boxes: Vec<TextBox>,
}

impl TextDetector for WorkerTextDetector {
    fn prefer_roi_detection(&self) -> bool {
        true
    }

    fn detect(&self, frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
        let expected = crate::frames::frame_bytes(size);
        if frame.len() < expected {
            return Err(DetectError::Output(format!(
                "帧数据不足：期望 {expected} 字节，实际 {}",
                frame.len()
            )));
        }
        let in_dir = self.work_dir.join("in");
        if self.work_dir.exists() {
            std::fs::remove_dir_all(&self.work_dir)?;
        }
        std::fs::create_dir_all(&in_dir)?;
        std::fs::write(in_dir.join("frame.raw"), &frame[..expected])?;
        let request = serde_json::json!({
            "mode": "detect",
            "width": size.width,
            "height": size.height,
            "frameFile": "in/frame.raw",
            "outputFile": "boxes.json",
        });
        std::fs::write(
            self.work_dir.join("request.json"),
            serde_json::to_vec_pretty(&request)
                .map_err(|error| DetectError::Backend(format!("请求序列化失败：{error}")))?,
        )?;

        self.invoke_backend()?;

        let path = self.work_dir.join("boxes.json");
        let raw = std::fs::read(&path).map_err(|error| {
            DetectError::Output(format!("缺少检测结果 {}：{error}", path.display()))
        })?;
        let parsed: BoxesFile = serde_json::from_slice(&raw)
            .map_err(|error| DetectError::Output(format!("boxes.json 解析失败：{error}")))?;
        Ok(parsed.boxes)
    }
}

/// 两个矩形的最小外接矩形。
pub fn union_rects(rects: &[MaskRect]) -> Option<MaskRect> {
    rects
        .iter()
        .copied()
        .reduce(|acc, rect| acc.union(&rect).unwrap_or(acc))
}

/// 交并比；任一矩形为空时返回 0.0。
pub fn iou(a: MaskRect, b: MaskRect) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let Some(overlap) = a.intersect(&b) else {
        return 0.0;
    };
    let inter = overlap.area() as f64;
    let union = (a.area() + b.area()) as f64 - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// 按置信度过滤。
pub fn filter_by_score(boxes: Vec<TextBox>, min_score: f32) -> Vec<TextBox> {
    boxes
        .into_iter()
        .filter(|box_| box_.score >= min_score)
        .collect()
}

/// 同一帧内：把高度重叠的框合并成一个大框（同一行字幕可能被切成多段）。
pub fn merge_overlapping(rects: &[MaskRect], iou_threshold: f64) -> Vec<MaskRect> {
    let mut merged: Vec<MaskRect> = Vec::new();
    for rect in rects.iter().copied().filter(|rect| !rect.is_empty()) {
        match merged
            .iter_mut()
            .find(|existing| iou(**existing, rect) >= iou_threshold)
        {
            Some(existing) => *existing = existing.union(&rect).unwrap_or(*existing),
            None => merged.push(rect),
        }
    }
    merged
}

/// 跨帧聚合的可信度参数。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reliability {
    /// 判定「同一位置」的 IoU 阈值。
    pub iou_threshold: f64,
    /// 至少出现在多少帧里才认为是真字幕（用于过滤偶发误检）。
    pub min_frames: usize,
}

impl Default for Reliability {
    fn default() -> Self {
        Self {
            iou_threshold: 0.5,
            min_frames: 2,
        }
    }
}

/// 跨帧聚合检测结果，返回**每一个**稳定区域（按面积从大到小）。
///
/// 与 [`stable_roi`] 的区别：短剧里常见「底部字幕 + 角落角标」两个互不相邻的
/// 标识，这里把它们作为独立区域返回，管线就能只在各自区域内生成掩码，
/// 而不会把两个区域之间的正常画面也划进掩码坐标空间。
///
/// 逐帧检测的偶发误检（例如画面里一闪而过的文字）会因出现帧数不足被丢弃。
pub fn stable_rois(samples: &[Vec<TextBox>], reliability: &Reliability) -> Vec<MaskRect> {
    if samples.is_empty() || reliability.min_frames == 0 {
        return Vec::new();
    }
    // (累计外接矩形, 出现的帧数)
    let mut clusters: Vec<(MaskRect, usize)> = Vec::new();
    for frame_boxes in samples {
        let rects: Vec<MaskRect> = frame_boxes.iter().map(TextBox::rect).collect();
        for rect in merge_overlapping(&rects, reliability.iou_threshold) {
            match clusters
                .iter_mut()
                .find(|(existing, _)| iou(*existing, rect) >= reliability.iou_threshold)
            {
                Some((existing, count)) => {
                    *existing = existing.union(&rect).unwrap_or(*existing);
                    *count += 1;
                }
                None => clusters.push((rect, 1)),
            }
        }
    }

    let stable: Vec<MaskRect> = clusters
        .into_iter()
        .filter(|(_, count)| *count >= reliability.min_frames)
        .map(|(rect, _)| rect)
        .collect();
    let mut merged = merge_overlapping(&stable, reliability.iou_threshold);
    merged.sort_by_key(|rect| std::cmp::Reverse(rect.area()));
    merged
}

/// 跨帧聚合检测结果，返回包含全部稳定区域的外接 ROI。
pub fn stable_roi(samples: &[Vec<TextBox>], reliability: &Reliability) -> Option<MaskRect> {
    union_rects(&stable_rois(samples, reliability))
}

/// 只保留中心点落在任一区域内的检测框。
///
/// 用于把逐帧检测限制在稳定的标识区域内：短剧画面里偶发的背景文字
/// （衣服印花、招牌等）会被这一步过滤掉，不进入掩码。
pub fn filter_by_regions(
    boxes: &[TextBox],
    regions: &[MaskRect],
    pad: u32,
    frame: FrameSize,
) -> Vec<TextBox> {
    if regions.is_empty() {
        return boxes.to_vec();
    }
    boxes
        .iter()
        .copied()
        .filter(|box_| {
            let center_x = box_.x + box_.width / 2;
            let center_y = box_.y + box_.height / 2;
            regions.iter().any(|region| {
                region.expand(pad, frame).is_some_and(|expanded| {
                    center_x >= expanded.x
                        && center_x < expanded.right()
                        && center_y >= expanded.y
                        && center_y < expanded.bottom()
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_rois_separates_subtitle_and_corner_logo() {
        let subtitle = MaskRect::new(120, 300, 400, 40);
        let logo = MaskRect::new(560, 30, 60, 40);
        let to_box = |rect: MaskRect| TextBox::new(rect.x, rect.y, rect.width, rect.height, 0.9);
        let samples: Vec<Vec<TextBox>> = (0..5)
            .map(|_| vec![to_box(subtitle), to_box(logo)])
            .collect();

        let regions = stable_rois(&samples, &Reliability::default());
        assert_eq!(regions.len(), 2, "两个字幕区域不应被并成一个：{regions:?}");
        assert_eq!(regions[0], subtitle, "面积大的区域应排在前面");
        assert_eq!(regions[1], logo);

        // 外接 ROI 仍是两者的并集，保证单次裁剪能覆盖全部区域
        assert_eq!(
            stable_roi(&samples, &Reliability::default()),
            Some(subtitle.union(&logo).unwrap())
        );
    }

    #[test]
    fn stable_rois_drops_transient_boxes() {
        let stable = MaskRect::new(0, 0, 100, 20);
        let transient = MaskRect::new(200, 100, 40, 15);
        let to_box = |rect: MaskRect| TextBox::new(rect.x, rect.y, rect.width, rect.height, 0.9);
        let mut samples = vec![vec![to_box(stable), to_box(transient)]];
        samples.extend((0..4).map(|_| vec![to_box(stable)]));

        let regions = stable_rois(&samples, &Reliability::default());
        assert_eq!(regions, vec![stable], "只出现一帧的框应被丢弃");
    }

    #[test]
    fn filter_by_regions_keeps_only_centers_inside() {
        let frame = FrameSize::new(640, 360);
        let regions = [MaskRect::new(120, 300, 400, 40)];
        let inside = TextBox::new(200, 310, 100, 20, 0.9);
        let outside = TextBox::new(200, 100, 100, 20, 0.9);
        let kept = filter_by_regions(&[inside, outside], &regions, 0, frame);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rect(), inside.rect());

        // 空区域列表不过滤
        assert_eq!(
            filter_by_regions(&[inside, outside], &[], 0, frame).len(),
            2
        );

        // 外扩量足够时，边界附近的框也会被保留
        let edge = TextBox::new(80, 300, 40, 20, 0.9);
        assert!(filter_by_regions(&[edge], &regions, 0, frame).is_empty());
        assert_eq!(filter_by_regions(&[edge], &regions, 20, frame).len(), 1);
    }

    #[test]
    fn detect_error_is_not_io_coupled() {
        // 保证错误类型可构造且信息可读
        let error = DetectError::Backend("worker 退出码 1".into());
        assert!(error.to_string().contains("worker 退出码 1"));
    }

    #[test]
    fn fixed_detector_clips_to_frame() {
        let detector = FixedRoiDetector::new(MaskRect::new(100, 300, 400, 40));
        let size = FrameSize::new(640, 360);
        let boxes = detector.detect(&[], size).unwrap();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].rect(), MaskRect::new(100, 300, 400, 40));

        // 完全在画面外时返回空，而不是报错
        let outside = FixedRoiDetector::new(MaskRect::new(700, 400, 10, 10));
        assert!(outside.detect(&[], size).unwrap().is_empty());
    }

    #[test]
    fn fixed_rois_detector_returns_each_region() {
        let size = FrameSize::new(640, 360);
        let subtitle = MaskRect::new(120, 300, 400, 40);
        let logo = MaskRect::new(560, 20, 60, 40);
        let detector =
            FixedRoisDetector::new(vec![subtitle, logo, MaskRect::new(700, 400, 10, 10)]);

        let boxes = detector.detect(&[], size).unwrap();
        assert_eq!(boxes.len(), 2, "完全在画面外的区域应被忽略");
        assert_eq!(boxes[0].rect(), subtitle);
        assert_eq!(boxes[1].rect(), logo);
        assert!(!detector.is_content_based());
    }

    #[test]
    fn offset_box_translates_and_clips() {
        let frame = FrameSize::new(100, 50);
        let cropped = TextBox::new(5, 3, 20, 10, 0.8);
        let moved = offset_box(&cropped, 40, 20, frame).unwrap();
        assert_eq!(moved.rect(), MaskRect::new(45, 23, 20, 10));
        assert_eq!(moved.score, 0.8);

        // 平移后越界：先裁到画面内，完全在画面外则丢弃
        let clipped = offset_box(&cropped, 90, 20, frame).unwrap();
        assert_eq!(clipped.rect(), MaskRect::new(95, 23, 5, 10));
        assert!(offset_box(&cropped, 200, 0, frame).is_none());
    }

    #[test]
    fn default_detectors_keep_full_frame_detection() {
        let detector = FixedRoisDetector::new(vec![MaskRect::new(0, 0, 10, 10)]);
        assert!(!detector.prefer_roi_detection());
        let worker = WorkerTextDetector::new(
            WorkerConfig {
                program: "true".into(),
                args: Vec::new(),
            },
            std::env::temp_dir(),
        );
        assert!(worker.prefer_roi_detection());
    }

    #[test]
    fn iou_and_merge_behave() {
        let a = MaskRect::new(0, 0, 10, 10);
        let b = MaskRect::new(0, 0, 10, 10);
        assert_eq!(iou(a, b), 1.0);
        assert_eq!(iou(a, MaskRect::new(100, 100, 10, 10)), 0.0);
        // 一半重叠
        assert!((iou(a, MaskRect::new(5, 0, 10, 10)) - (50.0 / 150.0)).abs() < 1e-9);

        // 互不重叠的框不会被合并，交给 stable_roi 的并集处理
        let merged = merge_overlapping(&[a, MaskRect::new(20, 0, 10, 10)], 0.1);
        assert_eq!(merged, vec![a, MaskRect::new(20, 0, 10, 10)]);

        // 同一行被切成两段且互相重叠时合并
        let merged_same_line = merge_overlapping(
            &[
                MaskRect::new(10, 300, 40, 20),
                MaskRect::new(40, 300, 40, 20),
            ],
            0.05,
        );
        assert_eq!(merged_same_line, vec![MaskRect::new(10, 300, 70, 20)]);

        // 相邻但无重叠（IoU 为 0）不合并，避免把无关两行糊在一起
        let adjacent = merge_overlapping(
            &[
                MaskRect::new(10, 300, 40, 20),
                MaskRect::new(60, 300, 40, 20),
            ],
            0.05,
        );
        assert_eq!(adjacent.len(), 2);
    }

    #[test]
    fn stable_roi_drops_one_off_false_positives() {
        let subtitle = TextBox::new(120, 300, 400, 40, 0.95);
        let false_positive = TextBox::new(400, 40, 120, 30, 0.80);
        let samples = vec![
            vec![subtitle],
            vec![subtitle, false_positive],
            vec![subtitle],
            vec![subtitle],
        ];

        let roi = stable_roi(&samples, &Reliability::default()).unwrap();
        assert_eq!(
            roi,
            MaskRect::new(120, 300, 400, 40),
            "只出现一帧的误检应被丢弃"
        );
    }

    #[test]
    fn stable_roi_merges_drifting_boxes() {
        let samples = vec![
            vec![TextBox::new(100, 300, 380, 40, 0.9)],
            vec![TextBox::new(104, 302, 380, 40, 0.9)],
            vec![TextBox::new(102, 301, 382, 40, 0.9)],
        ];
        let roi = stable_roi(&samples, &Reliability::default()).unwrap();
        // 轻微抖动会被并成更大的外接矩形，保证不漏擦
        assert!(roi.x <= 100 && roi.y <= 300);
        assert!(roi.right() >= 484 && roi.bottom() >= 342);
    }

    #[test]
    fn stable_roi_requires_min_frames_and_handles_empty_input() {
        let samples = vec![vec![TextBox::new(10, 10, 20, 10, 0.9)]];
        assert!(stable_roi(&samples, &Reliability::default()).is_none());
        assert!(stable_roi(&[], &Reliability::default()).is_none());

        let tolerant = Reliability {
            iou_threshold: 0.5,
            min_frames: 1,
        };
        assert_eq!(
            stable_roi(&samples, &tolerant),
            Some(MaskRect::new(10, 10, 20, 10))
        );
    }

    #[test]
    fn score_filter_keeps_confident_boxes() {
        let boxes = vec![
            TextBox::new(0, 0, 10, 10, 0.3),
            TextBox::new(0, 0, 10, 10, 0.9),
        ];
        assert_eq!(filter_by_score(boxes, 0.5).len(), 1);
    }

    #[test]
    fn worker_detector_parses_boxes_from_shell_stub() {
        let dir =
            std::env::temp_dir().join(format!("dossier-detect-worker-{}", std::process::id()));
        let stub = dir.join("stub.sh");
        std::fs::create_dir_all(&dir).unwrap();
        // 桩 worker：写出一份固定的 boxes.json
        std::fs::write(
            &stub,
            "#!/bin/sh\nset -e\nprintf '%s' '{\"boxes\":[{\"x\":10,\"y\":20,\"width\":30,\"height\":8,\"score\":0.9}]}' > \"$1/boxes.json\"\n",
        )
        .unwrap();

        let detector = WorkerTextDetector::new(
            WorkerConfig {
                program: PathBuf::from("sh"),
                args: vec![stub.display().to_string()],
            },
            dir.join("job"),
        );
        let size = FrameSize::new(4, 4);
        let frame = vec![0_u8; crate::frames::frame_bytes(size)];
        let boxes = detector.detect(&frame, size).unwrap();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].rect(), MaskRect::new(10, 20, 30, 8));
        assert!((boxes[0].score - 0.9).abs() < 1e-6);

        // 请求文件应写明几何信息与相对路径，便于 worker 解析原始帧
        let request: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("job/request.json")).unwrap()).unwrap();
        assert_eq!(request["mode"], "detect");
        assert_eq!(request["width"], 4);
        assert_eq!(request["frameFile"], "in/frame.raw");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_detector_rejects_short_frames_and_missing_output() {
        let dir =
            std::env::temp_dir().join(format!("dossier-detect-worker-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let noop = dir.join("noop.sh");
        std::fs::write(&noop, "#!/bin/sh\nexit 0\n").unwrap();

        let detector = WorkerTextDetector::new(
            WorkerConfig {
                program: PathBuf::from("sh"),
                args: vec![noop.display().to_string()],
            },
            dir.join("job"),
        );

        // 帧数据不足应直接报错，不必启动 worker
        let error = detector
            .detect(&[0_u8; 3], FrameSize::new(4, 4))
            .unwrap_err();
        assert!(matches!(error, DetectError::Output(_)));

        // worker 没有产出 boxes.json
        let size = FrameSize::new(2, 2);
        let frame = vec![0_u8; crate::frames::frame_bytes(size)];
        let error = detector.detect(&frame, size).unwrap_err();
        assert!(error.to_string().contains("缺少检测结果"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_detector_reports_backend_failure() {
        let dir =
            std::env::temp_dir().join(format!("dossier-detect-worker-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fail = dir.join("fail.sh");
        std::fs::write(&fail, "#!/bin/sh\necho 'DBNet 模型文件缺失' >&2\nexit 3\n").unwrap();

        let detector = WorkerTextDetector::new(
            WorkerConfig {
                program: PathBuf::from("sh"),
                args: vec![fail.display().to_string()],
            },
            dir.join("job"),
        );
        let size = FrameSize::new(2, 2);
        let frame = vec![0_u8; crate::frames::frame_bytes(size)];
        let error = detector.detect(&frame, size).unwrap_err();
        assert!(error.to_string().contains("DBNet 模型文件缺失"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
