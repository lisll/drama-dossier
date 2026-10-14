//! 工具箱纯逻辑：批量改名计划、码率分级、分集重组切点规划、微调帧数。
//!
//! 与 crate 其他模块的分工一致：这一层**只做计算与规划**，不碰进程与文件系统；
//! 真正执行（重命名、调 ffmpeg）在桌面入口 `src-tauri/src/toolbox.rs` 里，
//! 因此这里的每一条规则都能脱离 GUI 单测。

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::{Drama, ProbedEpisode};

/// 一条改名项；`from`/`to` 都是**文件名**（不含目录），防止跨目录移动。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameItem {
    pub from: String,
    pub to: String,
    pub episode_number: Option<u32>,
    /// 目标名与当前名一致（已合规，无需修改）。
    pub unchanged: bool,
}

/// 批量改名计划：`items` 是可执行项，`skipped` 是无法确定集号的文件名。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePlan {
    pub items: Vec<RenameItem>,
    pub skipped: Vec<String>,
}

/// 目标名规则：`{剧名} - 第{NN}集.{原扩展名}`；集号至少两位（`第01集`）。
pub fn episode_target_name(
    drama_name: &str,
    episode_number: u32,
    file_name: &str,
) -> Option<String> {
    let ext = Path::new(file_name).extension()?.to_str()?;
    if ext.is_empty() {
        return None;
    }
    Some(format!("{drama_name} - 第{episode_number:02}集.{ext}"))
}

/// 按扫描结果生成改名计划；解析不出集号的文件进 `skipped`，不改名。
pub fn plan_rename(drama: &Drama) -> RenamePlan {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for episode in &drama.episodes {
        match episode.episode_number {
            Some(number) => {
                let to = episode_target_name(&drama.name, number, &episode.file_name)
                    .unwrap_or_else(|| episode.file_name.clone());
                let unchanged = to == episode.file_name;
                items.push(RenameItem {
                    from: episode.file_name.clone(),
                    to,
                    episode_number: episode.episode_number,
                    unchanged,
                });
            }
            None => skipped.push(episode.file_name.clone()),
        }
    }
    items.sort_by(|left, right| {
        left.episode_number
            .unwrap_or(u32::MAX)
            .cmp(&right.episode_number.unwrap_or(u32::MAX))
    });
    RenamePlan { items, skipped }
}

/// 一条码率分级结果；`needs_fix` 表示低于安全线需要重编码。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitrateItem {
    pub file_name: String,
    pub path: String,
    /// `None` 表示探测不到码率（按不处理处理，但要在界面标出）。
    pub bitrate_bps: Option<u64>,
    pub needs_fix: bool,
}

/// 码率分级：只做比较，不执行转码。
pub fn bitrate_report(episodes: &[ProbedEpisode], min_bitrate_bps: u64) -> Vec<BitrateItem> {
    episodes
        .iter()
        .map(|episode| {
            let bitrate = episode.spec.bitrate_bps;
            BitrateItem {
                file_name: episode.file.file_name.clone(),
                path: episode.file.path.display().to_string(),
                bitrate_bps: bitrate,
                needs_fix: bitrate.is_some_and(|bps| bps < min_bitrate_bps),
            }
        })
        .collect()
}

/// 把全局时间线切成 `target_count` 段：优先落在场景切点上，等分点兜底。
///
/// - `boundaries_sec`：全剧时间线上的场景切点（秒，无需有序）；
/// - `min_segment_sec`：最短段落时长，保证不产生碎片段。
///
/// 返回 `[(start, end)]`，有序、不重叠、覆盖 `[0, total]`。
pub fn plan_recut_segments(
    boundaries_sec: &[f64],
    total_sec: f64,
    target_count: usize,
    min_segment_sec: f64,
) -> Vec<(f64, f64)> {
    // 用 partial_cmp 而不是 `!(total_sec > 0.0)`：NaN 时要照旧拒绝
    // （`<=` 写法会放行 NaN，产生 (0, NaN) 的垃圾分段）。
    if total_sec.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return vec![];
    }
    let target_count = target_count.max(1);
    if target_count == 1 {
        return vec![(0.0, total_sec)];
    }

    let mut sorted: Vec<f64> = boundaries_sec
        .iter()
        .copied()
        .filter(|value| *value > min_segment_sec && *value < total_sec - min_segment_sec)
        .collect();
    sorted.sort_by(|left, right| left.total_cmp(right));
    sorted.dedup_by(|left, right| (*left - *right).abs() < 0.05);

    let mut cuts: Vec<f64> = Vec::with_capacity(target_count - 1);
    for k in 1..target_count {
        let ideal = total_sec * k as f64 / target_count as f64;
        // 可用区间：上一刀之后、且给剩余段落留出最短时长；
        // 剩余时长不够切时放弃这一刀（剩余部分并入最后一段）。
        let lower = cuts.last().copied().unwrap_or(0.0) + min_segment_sec;
        let upper = total_sec - (target_count - k) as f64 * min_segment_sec;
        // 同上：NaN 或区间不可用都要跳过这一刀，partial_cmp 与原 `!(upper > lower)`
        // 在 NaN 上行为一致（拒绝），`<=` 写法则会放行。
        if upper.partial_cmp(&lower) != Some(std::cmp::Ordering::Greater) {
            continue;
        }
        let chosen = sorted
            .iter()
            .copied()
            .filter(|value| *value > lower && *value < upper)
            .min_by(|left, right| (left - ideal).abs().total_cmp(&(right - ideal).abs()))
            .unwrap_or(ideal);
        cuts.push(chosen.clamp(lower, upper));
    }

    let mut segments = Vec::with_capacity(target_count);
    let mut start = 0.0;
    for cut in cuts {
        let cut = cut.min(total_sec);
        if cut - start >= min_segment_sec {
            segments.push((start, cut));
            start = cut;
        }
    }
    if total_sec - start > 0.0 {
        segments.push((start, total_sec));
    }
    segments
}

/// 智能微调的帧数：1~3 帧，由种子决定（执行层用文件名+时间做种子实现“每次不同”）。
pub fn nudge_frames(seed: u64) -> u32 {
    (seed % 3 + 1) as u32
}

/// 路径归一化成分：忽略结尾分隔符、当前目录成分（`.`）与平台大小写差异。
///
/// 不做软链接解析，也不展开 `..`——本函数不碰文件系统，只做纯文本比较，
/// 因此可以在任何平台上单测。执行层在真正写盘前还会用 `canonicalize` 复查一次。
fn path_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(|component| {
            let text = component.as_os_str().to_string_lossy().to_string();
            if cfg!(windows) {
                text.to_lowercase()
            } else {
                text
            }
        })
        .collect()
}

/// 两个路径是否指向同一位置（按 [`path_parts`] 的规则比较）。
pub fn same_path(left: &Path, right: &Path) -> bool {
    path_parts(left) == path_parts(right)
}

/// 输出目录是否就是某个素材文件所在的目录。
///
/// 命中时返回该素材路径：把输出写回素材目录会直接覆盖原文件，
/// 所以要拒绝执行，并把「哪个文件会被覆盖」如实告诉使用者。
pub fn output_dir_hits_sources<'a>(
    output_dir: &Path,
    source_files: &'a [PathBuf],
) -> Option<&'a Path> {
    source_files
        .iter()
        .find(|file| {
            file.parent()
                .map(|parent| same_path(parent, output_dir))
                .unwrap_or(false)
        })
        .map(PathBuf::as_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EpisodeFile;

    fn drama_with(files: &[(&str, Option<u32>)]) -> Drama {
        Drama {
            name: "示例剧".into(),
            dir: "/tmp/示例剧".into(),
            episodes: files
                .iter()
                .map(|(name, number)| EpisodeFile {
                    path: format!("/tmp/示例剧/{name}").into(),
                    file_name: (*name).into(),
                    size_bytes: 1,
                    episode_number: *number,
                })
                .collect(),
            cover: None,
            synopsis: None,
        }
    }

    #[test]
    fn target_name_pads_episode_number_and_keeps_extension() {
        assert_eq!(
            episode_target_name("示例剧", 1, "第1集.mp4").as_deref(),
            Some("示例剧 - 第01集.mp4")
        );
        assert_eq!(
            episode_target_name("示例剧", 12, "xxx.MP4").as_deref(),
            Some("示例剧 - 第12集.MP4")
        );
        // 无扩展名 → 无法改名。
        assert_eq!(episode_target_name("示例剧", 3, "noext"), None);
    }

    #[test]
    fn plan_rename_marks_unchanged_and_skips_unknown() {
        let drama = drama_with(&[
            ("第1集.mp4", Some(1)),
            ("示例剧 - 第02集.mp4", Some(2)),
            ("花絮.mp4", None),
        ]);
        let plan = plan_rename(&drama);
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.skipped, vec!["花絮.mp4".to_string()]);
        assert!(!plan.items[0].unchanged);
        assert_eq!(plan.items[0].to, "示例剧 - 第01集.mp4");
        assert!(plan.items[1].unchanged);
        assert_eq!(plan.items[1].from, plan.items[1].to);
    }

    #[test]
    fn bitrate_report_flags_only_below_threshold() {
        let mut low = episode_for_test("低.mp4");
        low.spec.bitrate_bps = Some(2_000_000);
        let mut high = episode_for_test("高.mp4");
        high.spec.bitrate_bps = Some(6_000_000);
        let mut unknown = episode_for_test("未知.mp4");
        unknown.spec.bitrate_bps = None;

        let report = bitrate_report(&[low, high, unknown], 4_000_000);
        assert!(report[0].needs_fix);
        assert!(!report[1].needs_fix);
        assert!(!report[2].needs_fix);
        assert!(report[2].bitrate_bps.is_none());
    }

    #[test]
    fn recut_prefers_boundaries_and_falls_back_to_even_split() {
        // 100 秒切 4 段：等分点 25/50/75 附近有切点 26/49/77 → 应优先切点。
        let segments = plan_recut_segments(&[3.0, 26.0, 49.0, 77.0], 100.0, 4, 1.0);
        assert_eq!(segments.len(), 4);
        assert!((segments[0].1 - 26.0).abs() < 1e-9);
        assert!((segments[1].1 - 49.0).abs() < 1e-9);
        assert!((segments[2].1 - 77.0).abs() < 1e-9);
        assert!((segments[3].1 - 100.0).abs() < 1e-9);

        // 没有任何切点 → 等分。
        let even = plan_recut_segments(&[], 90.0, 3, 1.0);
        assert_eq!(even.len(), 3);
        assert!((even[0].1 - 30.0).abs() < 1e-9);
        assert!((even[1].1 - 60.0).abs() < 1e-9);
    }

    #[test]
    fn recut_respects_min_segment_and_returns_empty_for_bad_total() {
        assert!(plan_recut_segments(&[], 0.0, 4, 1.0).is_empty());
        // 目标段数多到超过可用时长时，应少切而不是产生碎片。
        let segments = plan_recut_segments(&[2.0], 3.0, 50, 1.0);
        assert!(segments.iter().all(|(start, end)| end - start >= 1.0));
    }

    #[test]
    fn same_path_ignores_trailing_separator_and_current_dir() {
        assert!(same_path(Path::new("/tmp/剧/"), Path::new("/tmp/剧")));
        assert!(same_path(Path::new("/tmp/./剧"), Path::new("/tmp/剧")));
        assert!(!same_path(
            Path::new("/tmp/剧"),
            Path::new("/tmp/剧/微调输出")
        ));
        // 文字相同但层级不同 → 不能算同一路径。
        assert!(!same_path(Path::new("/tmp/a/b"), Path::new("/tmp/a")));
    }

    #[cfg(windows)]
    #[test]
    fn same_path_folds_case_on_windows() {
        assert!(same_path(
            Path::new(r"C:\剧集\示例剧"),
            Path::new(r"c:\剧集\示例剧\")
        ));
    }

    #[test]
    fn output_dir_hits_sources_only_for_source_directory() {
        let sources = vec![
            PathBuf::from("/tmp/剧/第01集.mp4"),
            PathBuf::from("/tmp/剧/第02集.mp4"),
        ];
        assert_eq!(
            output_dir_hits_sources(Path::new("/tmp/剧"), &sources),
            Some(Path::new("/tmp/剧/第01集.mp4"))
        );
        // 素材目录的子目录是默认输出位置，必须放行。
        assert_eq!(
            output_dir_hits_sources(Path::new("/tmp/剧/微调输出"), &sources),
            None
        );
        assert!(output_dir_hits_sources(Path::new("/tmp/剧/"), &sources).is_some());
        assert_eq!(output_dir_hits_sources(Path::new("/tmp/剧"), &[]), None);
    }

    #[test]
    fn nudge_frames_stays_in_range() {
        for seed in [0u64, 1, 2, 3, 7, 99, u64::MAX] {
            let frames = nudge_frames(seed);
            assert!((1..=3).contains(&frames));
        }
    }

    fn episode_for_test(name: &str) -> ProbedEpisode {
        ProbedEpisode {
            file: EpisodeFile {
                path: format!("/tmp/示例剧/{name}").into(),
                file_name: name.into(),
                size_bytes: 1,
                episode_number: Some(1),
            },
            spec: crate::types::EpisodeSpec {
                duration_sec: 60.0,
                width: 1080,
                height: 1920,
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                fps: Some(30.0),
                bitrate_bps: Some(4_000_000),
            },
        }
    }
}
