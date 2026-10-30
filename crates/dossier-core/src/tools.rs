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

/// 码率偏离安全区间的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BitrateDirection {
    /// 低于下限：重编码拉到 `下限 + 0.5 Mbps`（不超过上限）
    Low,
    /// 高于上限：重编码压到上限
    High,
}

/// 一条码率分级结果；`needs_fix` 表示码率超出 `[下限, 上限]`、需要重编码。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitrateItem {
    pub file_name: String,
    pub path: String,
    /// 用于判定的视频轨码率；`None` 表示探测不到（不处理，但要在界面标出）。
    pub bitrate_bps: Option<u64>,
    /// `bitrate_bps` 不是直接读到的视频轨码率，而是由文件码率估算出来的。
    pub estimated: bool,
    pub needs_fix: bool,
    pub direction: Option<BitrateDirection>,
    /// 规格探测失败的原因；为空表示探测成功（不序列化，兼容旧界面）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BitrateItem {
    /// 探测失败的分集：码率未知、不修，原因写进 `error` 让界面显示。
    pub fn probe_failed(file_name: &str, path: &str, reason: &str) -> Self {
        Self {
            file_name: file_name.to_string(),
            path: path.to_string(),
            bitrate_bps: None,
            estimated: false,
            needs_fix: false,
            direction: None,
            error: Some(format!("探测失败：{reason}")),
        }
    }
}

/// 判定用的视频轨码率：视频轨 stream 码率 → 文件码率减音频码率 → 文件码率。
/// 返回 `(码率, 是否为估算)`。
///
/// 只看文件码率会把音频也算进去：音频码率高的文件，视频轨实际不足下限也会被判「达标」。
pub fn video_bitrate(spec: &crate::types::EpisodeSpec) -> (Option<u64>, bool) {
    if let Some(video) = spec.video_bitrate_bps.filter(|bps| *bps > 0) {
        return (Some(video), false);
    }
    match (spec.bitrate_bps, spec.audio_bitrate_bps) {
        (Some(total), Some(audio)) if total > audio => (Some(total - audio), true),
        (Some(total), _) => (Some(total), true),
        (None, _) => (None, false),
    }
}

/// 码率分级：只做比较，不执行转码。`[min, max]` 内的不处理。
pub fn bitrate_report(
    episodes: &[ProbedEpisode],
    min_bitrate_bps: u64,
    max_bitrate_bps: u64,
) -> Vec<BitrateItem> {
    episodes
        .iter()
        .map(|episode| {
            let (bitrate, estimated) = video_bitrate(&episode.spec);
            let direction = bitrate.and_then(|bps| {
                if bps < min_bitrate_bps {
                    Some(BitrateDirection::Low)
                } else if bps > max_bitrate_bps {
                    Some(BitrateDirection::High)
                } else {
                    None
                }
            });
            BitrateItem {
                file_name: episode.file.file_name.clone(),
                path: episode.file.path.display().to_string(),
                bitrate_bps: bitrate,
                estimated,
                needs_fix: direction.is_some(),
                direction,
                error: None,
            }
        })
        .collect()
}

/// 修复目标码率：偏低时拉到 `min + 0.5 Mbps`（不超过上限），偏高时压到上限。
///
/// 偏低时不直接用下限做目标：编码器的实际平均码率会在目标上下浮动，贴着下限编常常差一点不达标。
pub fn bitrate_fix_target(direction: BitrateDirection, min_bps: u64, max_bps: u64) -> u64 {
    match direction {
        BitrateDirection::Low => min_bps.saturating_add(500_000).min(max_bps),
        BitrateDirection::High => max_bps,
    }
}

/// 重编码结果的时长是否与原片一致：误差不超过 `max(0.5 秒, 原时长 × 1%)`。
/// 原片时长读不到（0 / 非有限）时无法核对，按不一致处理——宁可不替换也不冒险覆盖原文件。
pub fn durations_match(original_sec: f64, encoded_sec: f64) -> bool {
    if !(original_sec.is_finite() && original_sec > 0.0 && encoded_sec.is_finite()) {
        return false;
    }
    (encoded_sec - original_sec).abs() <= (original_sec * 0.01).max(0.5)
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

/// 按单集时长区间重组的规划结果。
#[derive(Debug, Clone, PartialEq)]
pub struct RecutPlan {
    /// 源时间线上的 `(起点秒, 终点秒)`，首尾相接覆盖全长。
    pub segments: Vec<(f64, f64)>,
    /// 每集**成片**时长都落在区间内；`false` 表示素材总时长无法在区间内均分，
    /// 给出的是偏差最小的方案（调用方应提示用户）。
    pub in_range: bool,
}

/// 按单集时长区间切全局时间线（不变速）：等价于 [`plan_recut_by_duration_with_speed`]
/// 传入恒为 1.0 的变速。
pub fn plan_recut_by_duration(
    boundaries_sec: &[f64],
    total_sec: f64,
    min_sec: f64,
    max_sec: f64,
) -> Vec<(f64, f64)> {
    plan_recut_by_duration_with_speed(boundaries_sec, total_sec, min_sec, max_sec, |_| 1.0).segments
}

/// 按单集**成片**时长区间切全局时间线，把逐集变速算进去。
///
/// `speed_of(i)` 是第 `i` 集成片的播放倍速（去重配方会加速 1.0~1.25 倍），
/// 第 `i` 段源时长 `d` 对应的成片时长是 `d / speed_of(i)`；`[min_sec, max_sec]`
/// 约束的是成片时长——用户在设置里填的「单集 60~70 秒」指的就是成片。
///
/// 算法：先定集数，再定切点（不再「贪心往前切、尾段太短就并入前一段」，那样会超上限）。
/// 1. 集数 n：成片等长时每集时长为 `total / Σspeed(0..n)`。选落在区间内且最接近区间
///    中点的 n；没有任何 n 落在区间内时选偏差最小的 n，并置 `in_range = false`。
/// 2. 切点：从当前位置出发，第 k 刀的理想位置是把剩余时长按剩余各集倍速等比分配后的
///    第一份；允许吸附到附近的场景切点，吸附范围同时保证本段成片在区间内、剩下的时长
///    仍能让后面各集都落在区间内。附近没有切点就用理想位置。
pub fn plan_recut_by_duration_with_speed(
    boundaries_sec: &[f64],
    total_sec: f64,
    min_sec: f64,
    max_sec: f64,
    speed_of: impl Fn(usize) -> f64,
) -> RecutPlan {
    // 与 plan_recut_segments 相同的 NaN 拒绝语义；区间非法（min ≥ max）同样拒绝。
    let sane = |value: f64| value.partial_cmp(&0.0) == Some(std::cmp::Ordering::Greater);
    if !sane(total_sec)
        || !sane(min_sec)
        || max_sec.partial_cmp(&min_sec) != Some(std::cmp::Ordering::Greater)
    {
        return RecutPlan {
            segments: vec![],
            in_range: false,
        };
    }
    let speed = |index: usize| {
        let value = speed_of(index);
        if value.is_finite() && value > 0.0 {
            value
        } else {
            1.0
        }
    };

    // 1) 定集数。成片每集时长随 n 单调递减，扫到低于下限的那一刻就可以停。
    let target = (min_sec + max_sec) / 2.0;
    let mut speed_sum = 0.0;
    let mut best: Option<(usize, bool, f64)> = None; // (n, 在区间内, 评分：越小越好)
    let max_count = ((total_sec / min_sec).ceil() as usize + 2).clamp(1, 100_000);
    for n in 1..=max_count {
        speed_sum += speed(n - 1);
        let out_len = total_sec / speed_sum;
        let fits = out_len >= min_sec - 1e-9 && out_len <= max_sec + 1e-9;
        let score = if fits {
            (out_len - target).abs()
        } else if out_len < min_sec {
            min_sec - out_len
        } else {
            out_len - max_sec
        };
        let better = match best {
            None => true,
            Some((_, best_fits, best_score)) => {
                (fits && !best_fits) || (fits == best_fits && score < best_score)
            }
        };
        if better {
            best = Some((n, fits, score));
        }
        if out_len < min_sec {
            break;
        }
    }
    let (count, in_range, _) = best.unwrap_or((1, false, 0.0));

    // 2) 定切点。
    let mut sorted: Vec<f64> = boundaries_sec.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let speeds: Vec<f64> = (0..count).map(speed).collect();
    let mut cuts: Vec<f64> = Vec::with_capacity(count.saturating_sub(1));
    let mut pos = 0.0;
    for k in 0..count.saturating_sub(1) {
        let remaining_speed: f64 = speeds[k..].iter().sum();
        let later_speed = remaining_speed - speeds[k];
        let ideal = pos + (total_sec - pos) * speeds[k] / remaining_speed;
        // 本段成片在区间内，且剩余时长让后面各集都能落在区间内。
        let lower = (pos + min_sec * speeds[k]).max(total_sec - max_sec * later_speed);
        let upper = (pos + max_sec * speeds[k]).min(total_sec - min_sec * later_speed);
        let chosen = if lower <= upper {
            sorted
                .iter()
                .copied()
                .filter(|value| *value >= lower && *value <= upper)
                .min_by(|left, right| (left - ideal).abs().total_cmp(&(right - ideal).abs()))
                .unwrap_or(ideal)
        } else {
            ideal
        };
        cuts.push(chosen);
        pos = chosen;
    }
    let mut segments = Vec::with_capacity(count);
    let mut start = 0.0;
    for cut in cuts {
        segments.push((start, cut));
        start = cut;
    }
    segments.push((start, total_sec));
    RecutPlan { segments, in_range }
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

// ============ 挂载文案 CSV 校验 ============

/// 挂载文案 CSV 的单条问题（行号从 1 起，0 表示表头）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountCsvIssue {
    pub row: usize,
    pub message: String,
}

/// 挂载文案 CSV 校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountCsvCheck {
    pub ok: bool,
    /// 解析出的表头列名（已去除 BOM 与首尾空白）。
    pub header: Vec<String>,
    /// 数据行数（不含表头）。
    pub rows: usize,
    pub issues: Vec<MountCsvIssue>,
}

/// 校验「挂载文案.csv」内容：
/// - 必填列「集号」「文案」；
/// - 每行集号必须是数字、落在剧目分集内且不重复；
/// - 文案非空。
///
/// 与其他纯逻辑一致：只做校验，不写文件。
pub fn validate_mount_csv(content: &str, episode_numbers: &[u32]) -> MountCsvCheck {
    let mut issues = Vec::new();

    // 统一去掉 UTF-8 BOM，按行切（容忍 \r\n）。
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .collect();

    let header: Vec<String> = lines
        .first()
        .map(|line| {
            split_csv_line(line)
                .iter()
                .map(|c| c.trim().to_string())
                .collect()
        })
        .unwrap_or_default();

    let idx_of = |name: &str| header.iter().position(|c| c == name);
    let episode_col = idx_of("集号");
    let copy_col = idx_of("文案");
    if episode_col.is_none() {
        issues.push(MountCsvIssue {
            row: 0,
            message: "缺少必填列「集号」".to_string(),
        });
    }
    if copy_col.is_none() {
        issues.push(MountCsvIssue {
            row: 0,
            message: "缺少必填列「文案」".to_string(),
        });
    }

    let rows = lines.len().saturating_sub(1);
    let (episode_col, copy_col) = match (episode_col, copy_col) {
        (Some(e), Some(c)) => (e, c),
        _ => {
            return MountCsvCheck {
                ok: false,
                header,
                rows,
                issues,
            }
        }
    };

    let valid: std::collections::HashSet<u32> = episode_numbers.iter().copied().collect();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();

    for (offset, line) in lines.iter().skip(1).enumerate() {
        let row = offset + 2; // 1 起行号：表头占第 1 行。
        if line.trim().is_empty() {
            issues.push(MountCsvIssue {
                row,
                message: "空行".to_string(),
            });
            continue;
        }
        let cells = split_csv_line(line);
        let episode_raw = cells.get(episode_col).map(|c| c.trim()).unwrap_or("");
        let copy_text = cells.get(copy_col).map(|c| c.trim()).unwrap_or("");

        let number: u32 = match episode_raw.parse() {
            Ok(value) => value,
            Err(_) => {
                issues.push(MountCsvIssue {
                    row,
                    message: format!("集号「{episode_raw}」不是有效数字"),
                });
                continue;
            }
        };
        if !valid.contains(&number) {
            let max = episode_numbers.iter().max().copied().unwrap_or(0);
            issues.push(MountCsvIssue {
                row,
                message: format!("集号 {number} 不在剧目分集内（1~{max}）"),
            });
            continue;
        }
        if !seen.insert(number) {
            issues.push(MountCsvIssue {
                row,
                message: format!("集号 {number} 重复出现"),
            });
        }
        if copy_text.is_empty() {
            issues.push(MountCsvIssue {
                row,
                message: "文案为空".to_string(),
            });
        }
    }

    MountCsvCheck {
        ok: issues.is_empty(),
        header,
        rows,
        issues,
    }
}

/// 单行 CSV 切分：支持双引号包裹字段（字段内可含逗号与 `""` 转义引号）。
fn split_csv_line(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(ch);
            }
        } else {
            match ch {
                '"' if current.is_empty() => in_quotes = true,
                ',' => {
                    cells.push(std::mem::take(&mut current));
                }
                _ => current.push(ch),
            }
        }
    }
    cells.push(current);
    cells
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
    fn bitrate_report_flags_outside_range_both_ways() {
        let mut low = episode_for_test("低.mp4");
        low.spec.bitrate_bps = Some(2_000_000);
        let mut ok = episode_for_test("正常.mp4");
        ok.spec.bitrate_bps = Some(6_000_000);
        let mut high = episode_for_test("高.mp4");
        high.spec.bitrate_bps = Some(12_000_000);
        let mut unknown = episode_for_test("未知.mp4");
        unknown.spec.bitrate_bps = None;

        let report = bitrate_report(&[low, ok, high, unknown], 4_000_000, 8_000_000);
        assert_eq!(report[0].direction, Some(BitrateDirection::Low));
        assert!(report[0].needs_fix);
        assert!(!report[1].needs_fix);
        assert_eq!(report[2].direction, Some(BitrateDirection::High));
        assert!(report[2].needs_fix);
        assert!(!report[3].needs_fix, "读不到码率不处理");
        assert!(report[3].bitrate_bps.is_none());
    }

    #[test]
    fn video_bitrate_prefers_stream_then_estimates() {
        let mut spec = crate::types::EpisodeSpec {
            bitrate_bps: Some(5_000_000),
            ..Default::default()
        };
        // 只有文件码率：原样使用，标为估算
        assert_eq!(video_bitrate(&spec), (Some(5_000_000), true));
        // 有音频码率：减掉音频
        spec.audio_bitrate_bps = Some(1_500_000);
        assert_eq!(video_bitrate(&spec), (Some(3_500_000), true));
        // 有视频轨码率：直接用，不是估算
        spec.video_bitrate_bps = Some(3_200_000);
        assert_eq!(video_bitrate(&spec), (Some(3_200_000), false));
        // 音频高 → 文件码率 5 Mbps 看似达标，视频轨其实只有 3.2 Mbps
        let mut episode = episode_for_test("高音频.mp4");
        episode.spec = spec;
        let report = bitrate_report(&[episode], 4_000_000, 8_000_000);
        assert_eq!(report[0].direction, Some(BitrateDirection::Low));
    }

    #[test]
    fn bitrate_fix_target_and_duration_check() {
        assert_eq!(
            bitrate_fix_target(BitrateDirection::Low, 4_000_000, 8_000_000),
            4_500_000
        );
        assert_eq!(
            bitrate_fix_target(BitrateDirection::High, 4_000_000, 8_000_000),
            8_000_000
        );
        // 区间很窄时，偏低的目标不能超过上限
        assert_eq!(
            bitrate_fix_target(BitrateDirection::Low, 4_000_000, 4_200_000),
            4_200_000
        );

        assert!(durations_match(60.0, 60.3));
        assert!(!durations_match(60.0, 59.0));
        assert!(durations_match(600.0, 605.0), "长片按 1% 容差");
        assert!(!durations_match(600.0, 590.0));
        assert!(!durations_match(0.0, 0.0), "原片时长未知时不替换");
        assert!(!durations_match(60.0, f64::NAN));
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
    fn recut_by_duration_keeps_segments_in_range() {
        // 650 秒、无切点：按中点 65 秒等距切，每段都落在 60~70。
        let even = plan_recut_by_duration(&[], 650.0, 60.0, 70.0);
        assert_eq!(even.len(), 10);
        assert!(even
            .iter()
            .all(|(start, end)| (end - start) >= 60.0 && (end - start) <= 70.0));

        // 区间内有场景切点 → 吸附切点（每段仍在区间内）。
        let boundaries: Vec<f64> = (1..10).map(|k| k as f64 * 65.0 + 2.0).collect();
        let snapped = plan_recut_by_duration(&boundaries, 650.0, 60.0, 70.0);
        assert_eq!(snapped.len(), 10);
        assert!(snapped
            .iter()
            .all(|(start, end)| (end - start) >= 60.0 && (end - start) <= 70.0));
        assert!((snapped[0].1 - 67.0).abs() < 1e-9);
    }

    fn lengths(segments: &[(f64, f64)]) -> Vec<f64> {
        segments.iter().map(|(start, end)| end - start).collect()
    }

    #[test]
    fn recut_by_duration_balances_instead_of_overflowing_tail() {
        // 总长不足一个 min → 整剧一段，并如实报告「不在区间内」。
        let short = plan_recut_by_duration_with_speed(&[], 50.0, 60.0, 70.0, |_| 1.0);
        assert_eq!(short.segments, vec![(0.0, 50.0)]);
        assert!(!short.in_range);
        // 120 秒：以前贪心切 65 剩 55 再并成一段 120 秒（超上限）；现在均分成 2 × 60。
        let plan = plan_recut_by_duration_with_speed(&[], 120.0, 60.0, 70.0, |_| 1.0);
        assert!(plan.in_range);
        assert_eq!(lengths(&plan.segments), vec![60.0, 60.0]);
        // 130 秒仍是两段，且都在区间内。
        let two = plan_recut_by_duration(&[], 130.0, 60.0, 70.0);
        assert_eq!(two.len(), 2);
        assert!(lengths(&two).iter().all(|len| (60.0..=70.0).contains(len)));
        // 区间太窄、任何集数都落不进去（100 秒，区间 60~65）：给偏差最小的方案并置 false。
        let tight = plan_recut_by_duration_with_speed(&[], 100.0, 60.0, 65.0, |_| 1.0);
        assert!(!tight.in_range);
        assert_eq!(tight.segments.last().unwrap().1, 100.0);
        // 非法区间与坏总长一律拒绝。
        assert!(plan_recut_by_duration(&[], 0.0, 60.0, 70.0).is_empty());
        assert!(plan_recut_by_duration(&[], 650.0, 70.0, 60.0).is_empty());
        assert!(plan_recut_by_duration(&[], f64::NAN, 60.0, 70.0).is_empty());
    }

    #[test]
    fn recut_by_duration_keeps_final_lengths_in_range_after_speedup() {
        // 逐集加速 1.0~1.25 倍；约束的是成片时长 = 源时长 / 倍速。
        let speeds = [1.15, 1.13, 1.24, 1.02, 1.2, 1.1, 1.05, 1.18];
        let speed_of = |index: usize| speeds[index % speeds.len()];
        for (total, min, max) in [
            (120.0, 30.0, 50.0),
            (650.0, 60.0, 70.0),
            (1800.0, 60.0, 70.0),
        ] {
            let plan = plan_recut_by_duration_with_speed(&[], total, min, max, speed_of);
            assert!(plan.in_range, "total={total}");
            for (index, len) in lengths(&plan.segments).iter().enumerate() {
                let out = len / speed_of(index);
                assert!(
                    out >= min - 1e-6 && out <= max + 1e-6,
                    "total={total} 第 {index} 集成片 {out:.2}s 不在 {min}~{max}"
                );
            }
            assert!((plan.segments.last().unwrap().1 - total).abs() < 1e-9);
        }
    }

    #[test]
    fn recut_by_duration_snaps_to_scene_cuts_within_safe_window() {
        // 理想切点 60；附近 62 处有场景切点，吸附后两段都仍在 55~65 内。
        let plan = plan_recut_by_duration_with_speed(&[62.0, 100.0], 120.0, 55.0, 65.0, |_| 1.0);
        assert!(plan.in_range);
        assert!((plan.segments[0].1 - 62.0).abs() < 1e-9);
        // 切点太远（吸附会让下一段越界）→ 不吸附，用理想位置。
        let plan = plan_recut_by_duration_with_speed(&[50.0], 120.0, 55.0, 65.0, |_| 1.0);
        assert!((plan.segments[0].1 - 60.0).abs() < 1e-9);
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
                ..Default::default()
            },
        }
    }

    #[test]
    fn mount_csv_accepts_valid_content() {
        let csv = "\u{feff}集号,文案\r\n1,\"第一集文案，含逗号\"\r\n2,第二集文案\r\n";
        let check = validate_mount_csv(csv, &[1, 2, 3]);
        assert!(check.ok, "issues: {:?}", check.issues);
        assert_eq!(check.rows, 2);
        assert_eq!(check.header, vec!["集号", "文案"]);
    }

    #[test]
    fn mount_csv_flags_missing_columns() {
        let check = validate_mount_csv("编号,内容\n1,文案\n", &[1]);
        assert!(!check.ok);
        assert!(check
            .issues
            .iter()
            .any(|i| i.row == 0 && i.message.contains("集号")));
        assert!(check
            .issues
            .iter()
            .any(|i| i.row == 0 && i.message.contains("文案")));
    }

    #[test]
    fn mount_csv_flags_bad_rows() {
        // 第 2 行文案为空；第 3 行集号越界；第 4 行集号重复且非数字其一。
        let csv = "集号,文案\n1,\n9,越界\nabc,非法\n2,正常\n";
        let check = validate_mount_csv(csv, &[1, 2]);
        assert!(!check.ok);
        let messages: Vec<&str> = check.issues.iter().map(|i| i.message.as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("文案为空")));
        assert!(messages.iter().any(|m| m.contains("不在剧目分集内")));
        assert!(messages.iter().any(|m| m.contains("不是有效数字")));
        // 正常行不该报问题。
        assert!(check.issues.iter().all(|i| i.row != 5));
    }

    #[test]
    fn mount_csv_flags_duplicate_episode() {
        let csv = "集号,文案\n1,第一条\n1,第二条\n";
        let check = validate_mount_csv(csv, &[1]);
        assert!(!check.ok);
        assert!(check
            .issues
            .iter()
            .any(|i| i.row == 3 && i.message.contains("重复")));
    }

    #[test]
    fn mount_csv_requires_all_episodes_covered_is_not_forced_here() {
        // 校验器只管「行的合法性」，不强制覆盖全部分集（模板补全由上层负责）。
        let csv = "集号,文案\n1,只有一集\n";
        let check = validate_mount_csv(csv, &[1, 2, 3]);
        assert!(check.ok);
    }
}
