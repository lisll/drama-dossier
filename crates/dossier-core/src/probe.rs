//! ffprobe 探测：调用外部 ffprobe 获取分集规格。
//!
//! 设计：不捆绑 ffmpeg/ffprobe 二进制；路径可配置，缺省从 PATH 查找。
//! 解析逻辑与进程调用分离，便于用 JSON 固定样本做单元测试。

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use thiserror::Error;

use crate::types::{EpisodeFile, EpisodeSpec, ProbedEpisode};

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("ffprobe 未找到：{0}。请在设置中配置 ffprobe 路径或安装 FFmpeg。")]
    FfprobeMissing(String),
    #[error("ffprobe 调用失败（{path}）：{source}")]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("ffprobe 返回非零（{path}）：{stderr}")]
    NonZero { path: PathBuf, stderr: String },
    #[error("ffprobe 输出解析失败：{0}")]
    Parse(#[from] serde_json::Error),
}

/// ffprobe 调用配置。
#[derive(Debug, Clone, Default)]
pub struct FfprobeConfig {
    /// ffprobe 可执行文件路径；None 表示使用 PATH 中的 `ffprobe`。
    pub bin: Option<PathBuf>,
}

/// 探测单个分集。
pub fn probe_episode(file: &EpisodeFile, cfg: &FfprobeConfig) -> Result<ProbedEpisode, ProbeError> {
    let bin = cfg.bin.clone().unwrap_or_else(|| PathBuf::from("ffprobe"));
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
    let output = command.arg(&file.path).output().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ProbeError::FfprobeMissing(bin.display().to_string())
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
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    let spec = parse_ffprobe_json(&output.stdout)?;
    Ok(ProbedEpisode {
        file: file.clone(),
        spec,
    })
}

/// 探测一组分集；单个失败不影响其余，结果带回错误信息。
pub fn probe_episodes(
    files: &[EpisodeFile],
    cfg: &FfprobeConfig,
) -> Vec<(EpisodeFile, Result<EpisodeSpec, String>)> {
    files
        .iter()
        .map(|f| {
            let r = probe_episode(f, cfg).map_err(|e| e.to_string());
            let spec = r.map(|p| p.spec);
            (f.clone(), spec)
        })
        .collect()
}

/// 解析 ffprobe `-show_format -show_streams` 的 JSON 输出。
pub fn parse_ffprobe_json(raw: &[u8]) -> Result<EpisodeSpec, serde_json::Error> {
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
        #[serde(default)]
        bit_rate: Option<String>,
    }
    #[derive(Deserialize)]
    struct Stream {
        codec_type: Option<String>,
        codec_name: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        #[serde(default)]
        avg_frame_rate: Option<String>,
        #[serde(default)]
        r_frame_rate: Option<String>,
        #[serde(default)]
        bit_rate: Option<String>,
        #[serde(default)]
        disposition: Option<Disposition>,
    }
    #[derive(Deserialize)]
    struct Disposition {
        #[serde(default)]
        attached_pic: u8,
    }

    let root: Root = serde_json::from_slice(raw)?;
    let mut spec = EpisodeSpec::default();
    if let Some(f) = &root.format {
        spec.duration_sec = f
            .duration
            .as_deref()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0);
        spec.bitrate_bps = f.bit_rate.as_deref().and_then(|s| s.parse().ok());
    }
    let mut video_seen = false;
    for st in &root.streams {
        let bit_rate = st.bit_rate.as_deref().and_then(|s| s.parse::<u64>().ok());
        match st.codec_type.as_deref() {
            // 只取第一条真正的视频流：mp4/m4v 的封面图（attached_pic）在 ffprobe 里也是 video 流，
            // 排在后面时会把宽高、编码、码率全换成那张图的。
            Some("video")
                if !video_seen && st.disposition.as_ref().is_none_or(|d| d.attached_pic == 0) =>
            {
                video_seen = true;
                spec.width = st.width.unwrap_or(0);
                spec.height = st.height.unwrap_or(0);
                spec.video_codec = st.codec_name.clone();
                spec.video_bitrate_bps = bit_rate;
                // 两个字段分别解析、取第一个有效值：`avg_frame_rate` 为 "0/0" 时它仍是 Some，
                // 写成 `.or(r_frame_rate)` 永远不会回退，结果按 30 fps 处理、微帧位移算偏。
                spec.fps = st
                    .avg_frame_rate
                    .as_deref()
                    .and_then(parse_frame_rate)
                    .or_else(|| st.r_frame_rate.as_deref().and_then(parse_frame_rate));
            }
            Some("audio") => {
                // 编码名取第一条（主音轨）；码率累加全部音轨：估算视频码率时要把音频整体扣掉，
                // 只扣第一条会把多音轨文件的视频码率估高。
                if spec.audio_codec.is_none() {
                    spec.audio_codec = st.codec_name.clone();
                }
                if let Some(bps) = bit_rate {
                    spec.audio_bitrate_bps = Some(spec.audio_bitrate_bps.unwrap_or(0) + bps);
                }
            }
            _ => {}
        }
    }
    Ok(spec)
}

fn parse_frame_rate(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let n: f64 = num.parse().ok()?;
    let d: f64 = den.parse().ok()?;
    if d == 0.0 {
        None
    } else {
        Some(n / d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn frame_rate_falls_back_when_avg_is_zero_over_zero() {
        let raw = br#"{"format":{"duration":"10"},"streams":[
            {"codec_type":"video","codec_name":"h264","width":720,"height":1280,
             "avg_frame_rate":"0/0","r_frame_rate":"25/1","bit_rate":"3000000"},
            {"codec_type":"audio","codec_name":"aac","bit_rate":"128000"},
            {"codec_type":"audio","codec_name":"ac3","bit_rate":"384000"}]}"#;
        let spec = parse_ffprobe_json(raw).unwrap();
        assert_eq!(
            spec.fps,
            Some(25.0),
            "avg_frame_rate 为 0/0 时应改读 r_frame_rate"
        );
        assert_eq!(spec.video_bitrate_bps, Some(3_000_000));
        // 多音轨时编码名记第一条（主音轨），码率累加全部音轨
        assert_eq!(spec.audio_codec.as_deref(), Some("aac"));
        assert_eq!(
            spec.audio_bitrate_bps,
            Some(512_000),
            "音频码率累加全部音轨"
        );
    }

    #[test]
    fn cover_art_stream_does_not_override_main_video() {
        let raw = br#"{"format":{"duration":"10","bit_rate":"3200000"},"streams":[
            {"codec_type":"video","codec_name":"h264","width":1080,"height":1920,
             "avg_frame_rate":"30/1","bit_rate":"3000000","disposition":{"attached_pic":0}},
            {"codec_type":"audio","codec_name":"aac","bit_rate":"128000"},
            {"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,
             "avg_frame_rate":"0/0","disposition":{"attached_pic":1}}]}"#;
        let spec = parse_ffprobe_json(raw).unwrap();
        assert_eq!((spec.width, spec.height), (1080, 1920));
        assert_eq!(spec.video_codec.as_deref(), Some("h264"));
        assert_eq!(spec.video_bitrate_bps, Some(3_000_000));
        assert_eq!(spec.fps, Some(30.0));
    }

    #[test]
    fn parses_ffprobe_fixture() {
        let json = br#"{
          "streams": [
            {"codec_type":"video","codec_name":"h264","width":1080,"height":1920,
             "avg_frame_rate":"30/1"},
            {"codec_type":"audio","codec_name":"aac"}
          ],
          "format":{"duration":"63.5","bit_rate":"7250000"}
        }"#;
        let spec = parse_ffprobe_json(json).unwrap();
        assert_eq!(spec.width, 1080);
        assert_eq!(spec.height, 1920);
        assert_eq!(spec.duration_sec, 63.5);
        assert_eq!(spec.bitrate_bps, Some(7_250_000));
        assert_eq!(spec.fps, Some(30.0));
        assert_eq!(spec.video_codec.as_deref(), Some("h264"));
        assert_eq!(spec.audio_codec.as_deref(), Some("aac"));
    }

    #[test]
    fn missing_ffprobe_is_actionable() {
        let cfg = FfprobeConfig {
            bin: Some(Path::new("/nonexistent/ffprobe").to_path_buf()),
        };
        let f = EpisodeFile {
            path: PathBuf::from("/tmp/x.mp4"),
            file_name: "x.mp4".into(),
            size_bytes: 1,
            episode_number: Some(1),
        };
        match probe_episode(&f, &cfg) {
            Err(ProbeError::FfprobeMissing(_)) => {}
            other => panic!("expected FfprobeMissing, got {:?}", other.map(|_| ())),
        }
    }
}
