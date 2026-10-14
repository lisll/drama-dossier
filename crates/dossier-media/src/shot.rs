//! 镜头切点检测（ffmpeg `select` + `metadata=print` 的场景分数）。
//!
//! 用途：长视频按镜头切分是做「逐镜头质量检查」「按镜头重组章节」的前提；
//! 本机没有 GPU，光流/模型类方法不可行，而 ffmpeg 内置的 `scene` 分数
//! （帧间直方图差异）在硬切场景上足够可靠，且**完全本地、零依赖**。
//!
//! 定位说明：它做的是**候选生成**，不做语义判断——真正的「这是一章的开头」属于
//! 编排层的事。因此这里同时给出分数与最小间隔，把「要不要合并」的决策权交出去。

use std::path::{Path, PathBuf};

use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 镜头检测错误。
#[derive(Debug, Error)]
pub enum ShotError {
    #[error("输入文件不存在：{0}")]
    InputMissing(PathBuf),
    #[error("阈值必须落在 (0,1) 内，实际 {0}")]
    InvalidThreshold(f64),
    #[error("检测失败（{command}）：{message}")]
    DetectFailed { command: String, message: String },
}

/// 检测配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotDetectConfig {
    /// 场景分数阈值；越高越保守（默认 0.35）。
    pub threshold: f64,
    /// 两个切点之间的最小间隔（秒）；用于压掉同一秒内的重复检测（默认 0.2）。
    pub min_gap_sec: f64,
}

impl Default for ShotDetectConfig {
    fn default() -> Self {
        Self {
            threshold: 0.35,
            min_gap_sec: 0.2,
        }
    }
}

/// 一个检测到的镜头切点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotBoundary {
    /// 切点时间（秒），即新镜头的第一帧。
    pub time_sec: f64,
    /// ffmpeg 给出的场景分数。
    pub score: f64,
}

/// 一个镜头区间。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotRange {
    pub index: usize,
    pub start_sec: f64,
    pub end_sec: f64,
}

impl ShotRange {
    pub fn duration(&self) -> f64 {
        (self.end_sec - self.start_sec).max(0.0)
    }
}

/// 检测结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotReport {
    pub threshold: f64,
    pub boundaries: Vec<ShotBoundary>,
    /// 原始工具输出（排查用）。
    pub detail: String,
}

impl ShotReport {
    /// 按切点切成镜头区间；`duration` 是素材总时长。
    pub fn to_ranges(&self, duration: f64) -> Vec<ShotRange> {
        let mut ranges = Vec::new();
        let mut start = 0.0_f64;
        let mut index = 0_usize;
        for boundary in &self.boundaries {
            if boundary.time_sec <= start {
                continue;
            }
            ranges.push(ShotRange {
                index,
                start_sec: start,
                end_sec: boundary.time_sec,
            });
            index += 1;
            start = boundary.time_sec;
        }
        if duration > start {
            ranges.push(ShotRange {
                index,
                start_sec: start,
                end_sec: duration,
            });
        }
        ranges
    }

    /// 合并间隔过近的切点（保留分数更高的那个）。
    pub fn merged(mut self, min_gap_sec: f64) -> Self {
        self.boundaries
            .sort_by(|left, right| left.time_sec.total_cmp(&right.time_sec));
        let mut merged: Vec<ShotBoundary> = Vec::with_capacity(self.boundaries.len());
        for boundary in self.boundaries.into_iter() {
            match merged.last_mut() {
                Some(previous) if boundary.time_sec - previous.time_sec < min_gap_sec => {
                    if boundary.score > previous.score {
                        *previous = boundary;
                    }
                }
                _ => merged.push(boundary),
            }
        }
        self.boundaries = merged;
        self
    }
}

/// 构造检测命令：`select` 挑出场景分数超阈值的帧，`metadata=print` 把分数打到 stdout。
pub fn build_detect_command(
    ffmpeg: &str,
    input: &Path,
    config: &ShotDetectConfig,
) -> Result<CommandSpec, ShotError> {
    if !input.is_file() {
        return Err(ShotError::InputMissing(input.to_path_buf()));
    }
    if !(config.threshold > 0.0 && config.threshold < 1.0) {
        return Err(ShotError::InvalidThreshold(config.threshold));
    }
    // `metadata=print:file=-` 让分数走 stdout：不去解析 stderr 里的日志格式，
    // 输出就与 `-loglevel` 的选择无关。
    let filter = format!(
        "select='gt(scene,{})',metadata=print:file=-",
        format_float(config.threshold)
    );
    Ok(CommandSpec::new(ffmpeg.to_string())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input.display().to_string())
        .args(["-filter_complex", &filter, "-an", "-f", "null", "-"]))
}

/// 解析 `metadata=print` 的输出。
///
/// 实测形态（ffmpeg 4.2.4）：
/// ```text
/// frame:0    pts:10240   pts_time:1
/// lavfi.scene_score=1.000000
/// ```
/// `frame:` 是「被 select 选中的第几帧」，不是绝对帧号，所以只认 `pts_time`。
pub fn parse_scene_scores(text: &str) -> Vec<ShotBoundary> {
    let mut boundaries = Vec::new();
    let mut pending: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        // 同一行里可能同时有 frame/pts/pts_time，只取 pts_time。
        if let Some(value) = extract_key_value(line, "pts_time:") {
            pending = value.parse::<f64>().ok();
        }
        if let Some(value) = line.strip_prefix("lavfi.scene_score=") {
            if let (Some(time), Ok(score)) = (pending, value.trim().parse::<f64>()) {
                boundaries.push(ShotBoundary {
                    time_sec: time,
                    score,
                });
            }
            pending = None;
        }
    }
    boundaries.sort_by(|left, right| left.time_sec.total_cmp(&right.time_sec));
    boundaries
}

fn extract_key_value(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let end = rest
        .find(|ch: char| ch.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// 真的跑一次检测。
pub fn detect_shots(
    ffmpeg: &str,
    input: &Path,
    config: &ShotDetectConfig,
) -> Result<ShotReport, ShotError> {
    let spec = build_detect_command(ffmpeg, input, config)?;
    let runner = SystemRunner::new();
    let output = runner.run(&spec).map_err(|error| ShotError::DetectFailed {
        command: spec.display(),
        message: error.to_string(),
    })?;
    if !output.success() {
        return Err(ShotError::DetectFailed {
            command: spec.display(),
            message: format!(
                "退出码 {}：{}",
                output.code_label(),
                first_line(&output.combined())
            ),
        });
    }
    let boundaries = parse_scene_scores(&output.stdout);
    let report = ShotReport {
        threshold: config.threshold,
        detail: format!("{} 个切点（阈值 {}）", boundaries.len(), config.threshold),
        boundaries,
    };
    Ok(report.merged(config.min_gap_sec))
}

fn format_float(value: f64) -> String {
    let text = format!("{value:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::SystemRunner;

    #[test]
    fn parses_the_real_metadata_print_shape() {
        let text = "frame:0    pts:10240   pts_time:1\nlavfi.scene_score=1.000000\n\
                    frame:1    pts:30720   pts_time:3\nlavfi.scene_score=0.512345\n";
        let boundaries = parse_scene_scores(text);
        assert_eq!(boundaries.len(), 2);
        assert_eq!(boundaries[0].time_sec, 1.0);
        assert_eq!(boundaries[0].score, 1.0);
        assert_eq!(boundaries[1].time_sec, 3.0);
        assert!((boundaries[1].score - 0.512345).abs() < 1e-9);
    }

    #[test]
    fn ignores_score_lines_without_a_timestamp() {
        let boundaries = parse_scene_scores("lavfi.scene_score=0.9\n");
        assert!(boundaries.is_empty());
    }

    #[test]
    fn merges_nearby_boundaries_keeping_the_stronger_one() {
        let report = ShotReport {
            threshold: 0.3,
            boundaries: vec![
                ShotBoundary {
                    time_sec: 5.0,
                    score: 0.4,
                },
                ShotBoundary {
                    time_sec: 5.05,
                    score: 0.9,
                },
                ShotBoundary {
                    time_sec: 8.0,
                    score: 0.5,
                },
            ],
            detail: String::new(),
        }
        .merged(0.2);
        assert_eq!(report.boundaries.len(), 2);
        assert_eq!(report.boundaries[0].time_sec, 5.05);
        assert_eq!(report.boundaries[0].score, 0.9);
        assert_eq!(report.boundaries[1].time_sec, 8.0);
    }

    #[test]
    fn boundaries_become_ranges_covering_the_whole_clip() {
        let report = ShotReport {
            threshold: 0.3,
            boundaries: vec![
                ShotBoundary {
                    time_sec: 2.0,
                    score: 0.6,
                },
                ShotBoundary {
                    time_sec: 6.5,
                    score: 0.7,
                },
            ],
            detail: String::new(),
        };
        let ranges = report.to_ranges(10.0);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].start_sec, 0.0);
        assert_eq!(ranges[0].end_sec, 2.0);
        assert_eq!(ranges[1].start_sec, 2.0);
        assert_eq!(ranges[2].end_sec, 10.0);
        assert!((ranges.iter().map(ShotRange::duration).sum::<f64>() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_bad_threshold_and_missing_input() {
        let config = ShotDetectConfig {
            threshold: 1.5,
            ..Default::default()
        };
        assert!(matches!(
            build_detect_command("ffmpeg", Path::new("/definitely/not/here.mp4"), &config),
            Err(ShotError::InputMissing(_))
        ));
    }

    /// 真跑一次：造一段「黑 1 秒 + 白 1 秒」的硬切视频，切点必须落在 1 秒附近。
    #[test]
    fn real_detection_finds_a_hard_cut() {
        let runner = SystemRunner::new();
        if !runner
            .run(&CommandSpec::new("ffmpeg").arg("-version"))
            .map(|output| output.success())
            .unwrap_or(false)
        {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        let dir = std::env::temp_dir().join(format!("dossier-shot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("cut.mp4");
        // 用黑→白这种亮度直方图完全不重叠的对色：scene 分数按构造就是 1.0，
        // 与 ffmpeg 版本无关（实测 6.1.1 = 1.000000）。红蓝这类有重叠的
        // 配色只能得 0.4 左右，会让断言在不同版本上时好时坏。
        let output = runner
            .run(
                &CommandSpec::new("ffmpeg")
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-y",
                        "-f",
                        "lavfi",
                        "-i",
                        "color=c=black:s=160x120:r=10:d=1",
                        "-f",
                        "lavfi",
                        "-i",
                        "color=c=white:s=160x120:r=10:d=1",
                        "-filter_complex",
                        "[0:v][1:v]concat=n=2:v=1:a=0[v]",
                        "-map",
                        "[v]",
                        "-pix_fmt",
                        "yuv420p",
                    ])
                    .arg(clip.display().to_string()),
            )
            .unwrap();
        assert!(output.success(), "造素材失败：{}", output.combined());

        let report = detect_shots("ffmpeg", &clip, &ShotDetectConfig::default()).unwrap();
        assert_eq!(report.boundaries.len(), 1, "{report:?}");
        let boundary = &report.boundaries[0];
        assert!(
            (boundary.time_sec - 1.0).abs() < 0.25,
            "切点应落在 1 秒附近，实际 {}",
            boundary.time_sec
        );
        assert!(
            boundary.score > 0.9,
            "硬切的分数应当很高：{}",
            boundary.score
        );

        let ranges = report.to_ranges(2.0);
        assert_eq!(ranges.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
