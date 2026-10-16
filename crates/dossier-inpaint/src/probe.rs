//! 用外部 `ffprobe` 读取视频几何信息。
//!
//! 与项目既有策略一致：不捆绑 FFmpeg，只调用用户环境里的 `ffprobe`。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mask::FrameSize;

/// 视频基础信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoInfo {
    pub size: FrameSize,
    /// 平均帧率；`ffprobe` 给不出有效值时（如 `0/0`）为 `None`。
    pub fps: Option<f64>,
    pub duration_sec: f64,
    pub has_audio: bool,
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("ffprobe 未找到：{0}。请安装 FFmpeg 或用 --ffprobe 指定路径。")]
    Missing(String),
    #[error("ffprobe 调用失败（{path}）：{source}")]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("ffprobe 返回非零（{path}）：{stderr}")]
    NonZero { path: PathBuf, stderr: String },
    #[error("ffprobe 输出解析失败：{0}")]
    Parse(String),
    #[error("未在素材中找到视频流：{0}")]
    NoVideoStream(String),
}

/// 调用 `ffprobe` 读取视频信息。
pub fn probe_video(ffprobe_bin: Option<&Path>, input: &Path) -> Result<VideoInfo, ProbeError> {
    let bin = ffprobe_bin
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("ffprobe"));
    let mut command = Command::new(&bin);
    command.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]);
    // Windows 的 GUI 进程拉起 ffprobe 会弹黑色 cmd 窗口，CREATE_NO_WINDOW 静默运行。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.arg(input).output().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ProbeError::Missing(bin.display().to_string())
        } else {
            ProbeError::Spawn {
                path: bin.clone(),
                source,
            }
        }
    })?;
    if !output.status.success() {
        return Err(ProbeError::NonZero {
            path: bin,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    parse_probe_json(&output.stdout, &input.display().to_string())
}

/// 解析 `ffprobe -show_format -show_streams` 的 JSON 输出。
pub fn parse_probe_json(raw: &[u8], source: &str) -> Result<VideoInfo, ProbeError> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        format: Option<Format>,
        #[serde(default)]
        streams: Vec<Stream>,
    }
    #[derive(Deserialize)]
    struct Format {
        #[serde(default)]
        duration: Option<String>,
    }
    #[derive(Deserialize)]
    struct Stream {
        #[serde(default)]
        codec_type: Option<String>,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
        #[serde(default)]
        avg_frame_rate: Option<String>,
        #[serde(default)]
        r_frame_rate: Option<String>,
    }

    let root: Root =
        serde_json::from_slice(raw).map_err(|error| ProbeError::Parse(error.to_string()))?;

    let video = root
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| ProbeError::NoVideoStream(source.to_string()))?;
    let width = video
        .width
        .ok_or_else(|| ProbeError::Parse(format!("{source} 的视频流缺少 width")))?;
    let height = video
        .height
        .ok_or_else(|| ProbeError::Parse(format!("{source} 的视频流缺少 height")))?;

    let fps = video
        .avg_frame_rate
        .as_deref()
        .and_then(parse_frame_rate)
        .or_else(|| video.r_frame_rate.as_deref().and_then(parse_frame_rate));

    Ok(VideoInfo {
        size: FrameSize::new(width, height),
        fps,
        duration_sec: root
            .format
            .as_ref()
            .and_then(|format| format.duration.as_deref())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0),
        has_audio: root
            .streams
            .iter()
            .any(|stream| stream.codec_type.as_deref() == Some("audio")),
    })
}

/// 解析 `"30/1"` 形式的帧率；`0/0` 等无效值返回 `None`。
fn parse_frame_rate(value: &str) -> Option<f64> {
    let (numerator, denominator) = value.split_once('/')?;
    let numerator: f64 = numerator.parse().ok()?;
    let denominator: f64 = denominator.parse().ok()?;
    if denominator == 0.0 || numerator <= 0.0 {
        None
    } else {
        Some(numerator / denominator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = br#"{
      "streams": [
        {"codec_type":"video","codec_name":"h264","width":640,"height":360,
         "avg_frame_rate":"30/1","r_frame_rate":"30/1"},
        {"codec_type":"audio","codec_name":"aac"}
      ],
      "format": {"duration": "2.033333"}
    }"#;

    #[test]
    fn parses_video_info() {
        let info = parse_probe_json(FIXTURE, "sub.mp4").unwrap();
        assert_eq!(info.size, FrameSize::new(640, 360));
        assert_eq!(info.fps, Some(30.0));
        assert!(info.has_audio);
        assert!((info.duration_sec - 2.033333).abs() < 1e-6);
    }

    #[test]
    fn falls_back_to_r_frame_rate() {
        let raw = br#"{"streams":[{"codec_type":"video","width":8,"height":8,
          "avg_frame_rate":"0/0","r_frame_rate":"24000/1001"}],"format":{"duration":"1.0"}}"#;
        let info = parse_probe_json(raw, "vfr.mp4").unwrap();
        assert!((info.fps.unwrap() - 23.976023976).abs() < 1e-6);
    }

    #[test]
    fn reports_missing_video_stream_and_bad_fps() {
        let audio_only = br#"{"streams":[{"codec_type":"audio"}],"format":{"duration":"1.0"}}"#;
        assert!(matches!(
            parse_probe_json(audio_only, "a.m4a"),
            Err(ProbeError::NoVideoStream(_))
        ));

        let no_fps = br#"{"streams":[{"codec_type":"video","width":4,"height":4,
          "avg_frame_rate":"0/0"}],"format":{}}"#;
        let info = parse_probe_json(no_fps, "x.mp4").unwrap();
        assert_eq!(info.fps, None);
        assert_eq!(info.duration_sec, 0.0);
        assert!(!info.has_audio);

        assert!(matches!(
            parse_probe_json(b"{ not json", "x.mp4"),
            Err(ProbeError::Parse(_))
        ));
    }
}
