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
//!
//! 导出结构（三轨）：
//! - **视频轨**：每集一个片段，`volume` 为 `0`（画面保留、原声不在这一轨出声）；
//! - **音频轨**：每集一个片段，时间范围与同序号的视频片段**完全一致**，`volume` 为 `1`，
//!   素材指向 `materials.audios` 里那条音频——这就是「独立原声轨」；
//! - **字幕轨**：剧目目录里有与分集同名的 `.srt` 时，每条 cue 一个文字素材 + 一个片段。
//!
//! 为什么要拆成两条轨而不是直接给视频片段留原声：剪辑师常要单独处理配乐/人声，
//! 视频轨静音 + 原声独立成轨是剪映里「视频原声分离」的结果形态，后续改动不必重新拆轨。

use std::path::{Path, PathBuf};

use dossier_media::srt::{load_srt, SubtitleCue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::types::ProbedEpisode;

/// 导出的时间线基准：微秒（剪映内部时间单位）。
const MICROS_PER_SEC: f64 = 1_000_000.0;

/// 字幕富文本里写的字号。
///
/// 这个值来自社区的草稿结构记录（见 [`caption_rich_text`] 的说明），不是官方规范；
/// 剪映读到自己不认识的字号也只是按默认样式显示，不会解析失败。
const CAPTION_FONT_SIZE: &str = "9.0";

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

/// 把一集对应的字幕文件路径找出来：分集视频同名、扩展名换成 `.srt`。
///
/// 找不到（或路径没有文件名）时返回 `None`。命中与否**只看存在性**，
/// 不做「按集号猜 srt」的模糊匹配——猜错会把别的集的字幕挂到这一集上。
fn sibling_srt_path(episode: &ProbedEpisode) -> Option<PathBuf> {
    let candidate = episode.file.path.with_extension("srt");
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

/// 转义 XML 文本节点里的三个字符。
///
/// 剪映的文字素材 `content` 是一段 `<font>` 包裹的**富文本**，正文被当成 XML 片段解析，
/// 所以正文里裸的 `<` / `>` / `&` 会让整段解析失败。这里按 XML 1.0 的最小转义集处理，
/// `&` 必须先换（否则会把后面新生成的 `&lt;` 再转义一次）。
fn escape_xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 生成剪映文字素材的富文本正文。
///
/// **依据说明（重要）**：剪映草稿格式没有公开规范，下面这个形状来自社区文档与开源工具的
/// 记录（例如开源草稿库 pyJianYingDraft 的文本素材示例、以及若干解析剪映字幕的博客
/// 一致给出的 `"<font id=\"\" size=\"9.0\"><color_val>文本</color_val></font>"` 结构），
/// **不是官方规范**，也可能随剪映版本变化。我们的实现只依赖其中两件事：
/// 1. 正文是一段 XML 片段，文本必须转义（这条确定性最高，也最容易验证）；
/// 2. 外层用 `<font>` 包一层，即使剪映换了解析方式，退化成「纯文本」也不至于报错。
///
/// `id` 留空表示「用默认字体」，这样不需要引用一个本机可能不存在的字体文件。
fn caption_rich_text(text: &str) -> String {
    format!(
        "<font id=\"\" size=\"{CAPTION_FONT_SIZE}\"><color_val>{}</color_val></font>",
        escape_xml_text(text)
    )
}

/// 一条字幕变成的（素材, 片段）对里需要的全部时间信息。
struct CaptionPlacement {
    /// 该字幕在**整条草稿**时间轴上的起点（微秒）。
    start_micros: u64,
    duration_micros: u64,
}

/// 把一集的字幕 cue 换算到整条草稿的时间轴上。
///
/// `episode_offset_micros` 是这一集在草稿里的起点（前面各集时长之和），
/// 加上 cue 在本集内的偏移才是全局时间——直接拿本集秒数当全局时间，
/// 第 2 集起就会全部堆在开头。
fn place_cues(cues: &[SubtitleCue], episode_offset_micros: u64) -> Vec<CaptionPlacement> {
    cues.iter()
        .map(|cue| {
            let start = episode_offset_micros + secs_to_micros(cue.start_sec);
            let mut duration = secs_to_micros(cue.duration());
            if duration == 0 {
                // 零时长的字幕在播放器里等于「不显示」，给一个最短可见时长兜住。
                duration = secs_to_micros(0.2);
            }
            CaptionPlacement {
                start_micros: start,
                duration_micros: duration,
            }
        })
        .collect()
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
    let mut audio_segments: Vec<Value> = Vec::new();
    let mut texts: Vec<Value> = Vec::new();
    let mut text_segments: Vec<Value> = Vec::new();
    let mut cursor_micros: u64 = 0;

    for (index, episode) in episodes.iter().enumerate() {
        let duration_micros = secs_to_micros(episode.spec.duration_sec);
        let path_text = episode.file.path.display().to_string();
        let material_id = stable_id("material", &format!("{index}:{path_text}"));
        let segment_id = stable_id("segment", &format!("{index}:{path_text}"));
        // 视频片段与同集原声片段必须共用同一段时间轴，所以只构造一次。
        let target_timerange = json!({ "start": cursor_micros, "duration": duration_micros });

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
        segments.push(json!({
            "id": segment_id,
            "material_id": material_id,
            "target_timerange": target_timerange,
            "source_timerange": { "start": 0, "duration": duration_micros },
            "speed": 1.0,
            // 原声不在视频轨出声，改由下面的独立音频轨承担——**前提是确实建了音频轨**。
            // 没有音频轨（`include_audio = false`）时视频轨必须保持原音量，
            // 否则整条草稿会一声不响（静音视频 + 不存在的音频轨 = 无声）。
            "volume": if opts.include_audio { 0.0 } else { 1.0 },
            "visible": true,
        }));
        if opts.include_audio {
            let audio_material_id = stable_id("audio", &format!("{index}:{path_text}"));
            audios.push(json!({
                "id": audio_material_id,
                "type": "audio",
                "path": path_text,
                "duration": duration_micros,
                "name": episode.file.file_name,
            }));
            audio_segments.push(json!({
                "id": stable_id("audio-segment", &format!("{index}:{path_text}")),
                "material_id": audio_material_id,
                "target_timerange": target_timerange,
                "source_timerange": { "start": 0, "duration": duration_micros },
                "speed": 1.0,
                "volume": 1.0,
            }));
        }

        // 字幕：只有存在同名 .srt 的集才生成，且时间轴按「草稿全局」计算。
        if let Some(srt_path) = sibling_srt_path(episode) {
            let cues = load_srt(&srt_path).map_err(|error| {
                std::io::Error::other(format!("读取字幕失败（{}）：{error}", srt_path.display()))
            })?;
            for (cue_index, (cue, placement)) in cues
                .iter()
                .zip(place_cues(&cues, cursor_micros))
                .enumerate()
            {
                let text_material_id =
                    stable_id("text-material", &format!("{index}:{cue_index}:{path_text}"));
                texts.push(json!({
                    "id": text_material_id,
                    // 富文本正文；转义与包裹方式的依据见 caption_rich_text 的说明。
                    "content": caption_rich_text(&cue.text),
                    "type": "subtitle",
                    "alignment": 1,
                    "letter_spacing": 0,
                    "line_spacing": 0,
                }));
                text_segments.push(json!({
                    "id": stable_id("text-segment", &format!("{index}:{cue_index}:{path_text}")),
                    "material_id": text_material_id,
                    "target_timerange": {
                        "start": placement.start_micros,
                        "duration": placement.duration_micros,
                    },
                    "visible": true,
                }));
            }
        }

        cursor_micros += duration_micros;
    }

    let mut tracks = vec![json!({
        "id": stable_id("track", "video"),
        "type": "video",
        "attribute": 0,
        "segments": segments,
    })];
    if opts.include_audio && !audio_segments.is_empty() {
        tracks.push(json!({
            "id": stable_id("track", "audio"),
            "type": "audio",
            "attribute": 0,
            "segments": audio_segments,
        }));
    }
    // 一条字幕都没有（整部剧没有 .srt）时不生成 text 轨，materials.texts 保持空数组。
    if !text_segments.is_empty() {
        tracks.push(json!({
            "id": stable_id("track", "text"),
            "type": "text",
            "attribute": 0,
            "segments": text_segments,
        }));
    }

    let content = json!({
        "canvas_config": { "width": opts.width, "height": opts.height, "ratio": "original" },
        "color_space": 0,
        "duration": cursor_micros,
        "fps": opts.fps,
        "id": stable_id("draft", drama_name),
        "materials": { "videos": videos, "audios": audios, "texts": texts, "stickers": [] },
        "platform": { "app_id": 3704, "os": "linux", "os_version": "generic" },
        "tracks": tracks,
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

    // ---- 原声轨 / 字幕轨 ----
    //
    // 这一节的测试只保证「结构自洽 + 时间轴对得上 + 素材 id 引用都存在」，
    // 能不能被剪映正确打开只能真机确认（草稿格式不公开）。

    /// 一个自带临时目录的剧目目录：视频文件与（可选的）.srt 真的落在磁盘上。
    struct DramaFixture {
        root: PathBuf,
    }

    impl DramaFixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dossier-draft-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        /// 写一个占位视频文件（内容不需要是真视频：导出只读探测结果）。
        fn touch_episode(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, b"not a real video").unwrap();
            path
        }

        fn write_srt(&self, name: &str, body: &str) {
            std::fs::write(self.root.join(name), body).unwrap();
        }
    }

    impl Drop for DramaFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// 用给定的文件路径构造分集（时长 61 / 62 / 63 秒，与现有测试一致）。
    fn episodes_at(paths: &[PathBuf]) -> Vec<ProbedEpisode> {
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| ProbedEpisode {
                file: EpisodeFile {
                    path: path.clone(),
                    file_name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    size_bytes: 1,
                    episode_number: Some(index as u32 + 1),
                },
                spec: EpisodeSpec {
                    duration_sec: 61.0 + index as f64,
                    width: 1080,
                    height: 1920,
                    video_codec: Some("h264".into()),
                    audio_codec: Some("aac".into()),
                    fps: Some(30.0),
                    bitrate_bps: Some(4_000_000),
                },
            })
            .collect()
    }

    /// 导出并读回 draft_content.json，省得每个用例都重复这几行。
    fn export_and_read(
        fixture: &DramaFixture,
        drama_name: &str,
        episodes: &[ProbedEpisode],
    ) -> Value {
        let result = export_capcut_draft(
            drama_name,
            fixture.path(),
            episodes,
            &DraftExportOptions::default(),
        )
        .expect("导出应成功");
        let text = std::fs::read_to_string(result.content_path).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// 按 type 取轨。
    fn track_of<'a>(content: &'a Value, track_type: &str) -> Option<&'a Value> {
        content
            .get("tracks")
            .and_then(Value::as_array)?
            .iter()
            .find(|track| track.get("type").and_then(Value::as_str) == Some(track_type))
    }

    fn segments_of<'a>(content: &'a Value, track_type: &str) -> &'a [Value] {
        track_of(content, track_type)
            .and_then(|track| track.get("segments"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn materials_of<'a>(content: &'a Value, kind: &str) -> &'a [Value] {
        content
            .get("materials")
            .and_then(|m| m.get(kind))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 不要音频轨时，视频轨必须保持原音量——否则整条草稿一声不响。
    ///
    /// 这是「拆出独立原声轨」这个改动的边界：静音视频轨只在**确实建了音频轨**时才对，
    /// 否则等于把声音彻底丢掉。
    #[test]
    fn without_an_audio_track_the_video_keeps_its_volume() {
        let fixture = DramaFixture::new("noaudio");
        let episodes = episodes_at(&[fixture.touch_episode("第01集.mp4")]);
        let result = export_capcut_draft(
            "无声剧",
            fixture.path(),
            &episodes,
            &DraftExportOptions {
                include_audio: false,
                ..DraftExportOptions::default()
            },
        )
        .expect("导出应成功");
        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(result.content_path).unwrap()).unwrap();

        for segment in segments_of(&content, "video") {
            assert_eq!(
                segment.get("volume").and_then(Value::as_f64),
                Some(1.0),
                "没有音频轨时必须保持原音量，否则整条草稿无声：{segment}"
            );
        }
        // 也不该凭空多出音频轨或音频素材。
        assert!(materials_of(&content, "audios").is_empty());
        let tracks = content["tracks"].as_array().unwrap();
        assert!(
            !tracks
                .iter()
                .any(|track| track.get("type").and_then(Value::as_str) == Some("audio")),
            "没要音频就不该建音频轨：{tracks:?}"
        );
    }

    #[test]
    fn video_segments_are_muted_so_the_separate_original_audio_can_speak() {
        let fixture = DramaFixture::new("mute");
        let episodes = episodes_at(&[
            fixture.touch_episode("第01集.mp4"),
            fixture.touch_episode("第02集.mp4"),
            fixture.touch_episode("第03集.mp4"),
        ]);
        let content = export_and_read(&fixture, "静音剧", &episodes);

        let videos = segments_of(&content, "video");
        assert_eq!(videos.len(), 3);
        for segment in videos {
            assert_eq!(
                segment.get("volume").and_then(Value::as_f64),
                Some(0.0),
                "视频片段必须静音：{segment}"
            );
            assert_eq!(segment.get("visible").and_then(Value::as_bool), Some(true));
        }
        // 素材本身仍然登记为「有音轨」，静音只发生在片段上。
        for material in materials_of(&content, "videos") {
            assert_eq!(
                material.get("has_audio").and_then(Value::as_bool),
                Some(true)
            );
        }
    }

    #[test]
    fn audio_track_mirrors_every_video_timerange_field_by_field() {
        let fixture = DramaFixture::new("audio");
        let episodes = episodes_at(&[
            fixture.touch_episode("第01集.mp4"),
            fixture.touch_episode("第02集.mp4"),
            fixture.touch_episode("第03集.mp4"),
        ]);
        let content = export_and_read(&fixture, "原声剧", &episodes);

        let audio_track = track_of(&content, "audio").expect("必须存在一条 audio 轨");
        let audio_segments = segments_of(&content, "audio");
        let video_segments = segments_of(&content, "video");
        assert_eq!(
            audio_segments.len(),
            video_segments.len(),
            "每集一段原声；同一轨：{}",
            audio_track
        );
        for (video, audio) in video_segments.iter().zip(audio_segments.iter()) {
            // 「逐字段相等」：直接比较整个 target_timerange 对象。
            assert_eq!(
                audio.get("target_timerange"),
                video.get("target_timerange"),
                "原声片段的时间范围必须与视频片段完全一致"
            );
            assert_eq!(
                audio.get("volume").and_then(Value::as_f64),
                Some(1.0),
                "原声轨音量应为 1.0"
            );
            // source_timerange 也保持一致，剪辑师截取素材的范围才没歧义。
            assert_eq!(audio.get("source_timerange"), video.get("source_timerange"));
        }
    }

    #[test]
    fn audio_segments_reference_existing_audio_materials() {
        let fixture = DramaFixture::new("audio-ref");
        let episodes = episodes_at(&[
            fixture.touch_episode("第01集.mp4"),
            fixture.touch_episode("第02集.mp4"),
        ]);
        let content = export_and_read(&fixture, "引用剧", &episodes);

        let audios = materials_of(&content, "audios");
        assert_eq!(audios.len(), 2);
        let audio_ids: Vec<&str> = audios
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .collect();

        for segment in segments_of(&content, "audio") {
            let material_id = segment
                .get("material_id")
                .and_then(Value::as_str)
                .expect("音频片段必须有 material_id");
            assert!(
                audio_ids.contains(&material_id),
                "material_id {material_id} 不在 materials.audios 里：{audio_ids:?}"
            );
        }
        // 视频片段的引用也不能断。
        let video_ids: Vec<&str> = materials_of(&content, "videos")
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .collect();
        for segment in segments_of(&content, "video") {
            let material_id = segment
                .get("material_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(video_ids.contains(&material_id));
        }
    }

    #[test]
    fn subtitle_track_offsets_cues_by_the_accumulated_episode_start() {
        let fixture = DramaFixture::new("subs");
        let episodes = episodes_at(&[
            fixture.touch_episode("第01集.mp4"),
            fixture.touch_episode("第02集.mp4"),
            fixture.touch_episode("第03集.mp4"),
        ]);
        // 手算表：每集两句，全局起点 = 前面各集时长之和 + 本集内偏移。
        // 时长：第1集 61s、第2集 62s、第3集 63s。
        // 第1集起点 0      → cue 在 1.0s   = 1_000_000us，时长 2.0s = 2_000_000us
        //                    cue 在 3.5s   = 3_500_000us，时长 1.5s = 1_500_000us
        // 第2集起点 61_000_000 → cue 在 2.0s = 63_000_000us，时长 1.0s
        //                        cue 在 5.0s = 66_000_000us，时长 2.5s
        // 第3集起点 123_000_000 → cue 在 0.5s = 123_500_000us，时长 0.25s
        //                         cue 在 4.0s = 127_000_000us，时长 1.0s
        fixture.write_srt(
            "第01集.srt",
            "1\n00:00:01,000 --> 00:00:03,000\n第一集第一句\n\n\
             2\n00:00:03,500 --> 00:00:05,000\n第一集第二句\n\n",
        );
        fixture.write_srt(
            "第02集.srt",
            "1\n00:00:02,000 --> 00:00:03,000\n第二集第一句\n\n\
             2\n00:00:05,000 --> 00:00:07,500\n第二集第二句\n\n",
        );
        // 第3集故意只写一条，验证「每集条数可以不同」。
        fixture.write_srt(
            "第03集.srt",
            "1\n00:00:00,500 --> 00:00:00,750\n第三集第一句\n\n\
             2\n00:00:04,000 --> 00:00:05,000\n第三集第二句\n\n",
        );
        let content = export_and_read(&fixture, "字幕剧", &episodes);

        let texts = materials_of(&content, "texts");
        let text_segments = segments_of(&content, "text");
        assert!(track_of(&content, "text").is_some(), "必须生成 text 轨");
        assert_eq!(texts.len(), 6, "三条 srt 共 6 条 cue");
        assert_eq!(text_segments.len(), 6);

        let expected: [(u64, u64, &str); 6] = [
            (1_000_000, 2_000_000, "第一集第一句"),
            (3_500_000, 1_500_000, "第一集第二句"),
            (63_000_000, 1_000_000, "第二集第一句"),
            (66_000_000, 2_500_000, "第二集第二句"),
            (123_500_000, 250_000, "第三集第一句"),
            (127_000_000, 1_000_000, "第三集第二句"),
        ];
        for (segment, (start, duration, text)) in text_segments.iter().zip(expected.iter()) {
            let timerange = segment
                .get("target_timerange")
                .expect("字幕片段要有时间范围");
            assert_eq!(
                timerange.get("start").and_then(Value::as_u64),
                Some(*start),
                "{text}：全局起点应为 {start}"
            );
            assert_eq!(
                timerange.get("duration").and_then(Value::as_u64),
                Some(*duration),
                "{text}：时长应为 {duration}"
            );
            let material_id = segment
                .get("material_id")
                .and_then(Value::as_str)
                .expect("字幕片段必须有 material_id");
            let material = texts
                .iter()
                .find(|m| m.get("id").and_then(Value::as_str) == Some(material_id))
                .expect("字幕片段引用的文字素材必须存在");
            let body = material.get("content").and_then(Value::as_str).unwrap();
            assert!(
                body.contains(text),
                "{text} 应出现在富文本正文里，实际：{body}"
            );
        }
    }

    #[test]
    fn episodes_without_srt_produce_no_text_track_and_no_text_materials() {
        let fixture = DramaFixture::new("nosubs");
        let episodes = episodes_at(&[
            fixture.touch_episode("第01集.mp4"),
            fixture.touch_episode("第02集.mp4"),
        ]);
        // 只给第 2 集配字幕：第 1 集不该凭空多出片段。
        fixture.write_srt(
            "第02集.srt",
            "1\n00:00:01,000 --> 00:00:02,000\n只有第二集有字幕\n\n",
        );
        let content = export_and_read(&fixture, "半字幕剧", &episodes);
        assert_eq!(materials_of(&content, "texts").len(), 1);
        assert_eq!(segments_of(&content, "text").len(), 1);
        let timerange = segments_of(&content, "text")[0]
            .get("target_timerange")
            .unwrap();
        // 第 1 集 61 秒 + 本集内 1 秒 = 62 秒。
        assert_eq!(
            timerange.get("start").and_then(Value::as_u64),
            Some(62_000_000)
        );

        // 一个 .srt 都没有：不出 text 轨，materials.texts 为空数组。
        let plain = DramaFixture::new("nosubs-none");
        let plain_episodes = episodes_at(&[plain.touch_episode("第01集.mp4")]);
        let content = export_and_read(&plain, "无字幕剧", &plain_episodes);
        assert!(
            track_of(&content, "text").is_none(),
            "没有 .srt 时不应生成 text 轨"
        );
        assert_eq!(
            materials_of(&content, "texts").len(),
            0,
            "没有 .srt 时 materials.texts 保持空数组"
        );
        // 顺带确认三条轨的正确形态：只有视频 + 音频。
        let types: Vec<&str> = content
            .get("tracks")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|t| t.get("type").and_then(Value::as_str))
            .collect();
        assert_eq!(types, vec!["video", "audio"]);
    }

    #[test]
    fn subtitle_text_is_xml_escaped_inside_the_rich_text_wrapper() {
        let fixture = DramaFixture::new("escape");
        let episodes = episodes_at(&[fixture.touch_episode("第01集.mp4")]);
        fixture.write_srt(
            "第01集.srt",
            "1\n00:00:01,000 --> 00:00:02,000\n1 < 2 & 3 > 2\n\n",
        );
        let content = export_and_read(&fixture, "转义剧", &episodes);
        let body = materials_of(&content, "texts")[0]
            .get("content")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        // 正文必须被转义……
        assert!(
            body.contains("1 &lt; 2 &amp; 3 &gt; 2"),
            "正文应转义，实际：{body}"
        );
        // ……否则富文本就会多出几个标签。
        assert!(
            !body.contains("1 < 2"),
            "正文里不能出现裸的 `<`，实际：{body}"
        );
        // `&` 只能转义一次，不能出现 `&amp;lt;` 这种双重转义。
        assert!(!body.contains("&amp;lt;"), "不允许双重转义：{body}");
        // 包裹结构本身还在。
        assert!(body.starts_with("<font"), "应以 <font 开头：{body}");
        assert!(body.ends_with("</font>"), "应以 </font> 结尾：{body}");
    }

    #[test]
    fn exporting_the_same_input_twice_is_byte_identical() {
        // 字幕轨也走 stable_id，所以两次导出的 draft_content.json 必须逐字节相同。
        let fixture = DramaFixture::new("stable");
        let episodes = episodes_at(&[
            fixture.touch_episode("第01集.mp4"),
            fixture.touch_episode("第02集.mp4"),
        ]);
        fixture.write_srt(
            "第01集.srt",
            "1\n00:00:01,000 --> 00:00:02,000\n第一集\n\n\
             2\n00:00:03,000 --> 00:00:04,000\n第二句\n\n",
        );
        fixture.write_srt("第02集.srt", "1\n00:00:01,000 --> 00:00:02,000\n换个集\n\n");

        let first_dir = fixture.path().join("out1");
        let second_dir = fixture.path().join("out2");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first = export_capcut_draft(
            "稳定剧",
            fixture.path(),
            &episodes,
            &DraftExportOptions {
                output_dir: Some(first_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let second = export_capcut_draft(
            "稳定剧",
            fixture.path(),
            &episodes,
            &DraftExportOptions {
                output_dir: Some(second_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        let first_bytes = std::fs::read(&first.content_path).unwrap();
        let second_bytes = std::fs::read(&second.content_path).unwrap();
        assert_eq!(
            first_bytes, second_bytes,
            "同一输入两次导出的 draft_content.json 必须逐字节相同"
        );
    }
}
