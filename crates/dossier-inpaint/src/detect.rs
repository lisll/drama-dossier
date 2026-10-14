//! 字幕文本检测：结果类型、跨帧聚合与后端抽象。
//!
//! DBNet 本体放在 Python worker（`tools/model_worker`）里运行，Rust 侧只负责
//! 结果建模与「把逐帧检测收敛成稳定 ROI」的聚合逻辑。这样既可以用真实模型，
//! 也可以用固定 ROI 或桩实现做完全离线的单元测试。

use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mask::{FrameSize, MaskRect};

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
    fn detect(&self, _frame: &[u8], size: FrameSize) -> Result<Vec<TextBox>, DetectError> {
        Ok(match self.rect.clip(size) {
            Some(rect) if !rect.is_empty() => {
                vec![TextBox::new(rect.x, rect.y, rect.width, rect.height, 1.0)]
            }
            _ => Vec::new(),
        })
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
}

impl WorkerTextDetector {
    pub fn new(config: WorkerConfig, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            config,
            work_dir: work_dir.into(),
        }
    }
}

#[derive(Deserialize)]
struct BoxesFile {
    #[serde(default)]
    boxes: Vec<TextBox>,
}

impl TextDetector for WorkerTextDetector {
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

/// 跨帧聚合检测结果，返回稳定的字幕 ROI。
///
/// 逐帧检测的偶发误检（例如画面里一闪而过的文字）会因出现帧数不足被丢弃。
pub fn stable_roi(samples: &[Vec<TextBox>], reliability: &Reliability) -> Option<MaskRect> {
    if samples.is_empty() || reliability.min_frames == 0 {
        return None;
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
    union_rects(&stable)
}

#[cfg(test)]
mod tests {
    use super::*;

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
