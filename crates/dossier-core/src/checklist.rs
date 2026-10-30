//! 报审材料完整性检查。
//!
//! 规则阈值默认值参考行业惯例，均可通过 [`ChecklistRules`] 调整。

use serde::{Deserialize, Serialize};

use crate::types::{CheckStatus, Drama, EpisodeSpec};

/// 检查规则配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChecklistRules {
    /// 最少分集数（不足为警告，不阻断）。
    pub min_episodes: u32,
    /// 单集时长下限（秒）。
    pub min_duration_sec: f64,
    /// 单集时长上限（秒）。
    pub max_duration_sec: f64,
    /// 目标码率上限（bps）；超过建议转码规范化。
    pub max_bitrate_bps: u64,
    /// 是否建议竖屏（高>宽）。
    pub prefer_portrait: bool,
}

impl Default for ChecklistRules {
    fn default() -> Self {
        Self {
            min_episodes: 4,
            min_duration_sec: 31.0,
            max_duration_sec: 480.0,
            max_bitrate_bps: 8_000_000,
            prefer_portrait: true,
        }
    }
}

impl ChecklistRules {
    /// 校验阈值是否自洽，返回可直接展示的中文错误。
    pub fn validate(&self) -> Result<(), String> {
        if self.min_episodes == 0 {
            return Err("最少集数必须大于 0。".into());
        }
        if !self.min_duration_sec.is_finite() || self.min_duration_sec <= 0.0 {
            return Err("单集时长下限必须是大于 0 的秒数。".into());
        }
        if !self.max_duration_sec.is_finite() || self.max_duration_sec <= self.min_duration_sec {
            return Err(format!(
                "单集时长上限（{:.0}s）必须大于下限（{:.0}s）。",
                self.max_duration_sec, self.min_duration_sec
            ));
        }
        if self.max_bitrate_bps == 0 {
            return Err("码率上限必须大于 0。".into());
        }
        Ok(())
    }
}

/// 一条检查结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
}

/// 一部剧的检查报告。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistReport {
    pub drama_name: String,
    pub items: Vec<ChecklistItem>,
    /// 全部为 pass 时为 true；warn 不阻断。
    pub ready: bool,
}

/// 对一部剧执行检查。`specs` 与 `drama.episodes` 按下标对应；
/// None 表示该集规格读取失败（探测出错），判为不通过：生成报审包时同样会被拒绝，两边说法一致。
pub fn run_checks(
    drama: &Drama,
    specs: &[Option<EpisodeSpec>],
    rules: &ChecklistRules,
) -> ChecklistReport {
    let mut items = Vec::new();
    let mut pass = true;
    let mut add = |id: &str, label: &str, status: CheckStatus, detail: String| {
        if status == CheckStatus::Fail {
            pass = false;
        }
        items.push(ChecklistItem {
            id: id.to_string(),
            label: label.to_string(),
            status,
            detail,
        });
    };

    // 分集数量
    let n = drama.episodes.len() as u32;
    if n == 0 {
        add(
            "episodes",
            "分集视频",
            CheckStatus::Fail,
            "未识别到任何分集视频（支持 mp4/mov/mkv/flv/ts/m4v/webm）".into(),
        );
    } else if n < rules.min_episodes {
        add(
            "episodes",
            "分集视频",
            CheckStatus::Warn,
            format!("共 {n} 集，少于建议的 {} 集", rules.min_episodes),
        );
    } else {
        add(
            "episodes",
            "分集视频",
            CheckStatus::Pass,
            format!("共 {n} 集"),
        );
    }

    // 集号：重复 / 缺号只提醒，不拦截（规格没有这条规则；报审包按集号命名，重复会加 -2 后缀）。
    if let Some(detail) = episode_number_issues(drama) {
        add("episode-numbers", "集号", CheckStatus::Warn, detail);
    }

    // 封面
    match &drama.cover {
        Some(c) => add(
            "cover",
            "封面图",
            CheckStatus::Pass,
            c.display().to_string(),
        ),
        None => add(
            "cover",
            "封面图",
            CheckStatus::Fail,
            "缺少封面（文件名含 封面/cover/poster 的 jpg/png/webp）".into(),
        ),
    }

    // 简介
    match &drama.synopsis {
        Some(s) => match std::fs::read_to_string(s).map(|t| t.trim().chars().count()) {
            Err(error) => add(
                "synopsis",
                "剧名与简介",
                CheckStatus::Warn,
                format!("简介文件无法读取（{error}），请确认是 UTF-8 文本"),
            ),
            Ok(len) if len >= 30 => {
                add(
                    "synopsis",
                    "剧名与简介",
                    CheckStatus::Pass,
                    format!("{len} 字"),
                );
            }
            Ok(len) => add(
                "synopsis",
                "剧名与简介",
                CheckStatus::Warn,
                format!("简介过短（{len} 字，建议不少于 30 字）"),
            ),
        },
        None => add(
            "synopsis",
            "剧名与简介",
            CheckStatus::Fail,
            "缺少简介（文件名含 简介/intro/synopsis 的 txt/md）".into(),
        ),
    }

    // 逐集规格
    for (i, ep) in drama.episodes.iter().enumerate() {
        let no = ep
            .episode_number
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        match specs.get(i).cloned().flatten() {
            None => add(
                &format!("ep-{no}"),
                &format!("第 {no} 集规格"),
                CheckStatus::Fail,
                format!(
                    "{}：规格读取失败，生成报审包时会被拒绝；原因见分集规格表",
                    ep.file_name
                ),
            ),
            Some(spec) => {
                // 时长：读不到（0 或非有限值）时不能当成「低于下限」，如实提示人工确认。
                if !spec.duration_sec.is_finite() || spec.duration_sec <= 0.0 {
                    add(
                        &format!("ep-{no}-duration"),
                        &format!("第 {no} 集时长"),
                        CheckStatus::Warn,
                        "读不到时长，请人工确认".into(),
                    );
                } else if spec.duration_sec < rules.min_duration_sec {
                    add(
                        &format!("ep-{no}-duration"),
                        &format!("第 {no} 集时长"),
                        CheckStatus::Warn,
                        format!(
                            "{:.1}s 低于下限 {:.0}s",
                            spec.duration_sec, rules.min_duration_sec
                        ),
                    );
                } else if spec.duration_sec > rules.max_duration_sec {
                    add(
                        &format!("ep-{no}-duration"),
                        &format!("第 {no} 集时长"),
                        CheckStatus::Warn,
                        format!(
                            "{:.1}s 超过上限 {:.0}s",
                            spec.duration_sec, rules.max_duration_sec
                        ),
                    );
                }
                // 码率：文件码率与视频轨码率都读不到才提示（ts / flv 常常没有文件码率）。
                if spec.bitrate_bps.is_none() && spec.video_bitrate_bps.is_none() {
                    add(
                        &format!("ep-{no}-bitrate"),
                        &format!("第 {no} 集码率"),
                        CheckStatus::Warn,
                        "读不到码率，请人工确认".into(),
                    );
                }
                if let Some(bps) = spec.bitrate_bps {
                    if bps > rules.max_bitrate_bps {
                        add(
                            &format!("ep-{no}-bitrate"),
                            &format!("第 {no} 集码率"),
                            CheckStatus::Warn,
                            format!(
                                "{:.1} Mbps 超过 {} Mbps，建议转码规范化",
                                bps as f64 / 1e6,
                                rules.max_bitrate_bps as f64 / 1e6
                            ),
                        );
                    }
                }
                // 分辨率读不到时方向也无从判断。
                if spec.width == 0 || spec.height == 0 {
                    add(
                        &format!("ep-{no}-resolution"),
                        &format!("第 {no} 集分辨率"),
                        CheckStatus::Warn,
                        "读不到分辨率，请人工确认".into(),
                    );
                }
                // 方向
                if rules.prefer_portrait
                    && spec.width > 0
                    && spec.height > 0
                    && spec.width > spec.height
                {
                    add(
                        &format!("ep-{no}-orientation"),
                        &format!("第 {no} 集画幅"),
                        CheckStatus::Warn,
                        format!("{}x{} 为横屏；短剧通常要求竖屏", spec.width, spec.height),
                    );
                }
            }
        }
    }

    ChecklistReport {
        drama_name: drama.name.clone(),
        ready: pass,
        items,
    }
}

/// 集号重复与缺号说明；都没问题时返回 None。
/// 缺号只在所有分集都解析出集号时才检查（有解析不出的分集时，缺的那集可能就是它）。
fn episode_number_issues(drama: &Drama) -> Option<String> {
    let numbers: Vec<u32> = drama
        .episodes
        .iter()
        .filter_map(|e| e.episode_number)
        .collect();
    if numbers.is_empty() {
        return None;
    }
    let mut counts = std::collections::BTreeMap::new();
    for n in &numbers {
        *counts.entry(*n).or_insert(0_u32) += 1;
    }
    let duplicated: Vec<String> = counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(n, count)| format!("第 {n} 集 ×{count}"))
        .collect();
    let mut parts = Vec::new();
    if !duplicated.is_empty() {
        parts.push(format!(
            "集号重复：{}（报审包里重复的会命名为 第NN集-2 等）",
            duplicated.join("、")
        ));
    }
    if numbers.len() == drama.episodes.len() {
        let (min, max) = (*counts.keys().next()?, *counts.keys().last()?);
        // 只算个数、样本最多取 20 个：异常文件名可能解析出极大的集号，不能把整段区间收集进内存。
        let total_missing = u64::from(max) - u64::from(min) + 1 - counts.len() as u64;
        if total_missing > 0 {
            let shown: Vec<String> = (min..=max)
                .filter(|n| !counts.contains_key(n))
                .take(20)
                .map(|n| n.to_string())
                .collect();
            let more = if total_missing > 20 {
                format!(" 等 {total_missing} 个")
            } else {
                String::new()
            };
            parts.push(format!("缺少集号：{}{more}", shown.join("、")));
        }
    }
    (!parts.is_empty()).then(|| parts.join("；"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EpisodeFile;
    use std::path::PathBuf;

    fn drama(n_eps: usize) -> Drama {
        Drama {
            name: "t".into(),
            dir: PathBuf::from("/tmp/t"),
            episodes: (1..=n_eps)
                .map(|i| EpisodeFile {
                    path: PathBuf::from(format!("/tmp/t/第{i}集.mp4")),
                    file_name: format!("第{i}集.mp4"),
                    size_bytes: 1,
                    episode_number: Some(i as u32),
                })
                .collect(),
            cover: Some(PathBuf::from("/tmp/t/封面.jpg")),
            synopsis: None,
        }
    }

    #[test]
    fn flags_missing_synopsis() {
        let d = drama(4);
        let r = run_checks(&d, &[], &ChecklistRules::default());
        assert!(!r.ready);
        assert!(r
            .items
            .iter()
            .any(|i| i.id == "synopsis" && i.status == CheckStatus::Fail));
    }

    fn full_spec() -> EpisodeSpec {
        EpisodeSpec {
            duration_sec: 60.0,
            width: 1080,
            height: 1920,
            bitrate_bps: Some(5_000_000),
            ..Default::default()
        }
    }

    fn item<'a>(r: &'a ChecklistReport, id: &str) -> Option<&'a ChecklistItem> {
        r.items.iter().find(|i| i.id == id)
    }

    #[test]
    fn probe_failure_is_a_fail_not_pending() {
        let mut d = drama(2);
        d.synopsis = None;
        let r = run_checks(&d, &[Some(full_spec()), None], &ChecklistRules::default());
        let failed = item(&r, "ep-2").expect("探测失败的分集要有一项");
        assert_eq!(failed.status, CheckStatus::Fail);
        assert!(failed.detail.contains("规格读取失败"));
        assert!(!r.items.iter().any(|i| i.status == CheckStatus::Pending));
    }

    #[test]
    fn missing_spec_fields_warn_instead_of_misreporting() {
        let d = drama(1);
        let spec = EpisodeSpec::default(); // 时长 0、宽高 0、码率空
        let r = run_checks(&d, &[Some(spec)], &ChecklistRules::default());
        let duration = item(&r, "ep-1-duration").unwrap();
        assert_eq!(duration.status, CheckStatus::Warn);
        assert!(
            duration.detail.contains("读不到时长"),
            "{}",
            duration.detail
        );
        assert!(!duration.detail.contains("低于下限"));
        assert!(item(&r, "ep-1-resolution")
            .unwrap()
            .detail
            .contains("读不到分辨率"));
        assert!(item(&r, "ep-1-bitrate")
            .unwrap()
            .detail
            .contains("读不到码率"));
        // 完整规格：这三项都不出现。
        let r = run_checks(&d, &[Some(full_spec())], &ChecklistRules::default());
        assert!(item(&r, "ep-1-duration").is_none());
        assert!(item(&r, "ep-1-resolution").is_none());
        assert!(item(&r, "ep-1-bitrate").is_none());
    }

    #[test]
    fn unreadable_synopsis_is_reported_as_unreadable() {
        let mut d = drama(1);
        let dir = std::env::temp_dir().join(format!("dossier-syn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("简介.txt");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0xd8]).unwrap(); // 非 UTF-8
        d.synopsis = Some(path);
        let r = run_checks(&d, &[Some(full_spec())], &ChecklistRules::default());
        let syn = item(&r, "synopsis").unwrap();
        assert_eq!(syn.status, CheckStatus::Warn);
        assert!(syn.detail.contains("无法读取"), "{}", syn.detail);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn with_numbers(numbers: &[Option<u32>]) -> Drama {
        let mut d = drama(numbers.len());
        for (episode, number) in d.episodes.iter_mut().zip(numbers) {
            episode.episode_number = *number;
        }
        d
    }

    #[test]
    fn episode_number_duplicates_and_gaps_warn() {
        let rules = ChecklistRules::default();
        let r = run_checks(
            &with_numbers(&[Some(1), Some(2), Some(3), Some(4)]),
            &[],
            &rules,
        );
        assert!(item(&r, "episode-numbers").is_none(), "正常时不加这一项");

        let r = run_checks(
            &with_numbers(&[Some(1), Some(3), Some(3), Some(5)]),
            &[],
            &rules,
        );
        let issue = item(&r, "episode-numbers").unwrap();
        assert_eq!(issue.status, CheckStatus::Warn);
        assert!(issue.detail.contains("第 3 集 ×2"), "{}", issue.detail);
        assert!(issue.detail.contains("缺少集号：2、4"), "{}", issue.detail);

        // 有解析不出集号的分集：不检查缺号。
        let r = run_checks(&with_numbers(&[Some(1), None, Some(4)]), &[], &rules);
        assert!(item(&r, "episode-numbers").is_none());

        // 极大的集号：只计数、不展开整段区间（以前会尝试收集约 40 亿个数）。
        let r = run_checks(&with_numbers(&[Some(1), Some(u32::MAX)]), &[], &rules);
        let issue = item(&r, "episode-numbers").unwrap();
        assert!(
            issue
                .detail
                .contains(&format!("等 {} 个", u64::from(u32::MAX) - 2)),
            "{}",
            issue.detail
        );
    }

    #[test]
    fn bitrate_warning() {
        let d = drama(1);
        let spec = EpisodeSpec {
            duration_sec: 60.0,
            bitrate_bps: Some(9_500_000),
            ..Default::default()
        };
        let r = run_checks(&d, &[Some(spec)], &ChecklistRules::default());
        assert!(r
            .items
            .iter()
            .any(|i| i.id.ends_with("bitrate") && i.status == CheckStatus::Warn));
    }

    #[test]
    fn rejects_inconsistent_rules() {
        assert!(ChecklistRules::default().validate().is_ok());
        assert!(ChecklistRules {
            min_episodes: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(ChecklistRules {
            min_duration_sec: 0.0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(ChecklistRules {
            min_duration_sec: 300.0,
            max_duration_sec: 120.0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(ChecklistRules {
            max_bitrate_bps: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
    }
}
