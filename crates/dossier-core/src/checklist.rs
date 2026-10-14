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
/// None 表示该集尚未探测（产生 pending 项）。
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
        Some(s) => {
            let len = std::fs::read_to_string(s)
                .map(|t| t.trim().chars().count())
                .unwrap_or(0);
            if len >= 30 {
                add(
                    "synopsis",
                    "剧名与简介",
                    CheckStatus::Pass,
                    format!("{len} 字"),
                );
            } else {
                add(
                    "synopsis",
                    "剧名与简介",
                    CheckStatus::Warn,
                    format!("简介过短（{len} 字，建议不少于 30 字）"),
                );
            }
        }
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
                CheckStatus::Pending,
                "尚未探测".into(),
            ),
            Some(spec) => {
                // 时长
                if spec.duration_sec < rules.min_duration_sec {
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
                // 码率
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
