//! SRT 字幕读写。
//!
//! ASR 与 OCR 都要吐出字幕，所以放一个公共实现，避免两条路径各写一套时间轴格式化
//! （那正是「同一个项目里两个模块对同一份数据有两种解释」的常见源头）。

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 字幕读写错误。
#[derive(Debug, Error)]
pub enum SubtitleError {
    #[error("时间戳非法：{0}")]
    InvalidTime(f64),
    #[error("字幕文本为空")]
    EmptyText,
    #[error("字幕文件读写失败（{path}）：{source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

/// 一条字幕。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleCue {
    pub start_sec: f64,
    pub end_sec: f64,
    /// 多行文本（不含时间轴）。
    pub text: String,
}

impl SubtitleCue {
    pub fn new(start_sec: f64, end_sec: f64, text: impl Into<String>) -> Self {
        Self {
            start_sec,
            end_sec,
            text: text.into(),
        }
    }

    pub fn duration(&self) -> f64 {
        (self.end_sec - self.start_sec).max(0.0)
    }

    /// 归一化：负数归零、结束早于开始时补齐、去掉首尾空白。
    pub fn normalized(&self) -> Result<Self, SubtitleError> {
        let start = if self.start_sec.is_finite() {
            self.start_sec.max(0.0)
        } else {
            return Err(SubtitleError::InvalidTime(self.start_sec));
        };
        let end = if self.end_sec.is_finite() {
            self.end_sec.max(0.0)
        } else {
            return Err(SubtitleError::InvalidTime(self.end_sec));
        };
        let text = self.text.trim().to_string();
        if text.is_empty() {
            return Err(SubtitleError::EmptyText);
        }
        // 结束时间不晚于开始时间时，给一个最小显示时长（0.2 秒），而不是生成一条
        // 时长为零、播放器可能直接不显示的字幕。
        let end = if end <= start { start + 0.2 } else { end };
        Ok(Self {
            start_sec: start,
            end_sec: end,
            text,
        })
    }
}

/// 把秒格式化为 `HH:MM:SS,mmm`。
pub fn format_timestamp(seconds: f64) -> String {
    let clamped = seconds.max(0.0);
    let total_ms = (clamped * 1000.0).round() as u64;
    let millis = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    format!("{hours:02}:{mins:02}:{secs:02},{millis:03}")
}

/// 解析 `HH:MM:SS,mmm` / `HH:MM:SS.mmm` / `MM:SS`。
pub fn parse_timestamp(text: &str) -> Option<f64> {
    let text = text.trim();
    let (clock, millis) = match text.rsplit_once([',', '.']) {
        Some((clock, fraction)) => {
            let digits: String = fraction.chars().filter(char::is_ascii_digit).collect();
            let value: u32 = digits.parse().ok()?;
            let scale = 10_u32.pow(digits.len() as u32);
            (clock, (value as f64 / scale as f64) * 1000.0)
        }
        None => (text, 0.0),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut seconds = 0.0_f64;
    for (index, part) in parts.iter().enumerate() {
        let value: f64 = part.trim().parse().ok()?;
        // 依次是 时:分:秒，从右往左乘 60 的幂。
        let power = parts.len() - 1 - index;
        seconds += value * 60_f64.powi(power as i32);
    }
    Some(seconds + millis / 1000.0)
}

/// 渲染成 SRT 文本（序号从 1 开始，时间轴按起始时间稳定排序）。
pub fn to_srt(cues: &[SubtitleCue]) -> Result<String, SubtitleError> {
    let mut normalized: Vec<SubtitleCue> = Vec::with_capacity(cues.len());
    for cue in cues {
        normalized.push(cue.normalized()?);
    }
    // 稳定排序：起始时间相同时保持输入顺序，避免同一份输入两次输出不一样。
    normalized.sort_by(|left, right| left.start_sec.total_cmp(&right.start_sec));

    let mut out = String::new();
    for (index, cue) in normalized.iter().enumerate() {
        out.push_str(&(index + 1).to_string());
        out.push('\n');
        out.push_str(&format!(
            "{} --> {}\n",
            format_timestamp(cue.start_sec),
            format_timestamp(cue.end_sec)
        ));
        out.push_str(&cue.text);
        out.push_str("\n\n");
    }
    Ok(out)
}

/// 解析 SRT 文本。空块与无时间轴的块会被跳过（真实工具的产物里经常有多余空行）。
pub fn parse_srt(text: &str) -> Result<Vec<SubtitleCue>, SubtitleError> {
    let mut cues = Vec::new();
    // 先按「空行」切块；CRLF 也要能处理。
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for block in normalized.split("\n\n") {
        let block = block.trim_matches('\n');
        if block.trim().is_empty() {
            continue;
        }
        let mut lines = block.lines().filter(|line| !line.trim().is_empty());
        let Some(first) = lines.next() else { continue };
        // 首行可能是序号，也可能是时间轴（有些工具不写序号）。
        let timeline = if first.contains("-->") {
            first
        } else {
            match lines.next() {
                Some(line) => line,
                None => continue,
            }
        };
        let Some((start, end)) = timeline.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_timestamp(start), parse_timestamp(end)) else {
            continue;
        };
        let body: Vec<&str> = lines.collect();
        let text = body.join("\n");
        if text.trim().is_empty() {
            continue;
        }
        cues.push(SubtitleCue::new(start, end, text));
    }
    Ok(cues)
}

/// 读取并解析 SRT 文件。
pub fn load_srt(path: &Path) -> Result<Vec<SubtitleCue>, SubtitleError> {
    let raw = std::fs::read_to_string(path).map_err(|source| SubtitleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_srt(&raw)
}

/// 写出 SRT 文件。
pub fn save_srt(path: &Path, cues: &[SubtitleCue]) -> Result<(), SubtitleError> {
    let text = to_srt(cues)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| SubtitleError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    std::fs::write(path, text).map_err(|source| SubtitleError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_roundtrip_is_stable() {
        for (value, text) in [
            (0.0, "00:00:00,000"),
            (1.5, "00:00:01,500"),
            (61.25, "00:01:01,250"),
            (3661.007, "01:01:01,007"),
        ] {
            assert_eq!(format_timestamp(value), text);
            let parsed = parse_timestamp(text).unwrap();
            assert!((parsed - value).abs() < 0.001, "{text} → {parsed}");
        }
    }

    #[test]
    fn parses_timestamps_of_varied_shapes() {
        assert_eq!(parse_timestamp("00:01:02,500"), Some(62.5));
        assert_eq!(parse_timestamp("00:01:02.500"), Some(62.5));
        assert_eq!(parse_timestamp("01:02"), Some(62.0));
        assert_eq!(parse_timestamp(" 00:00:03,000 "), Some(3.0));
        assert_eq!(parse_timestamp("garbage"), None);
    }

    #[test]
    fn srt_roundtrip_preserves_cues() {
        let cues = vec![
            SubtitleCue::new(3.0, 5.5, "第二句\n换行"),
            SubtitleCue::new(1.0, 2.5, "第一句"),
        ];
        let text = to_srt(&cues).unwrap();
        // 按起始时间排序后再编号。
        assert!(text.starts_with("1\n00:00:01,000 --> 00:00:02,500\n第一句\n"));
        assert!(text.contains("2\n00:00:03,000 --> 00:00:05,500\n第二句\n换行\n"));
        let parsed = parse_srt(&text).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].text, "第一句");
        assert_eq!(parsed[1].text, "第二句\n换行");
        assert_eq!(parsed[1].start_sec, 3.0);
    }

    #[test]
    fn parse_tolerates_missing_index_and_blank_lines() {
        let raw = "00:00:01,000 --> 00:00:02,000\n没有序号\n\n\n\n2\n00:00:03,000 --> 00:00:04,000\n有序号\n\n";
        let cues = parse_srt(raw).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "没有序号");
        assert_eq!(cues[1].text, "有序号");
    }

    #[test]
    fn normalization_rejects_empty_and_repairs_reversed_ranges() {
        assert!(matches!(
            SubtitleCue::new(0.0, 1.0, "   ").normalized().unwrap_err(),
            SubtitleError::EmptyText
        ));
        let repaired = SubtitleCue::new(5.0, 4.0, "倒过来的区间")
            .normalized()
            .unwrap();
        assert!(repaired.end_sec > repaired.start_sec);
        assert!((repaired.duration() - 0.2).abs() < 1e-9);
        assert!(matches!(
            SubtitleCue::new(f64::NAN, 1.0, "x")
                .normalized()
                .unwrap_err(),
            SubtitleError::InvalidTime(_)
        ));
    }
}
