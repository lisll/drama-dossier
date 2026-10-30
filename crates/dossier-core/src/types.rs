use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一部剧（剧集库根目录下的一个子目录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drama {
    pub name: String,
    pub dir: PathBuf,
    pub episodes: Vec<EpisodeFile>,
    pub cover: Option<PathBuf>,
    pub synopsis: Option<PathBuf>,
}

/// 一个分集视频文件（尚未探测规格）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeFile {
    pub path: PathBuf,
    pub file_name: String,
    pub size_bytes: u64,
    /// 从文件名解析出的集号；解析失败为 None。
    pub episode_number: Option<u32>,
}

/// ffprobe 探测出的分集规格。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeSpec {
    pub duration_sec: f64,
    pub width: u32,
    pub height: u32,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub fps: Option<f64>,
    /// 整个文件的码率（`format.bit_rate`，含音频）。
    pub bitrate_bps: Option<u64>,
    /// 视频轨码率（stream `bit_rate`）；部分封装（如 mkv/webm）不写，读不到为 None。
    #[serde(default)]
    pub video_bitrate_bps: Option<u64>,
    /// 音频码率（全部音轨 stream `bit_rate` 之和；都读不到为 None）。
    #[serde(default)]
    pub audio_bitrate_bps: Option<u64>,
}

/// 探测完成的分集 = 文件信息 + 规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbedEpisode {
    #[serde(flatten)]
    pub file: EpisodeFile,
    pub spec: EpisodeSpec,
}

/// 规格探测失败的分集：文件信息 + 原因。调用方据此拒绝执行或在结果里标明，不能静默跳过。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeFailure {
    #[serde(flatten)]
    pub file: EpisodeFile,
    pub reason: String,
}

/// 检查项状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Pending,
}
