//! 剪映（CapCut / 剪映专业版）草稿的导出与导入（Roadmap：剪映草稿导入 / 导出）。
//!
//! 目标不是「一键出片」，而是把本地剧集的分集顺序与时长**交接给剪辑工具**，
//! 让剪辑师在时间线上继续处理；反过来也把剪辑工具里排好的顺序读回来做核对。
//!
//! 交换文件：
//! - `draft_content.json`：时间线本体（画布、素材、轨道与片段）；
//! - `draft_meta_info.json`：草稿元信息（名称、创建/修改时间、时长）。
//!
//! 已知边界：草稿格式是**未公开、随版本变化**的私有 JSON，不同版本字段差异很大。
//! 本模块只写「最小可用 + 常见字段」的骨架，读回时也只依赖 `tracks/segments/materials`
//! 这几个稳定概念；遇到不认识的字段一律忽略，不做版本猜测。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::types::ProbedEpisode;

/// 导出的时间线基准：微秒（剪映内部时间单位）。
const MICROS_PER_SEC: f64 = 1_000_000.0;

/// 导出选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DraftExportOptions {
    /// 时间线帧率。
    pub fps: f64,
    /// 画布宽（像素）。
    pub width: u32,
    /// 画布高（像素）。
    pub height: u32,
    /// 是否导入音频轨（本原型只登记素材，不做音频分离）。
    pub include_audio: bool,
    /// 输出根目录；缺省写在剧目目录下。
    pub output_dir: Option<PathBuf>,
}

impl Default for DraftExportOptions {
    fn default() -> Self {
        Self {
            fps: 30.0,
            width: 1080,
            height: 1920,
            include_audio: true,
            output_dir: None,
        }
    }
}

impl DraftExportOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !self.fps.is_finite() || self.fps <= 0.0 || self.fps > 240.0 {
            return Err("时间线帧率必须是 0–240 之间的数。".into());
        }
        if self.width == 0 || self.height == 0 {
            return Err("画布宽高必须大于 0。".into());
        }
        Ok(())
    }
}

/// 导出结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftExportResult {
    pub draft_dir: PathBuf,
    pub content_path: PathBuf,
    pub meta_path: PathBuf,
    pub segment_count: usize,
    pub duration_sec: f64,
}

/// 导入回来的一个片段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedEpisode {
    /// 时间线上的顺序（从 1 开始）。
    pub order: usize,
    pub name: String,
    pub path: PathBuf,
    pub start_sec: f64,
    pub duration_sec: f64,
    pub width: u32,
    pub height: u32,
}

/// 读回的草稿摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapcutDraft {
    pub draft_name: String,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub duration_sec: f64,
    pub segments: Vec<ImportedEpisode>,
}

impl CapcutDraft {
    /// 时间线上登记的文件路径（按顺序），便于与本地扫描结果核对。
    pub fn paths(&self) -> Vec<&Path> {
        self.segments.iter().map(|s| s.path.as_path()).collect()
    }
}

/// 由名称与序号派生一个稳定 id（不引入随机数，导出可复现）。
fn stable_id(namespace: &str, seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update(b"\0");
    hasher.update(seed.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!(
        "{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    )
}

fn secs_to_micros(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * MICROS_PER_SEC).round() as u64
}

fn micros_to_secs(micros: u64) -> f64 {
    micros as f64 / MICROS_PER_SEC
}

/// 把一部剧导出成剪映草稿目录。
pub fn export_capcut_draft(
    drama_name: &str,
    drama_dir: &Path,
    episodes: &[ProbedEpisode],
    opts: &DraftExportOptions,
) -> std::io::Result<DraftExportResult> {
    opts.validate()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let draft_dir = opts
        .output_dir
        .clone()
        .unwrap_or_else(|| drama_dir.to_path_buf())
        .join(format!("{drama_name}_剪映草稿"));
    if draft_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "草稿目录已存在：{}。请选择新的输出目录或先移除旧目录。",
                draft_dir.display()
            ),
        ));
    }
    std::fs::create_dir_all(&draft_dir)?;

    let mut videos: Vec<Value> = Vec::new();
    let mut segments: Vec<Value> = Vec::new();
    let mut audios: Vec<Value> = Vec::new();
    let mut cursor_micros: u64 = 0;

    for (index, episode) in episodes.iter().enumerate() {
        let duration_micros = secs_to_micros(episode.spec.duration_sec);
        let path_text = episode.file.path.display().to_string();
        let material_id = stable_id("material", &format!("{index}:{path_text}"));
        let segment_id = stable_id("segment", &format!("{index}:{path_text}"));

        videos.push(json!({
            "id": material_id,
            "type": "video",
            "path": path_text,
            "material_name": episode.file.file_name,
            "duration": duration_micros,
            "width": episode.spec.width,
            "height": episode.spec.height,
            "has_audio": opts.include_audio,
        }));
        if opts.include_audio {
            audios.push(json!({
                "id": stable_id("audio", &format!("{index}:{path_text}")),
                "type": "audio",
                "path": path_text,
                "duration": duration_micros,
                "name": episode.file.file_name,
            }));
        }
        segments.push(json!({
            "id": segment_id,
            "material_id": material_id,
            "target_timerange": { "start": cursor_micros, "duration": duration_micros },
            "source_timerange": { "start": 0, "duration": duration_micros },
            "speed": 1.0,
            "volume": 1.0,
            "visible": true,
        }));
        cursor_micros += duration_micros;
    }

    let content = json!({
        "canvas_config": { "width": opts.width, "height": opts.height, "ratio": "original" },
        "color_space": 0,
        "duration": cursor_micros,
        "fps": opts.fps,
        "id": stable_id("draft", drama_name),
        "materials": { "videos": videos, "audios": audios, "texts": [], "stickers": [] },
        "platform": { "app_id": 3704, "os": "linux", "os_version": "generic" },
        "tracks": [
            { "id": stable_id("track", "video"), "type": "video", "attribute": 0, "segments": segments },
        ],
        "version": 360000,
        "draft_name": drama_name,
    });
    let meta = json!({
        "draft_id": stable_id("draft-meta", drama_name),
        "draft_name": format!("{drama_name}_剪映草稿"),
        "draft_vendor": "drama-dossier",
        "draft_root_path": draft_dir.display().to_string(),
        "draft_duration": cursor_micros,
        "draft_fold_path": draft_dir.display().to_string(),
        "draft_removable_storage_device": "",
        "draft_cover": "",
        "draft_materials": [],
    });

    let content_path = draft_dir.join("draft_content.json");
    let meta_path = draft_dir.join("draft_meta_info.json");
    write_json(&content_path, &content)?;
    write_json(&meta_path, &meta)?;
    std::fs::write(
        draft_dir.join("导入说明.txt"),
        format!(
            "本目录由 DramaDossier 生成，用于技术学习。\n\
             把整个目录放进剪映的草稿目录，或用它对照检查分集顺序与时长。\n\
             草稿格式随剪映版本变化，导入失败时请以本地素材为准。\n\
             分集数：{}\n总时长：{:.2} 秒\n",
            episodes.len(),
            micros_to_secs(cursor_micros)
        ),
    )?;

    Ok(DraftExportResult {
        draft_dir: draft_dir.clone(),
        content_path,
        meta_path,
        segment_count: episodes.len(),
        duration_sec: micros_to_secs(cursor_micros),
    })
}

fn write_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// 读取剪映草稿目录（只需要 `draft_content.json`）。
pub fn import_capcut_draft(draft_dir: &Path) -> std::io::Result<CapcutDraft> {
    let content_path = if draft_dir.is_dir() {
        draft_dir.join("draft_content.json")
    } else {
        draft_dir.to_path_buf()
    };
    let text = std::fs::read_to_string(&content_path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("读取草稿失败（{}）：{error}", content_path.display()),
        )
    })?;
    let root: Value = serde_json::from_str(&text)
        .map_err(|error| std::io::Error::other(format!("草稿不是合法 JSON：{error}")))?;

    let canvas = root.get("canvas_config").cloned().unwrap_or(Value::Null);
    let width = canvas.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height = canvas.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;

    // 素材表：videos 里按 id 建索引，片段只引用 id。
    let mut materials: Vec<(String, &Value)> = Vec::new();
    if let Some(videos) = root
        .get("materials")
        .and_then(|m| m.get("videos"))
        .and_then(Value::as_array)
    {
        for video in videos {
            if let Some(id) = video.get("id").and_then(Value::as_str) {
                materials.push((id.to_string(), video));
            }
        }
    }

    let mut segments = Vec::new();
    if let Some(tracks) = root.get("tracks").and_then(Value::as_array) {
        let mut raw: Vec<&Value> = Vec::new();
        for track in tracks {
            let is_video = track
                .get("type")
                .and_then(Value::as_str)
                .map(|t| t == "video")
                .unwrap_or(true);
            if !is_video {
                continue;
            }
            if let Some(items) = track.get("segments").and_then(Value::as_array) {
                raw.extend(items.iter());
            }
        }
        raw.sort_by_key(|segment| {
            segment
                .get("target_timerange")
                .and_then(|t| t.get("start"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
        for (index, segment) in raw.iter().enumerate() {
            let material_id = segment
                .get("material_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let material = materials
                .iter()
                .find(|(id, _)| id == material_id)
                .map(|(_, value)| *value);
            let target = segment
                .get("target_timerange")
                .cloned()
                .unwrap_or(Value::Null);
            let source = segment
                .get("source_timerange")
                .cloned()
                .unwrap_or(Value::Null);
            let duration_micros = source
                .get("duration")
                .and_then(Value::as_u64)
                .or_else(|| target.get("duration").and_then(Value::as_u64))
                .or_else(|| {
                    material
                        .and_then(|m| m.get("duration"))
                        .and_then(Value::as_u64)
                })
                .unwrap_or(0);
            let path_text = material
                .and_then(|m| m.get("path"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = material
                .and_then(|m| m.get("material_name"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    Path::new(&path_text)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                });
            segments.push(ImportedEpisode {
                order: index + 1,
                name,
                path: PathBuf::from(path_text),
                start_sec: micros_to_secs(target.get("start").and_then(Value::as_u64).unwrap_or(0)),
                duration_sec: micros_to_secs(duration_micros),
                width: material
                    .and_then(|m| m.get("width"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                height: material
                    .and_then(|m| m.get("height"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
            });
        }
    }

    let duration_micros = root
        .get("duration")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            segments
                .iter()
                .map(|s| secs_to_micros(s.start_sec + s.duration_sec))
                .max()
                .unwrap_or(0)
        });
    let draft_name = root
        .get("draft_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Ok(CapcutDraft {
        draft_name,
        fps: root.get("fps").and_then(Value::as_f64).unwrap_or(30.0),
        width,
        height,
        duration_sec: micros_to_secs(duration_micros),
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EpisodeFile, EpisodeSpec};

    fn episodes() -> Vec<ProbedEpisode> {
        (1..=3)
            .map(|i| ProbedEpisode {
                file: EpisodeFile {
                    path: PathBuf::from(format!("/tmp/d/第{i:02}集.mp4")),
                    file_name: format!("第{i:02}集.mp4"),
                    size_bytes: 1,
                    episode_number: Some(i),
                },
                spec: EpisodeSpec {
                    duration_sec: 60.0 + i as f64,
                    width: 1080,
                    height: 1920,
                    video_codec: Some("h264".into()),
                    audio_codec: Some("aac".into()),
                    fps: Some(30.0),
                    bitrate_bps: Some(4_000_000),
                    ..Default::default()
                },
            })
            .collect()
    }

    #[test]
    fn export_then_import_round_trips_the_timeline() {
        let dir = std::env::temp_dir().join(format!("dossier-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let result =
            export_capcut_draft("测试剧", &dir, &episodes(), &DraftExportOptions::default())
                .expect("导出应成功");
        assert_eq!(result.segment_count, 3);
        assert!((result.duration_sec - (61.0 + 62.0 + 63.0)).abs() < 0.01);

        let draft = import_capcut_draft(&result.draft_dir).expect("导入应成功");
        assert_eq!(draft.segments.len(), 3);
        assert_eq!(draft.width, 1080);
        assert_eq!(draft.height, 1920);
        assert_eq!(draft.segments[0].order, 1);
        assert_eq!(draft.segments[0].name, "第01集.mp4");
        // 片段在时间线上首尾相接。
        assert!(draft.segments[0].start_sec.abs() < 1e-6);
        assert!((draft.segments[1].start_sec - 61.0).abs() < 0.01);
        assert!((draft.segments[2].start_sec - 123.0).abs() < 0.01);
        assert!((draft.duration_sec - 186.0).abs() < 0.01);
        assert_eq!(draft.paths().len(), 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_refuses_to_overwrite_existing_draft() {
        let dir = std::env::temp_dir().join(format!("dossier-draft-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        export_capcut_draft("测试剧", &dir, &episodes(), &DraftExportOptions::default()).unwrap();
        let again =
            export_capcut_draft("测试剧", &dir, &episodes(), &DraftExportOptions::default());
        assert!(again.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_tolerates_minimal_and_foreign_drafts() {
        let dir = std::env::temp_dir().join(format!("dossier-draft-min-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("draft_content.json"),
            r#"{"canvas_config":{"width":720,"height":1280},"fps":25,
                "materials":{"videos":[{"id":"v1","path":"/tmp/a.mp4","material_name":"a.mp4",
                "duration":5000000,"width":720,"height":1280}]},
                "tracks":[{"type":"video","segments":[{"id":"s1","material_id":"v1",
                "target_timerange":{"start":0,"duration":5000000},
                "source_timerange":{"start":0,"duration":5000000}}]},
                {"type":"audio","segments":[{"id":"a1"}]}],
                "extra_unknown_field": {"version_specific": true}}"#,
        )
        .unwrap();
        let draft = import_capcut_draft(&dir).unwrap();
        assert_eq!(draft.segments.len(), 1, "音频轨应被忽略");
        assert_eq!(draft.segments[0].name, "a.mp4");
        assert!((draft.segments[0].duration_sec - 5.0).abs() < 1e-6);
        assert!((draft.duration_sec - 5.0).abs() < 1e-6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_rejects_invalid_options() {
        assert!(DraftExportOptions::default().validate().is_ok());
        assert!(DraftExportOptions {
            width: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(DraftExportOptions {
            fps: -1.0,
            ..Default::default()
        }
        .validate()
        .is_err());
    }
}
