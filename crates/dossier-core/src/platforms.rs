//! 平台材料要求知识库（Roadmap：平台材料要求知识库）。
//!
//! **重要说明**：这里的数据是**学习用的示例基线**，来自公开的行业惯例与经验值，
//! 不是任何平台的官方规范，也不构成合规意见。接入真实业务前必须替换为平台方
//! 最新发布的《投稿/合作规范》，并保留人工复核环节。
//!
//! 模块只提供「查表 + 对照本地探测结果」的纯函数，不联网、不抓取平台页面。

use serde::{Deserialize, Serialize};

use crate::types::{EpisodeSpec, ProbeFailure, ProbedEpisode};

/// 一项材料要求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialRequirement {
    pub id: String,
    pub label: String,
    /// 是否必需；false 表示「有则更好」。
    pub required: bool,
    pub detail: String,
}

/// 一个平台（或投放形态）的材料与规格要求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformProfile {
    pub id: String,
    pub name: String,
    /// 是否为竖屏形态。
    pub portrait: bool,
    pub container: Vec<String>,
    pub video_codec: Vec<String>,
    pub audio_codec: Vec<String>,
    /// 单集时长建议区间（秒）。
    pub min_duration_sec: f64,
    pub max_duration_sec: f64,
    /// 建议的最小短边（像素）；0 表示不限制。
    pub min_short_side: u32,
    /// 建议的码率上限（bps）。
    pub max_bitrate_bps: u64,
    pub required_materials: Vec<MaterialRequirement>,
    /// 备注（例如「片头片尾规范」「字幕硬性要求」）。
    pub notes: Vec<String>,
}

/// 单项对照结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCheck {
    pub id: String,
    pub label: String,
    /// `pass` / `warn` / `fail`。
    pub status: String,
    pub detail: String,
}

/// 内置示例平台档案：一部竖屏短剧常见的三种投放形态 + 一个通用兜底档案。
///
/// 数值刻意写得保守（宁松勿紧），因为它们只用于学习演示。
pub fn builtin_profiles() -> Vec<PlatformProfile> {
    vec![
        PlatformProfile {
            id: "vertical-short".into(),
            name: "竖屏短剧（示例档案）".into(),
            portrait: true,
            container: vec!["mp4".into(), "mov".into()],
            video_codec: vec!["h264".into(), "hevc".into()],
            audio_codec: vec!["aac".into()],
            min_duration_sec: 30.0,
            max_duration_sec: 300.0,
            min_short_side: 1080,
            max_bitrate_bps: 8_000_000,
            required_materials: vec![
                MaterialRequirement {
                    id: "cover".into(),
                    label: "竖版封面".into(),
                    required: true,
                    detail: "文件名含「封面/cover/poster」，建议 3:4 或 9:16。".into(),
                },
                MaterialRequirement {
                    id: "synopsis".into(),
                    label: "剧名与简介".into(),
                    required: true,
                    detail: "简介建议不少于 30 字，含人物与主线钩子。".into(),
                },
                MaterialRequirement {
                    id: "episode_list".into(),
                    label: "分集清单".into(),
                    required: true,
                    detail: "集号连续、文件名与正片一致。".into(),
                },
                MaterialRequirement {
                    id: "cost_sheet".into(),
                    label: "成本配置表".into(),
                    required: false,
                    detail: "部分平台在签约阶段才要求，先备好可减少返工。".into(),
                },
                MaterialRequirement {
                    id: "ownership".into(),
                    label: "内容权属声明".into(),
                    required: true,
                    detail: "须如实填写并签字盖章，工具只提供模板。".into(),
                },
            ],
            notes: vec![
                "竖屏短剧通常要求 9:16；横屏素材需先做画幅处理。".into(),
                "单集结尾避免硬切，便于平台做连续性推荐。".into(),
            ],
        },
        PlatformProfile {
            id: "horizontal-drama".into(),
            name: "横屏长剧（示例档案）".into(),
            portrait: false,
            container: vec!["mp4".into(), "mov".into()],
            video_codec: vec!["h264".into(), "hevc".into()],
            audio_codec: vec!["aac".into()],
            min_duration_sec: 60.0,
            max_duration_sec: 2700.0,
            min_short_side: 720,
            max_bitrate_bps: 12_000_000,
            required_materials: vec![
                MaterialRequirement {
                    id: "cover".into(),
                    label: "横版封面".into(),
                    required: true,
                    detail: "建议 16:9，海报需含剧名。".into(),
                },
                MaterialRequirement {
                    id: "synopsis".into(),
                    label: "剧名与简介".into(),
                    required: true,
                    detail: "建议附分集梗概，便于人工评审。".into(),
                },
                MaterialRequirement {
                    id: "episode_list".into(),
                    label: "分集清单".into(),
                    required: true,
                    detail: "含时长、规格与校验和。".into(),
                },
                MaterialRequirement {
                    id: "ownership".into(),
                    label: "内容权属声明".into(),
                    required: true,
                    detail: "须如实填写；工具不生成签名或公章。".into(),
                },
            ],
            notes: vec!["横屏素材画幅已符合，无需强制竖屏转换。".into()],
        },
        PlatformProfile {
            id: "trailer-cut".into(),
            name: "预告 / 物料切片（示例档案）".into(),
            portrait: false,
            container: vec!["mp4".into()],
            video_codec: vec!["h264".into()],
            audio_codec: vec!["aac".into()],
            min_duration_sec: 15.0,
            max_duration_sec: 120.0,
            min_short_side: 720,
            max_bitrate_bps: 10_000_000,
            required_materials: vec![MaterialRequirement {
                id: "cover".into(),
                label: "封面帧".into(),
                required: false,
                detail: "可从正片抽取一帧作为封面。".into(),
            }],
            notes: vec!["切片通常不需要单独的成本表与权属声明。".into()],
        },
        PlatformProfile {
            id: "generic-h264-mp4".into(),
            name: "通用兜底（H.264 / MP4）".into(),
            portrait: false,
            container: vec!["mp4".into()],
            video_codec: vec!["h264".into()],
            audio_codec: vec!["aac".into()],
            min_duration_sec: 0.0,
            max_duration_sec: f64::MAX,
            min_short_side: 0,
            max_bitrate_bps: 8_000_000,
            required_materials: vec![],
            notes: vec!["不校验时长与画幅，只保证容器/编码属于最通用的组合。".into()],
        },
    ]
}

/// 按 id 查档案。
pub fn find_profile(id: &str) -> Option<PlatformProfile> {
    builtin_profiles().into_iter().find(|p| p.id == id)
}

/// 用平台档案对照一集规格与本地材料情况。
///
/// `has_cover` / `has_synopsis` 由 `checklist` 或界面给出；这里只做判断，不读文件。
pub fn check_profile(
    profile: &PlatformProfile,
    spec: &EpisodeSpec,
    has_cover: bool,
    has_synopsis: bool,
) -> Vec<ProfileCheck> {
    let mut checks = Vec::new();
    let mut push = |id: &str, label: &str, status: &str, detail: String| {
        checks.push(ProfileCheck {
            id: id.to_string(),
            label: label.to_string(),
            status: status.to_string(),
            detail,
        });
    };

    if profile.min_duration_sec > 0.0 && spec.duration_sec < profile.min_duration_sec {
        push(
            "duration",
            "单集时长",
            "warn",
            format!(
                "{:.1}s 短于建议下限 {:.0}s",
                spec.duration_sec, profile.min_duration_sec
            ),
        );
    } else if spec.duration_sec > profile.max_duration_sec {
        push(
            "duration",
            "单集时长",
            "warn",
            format!(
                "{:.1}s 超过建议上限 {:.0}s",
                spec.duration_sec, profile.max_duration_sec
            ),
        );
    } else {
        push("duration", "单集时长", "pass", "在建议区间内".into());
    }

    // 画幅方向用短边/长边关系判断；竖屏档案要求 height > width。
    if spec.width > 0 && spec.height > 0 {
        let is_portrait = spec.height > spec.width;
        if profile.portrait && !is_portrait {
            push(
                "orientation",
                "画幅方向",
                "fail",
                format!("{}x{} 为横屏，该档案要求竖屏", spec.width, spec.height),
            );
        } else {
            push(
                "orientation",
                "画幅方向",
                "pass",
                if is_portrait {
                    format!("{}x{} 为竖屏，符合档案要求", spec.width, spec.height)
                } else {
                    format!("{}x{} 为横屏，档案未强制竖屏", spec.width, spec.height)
                },
            );
        }
        let short = spec.width.min(spec.height);
        if profile.min_short_side > 0 && short < profile.min_short_side {
            push(
                "resolution",
                "分辨率",
                "warn",
                format!("短边 {short}px 低于建议的 {}px", profile.min_short_side),
            );
        } else {
            push(
                "resolution",
                "分辨率",
                "pass",
                format!("{}x{}", spec.width, spec.height),
            );
        }
    }

    if let Some(bitrate) = spec.bitrate_bps {
        if bitrate > profile.max_bitrate_bps {
            push(
                "bitrate",
                "码率",
                "warn",
                format!(
                    "{:.1} Mbps 超过建议上限 {:.1} Mbps",
                    bitrate as f64 / 1e6,
                    profile.max_bitrate_bps as f64 / 1e6
                ),
            );
        }
    }

    if let Some(codec) = spec.video_codec.as_deref() {
        let ok = profile
            .video_codec
            .iter()
            .any(|c| c.eq_ignore_ascii_case(codec));
        push(
            "video_codec",
            "视频编码",
            if ok { "pass" } else { "warn" },
            if ok {
                format!("{codec} 在档案支持列表内")
            } else {
                format!(
                    "{codec} 不在档案支持列表（{}）",
                    profile.video_codec.join("/")
                )
            },
        );
    }

    for requirement in &profile.required_materials {
        let (status, detail) = match requirement.id.as_str() {
            "cover" => (
                if has_cover { "pass" } else { "fail" },
                requirement.detail.clone(),
            ),
            "synopsis" => (
                if has_synopsis { "pass" } else { "fail" },
                requirement.detail.clone(),
            ),
            _ => ("pending", requirement.detail.clone()),
        };
        if requirement.required || status == "pass" {
            push(
                &format!("material-{}", requirement.id),
                &requirement.label,
                status,
                detail,
            );
        }
    }

    checks
}

/// 一部剧按某个平台档案逐集对照的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeProfileReview {
    pub file_name: String,
    pub episode_number: Option<u32>,
    pub checks: Vec<ProfileCheck>,
}

/// 汇总：整部剧的对照结论。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileReview {
    pub profile_id: String,
    pub profile_name: String,
    pub episodes: Vec<EpisodeProfileReview>,
    /// 所有分集都没有 `fail` 项时为 true（`warn` / `pending` 仍要人工确认）。
    pub clean: bool,
    pub fail_count: usize,
    pub warn_count: usize,
    /// 未能对照的项（例如尚未探测到规格的分集）。
    pub notes: Vec<String>,
}

/// 按平台档案对照整部剧：逐集跑 [`check_profile`]，并汇总结论。
///
/// `probe_failures` 是规格探测失败的分集：每集记一条 `fail` 检查项并写进 `notes`，
/// 不能因为没探测出来就被当成「无阻断」。
pub fn review_drama(
    profile: &PlatformProfile,
    episodes: &[ProbedEpisode],
    probe_failures: &[ProbeFailure],
    has_cover: bool,
    has_synopsis: bool,
) -> ProfileReview {
    let mut reviews = Vec::with_capacity(episodes.len());
    let mut fail_count = 0;
    let mut warn_count = 0;
    let mut notes = Vec::new();
    for episode in episodes {
        let checks = check_profile(profile, &episode.spec, has_cover, has_synopsis);
        fail_count += checks.iter().filter(|c| c.status == "fail").count();
        warn_count += checks.iter().filter(|c| c.status == "warn").count();
        if episode.spec.width == 0 || episode.spec.height == 0 || episode.spec.duration_sec <= 0.0 {
            notes.push(format!(
                "{}：没有可用规格，已跳过画幅/时长对照",
                episode.file.file_name
            ));
        }
        reviews.push(EpisodeProfileReview {
            file_name: episode.file.file_name.clone(),
            episode_number: episode.file.episode_number,
            checks,
        });
    }
    for failure in probe_failures {
        fail_count += 1;
        notes.push(format!(
            "{}：规格探测失败（{}），未能对照",
            failure.file.file_name, failure.reason
        ));
        reviews.push(EpisodeProfileReview {
            file_name: failure.file.file_name.clone(),
            episode_number: failure.file.episode_number,
            checks: vec![ProfileCheck {
                id: "probe".into(),
                label: "规格探测".into(),
                status: "fail".into(),
                detail: format!("探测失败：{}", failure.reason),
            }],
        });
    }
    ProfileReview {
        profile_id: profile.id.clone(),
        profile_name: profile.name.clone(),
        clean: fail_count == 0,
        fail_count,
        warn_count,
        episodes: reviews,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EpisodeFile;
    use std::path::PathBuf;

    fn spec(width: u32, height: u32, duration: f64, bitrate: u64) -> EpisodeSpec {
        EpisodeSpec {
            duration_sec: duration,
            width,
            height,
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            fps: Some(30.0),
            bitrate_bps: Some(bitrate),
            ..Default::default()
        }
    }

    #[test]
    fn builtin_profiles_have_unique_ids_and_validate() {
        let profiles = builtin_profiles();
        assert!(profiles.len() >= 3);
        let mut ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), profiles.len(), "平台档案 id 必须唯一");
        for profile in &profiles {
            assert!(!profile.name.is_empty());
            assert!(!profile.container.is_empty());
            assert!(profile.max_duration_sec >= profile.min_duration_sec);
        }
    }

    #[test]
    fn vertical_profile_fails_on_horizontal_episode() {
        let profile = find_profile("vertical-short").unwrap();
        let checks = check_profile(&profile, &spec(1920, 1080, 90.0, 4_000_000), true, true);
        let orientation = checks.iter().find(|c| c.id == "orientation").unwrap();
        assert_eq!(orientation.status, "fail");
    }

    #[test]
    fn vertical_profile_passes_for_clean_portrait_episode() {
        let profile = find_profile("vertical-short").unwrap();
        let checks = check_profile(&profile, &spec(1080, 1920, 120.0, 4_000_000), true, true);
        // 规格类检查应全部通过；材料类里「分集清单/成本表」需要人工确认，记为 pending。
        assert!(
            checks
                .iter()
                .all(|c| c.status != "fail" && c.status != "warn"),
            "{checks:?}"
        );
        assert!(checks
            .iter()
            .any(|c| c.id == "orientation" && c.status == "pass"));
    }

    #[test]
    fn missing_required_material_is_reported() {
        let profile = find_profile("vertical-short").unwrap();
        let checks = check_profile(&profile, &spec(1080, 1920, 120.0, 4_000_000), false, true);
        let cover = checks.iter().find(|c| c.id == "material-cover").unwrap();
        assert_eq!(cover.status, "fail");
        let ownership = checks
            .iter()
            .find(|c| c.id == "material-ownership")
            .unwrap();
        assert_eq!(ownership.status, "pending");
    }

    #[test]
    fn unknown_profile_returns_none() {
        assert!(find_profile("no-such-profile").is_none());
    }

    #[test]
    fn review_drama_aggregates_counts_and_notes() {
        let profile = find_profile("trailer-cut").unwrap();
        let episodes = vec![
            ProbedEpisode {
                file: EpisodeFile {
                    path: PathBuf::from("/tmp/第01集.mp4"),
                    file_name: "第01集.mp4".into(),
                    size_bytes: 1,
                    episode_number: Some(1),
                },
                spec: spec(1920, 1080, 30.0, 4_000_000),
            },
            ProbedEpisode {
                file: EpisodeFile {
                    path: PathBuf::from("/tmp/第02集.mp4"),
                    file_name: "第02集.mp4".into(),
                    size_bytes: 1,
                    episode_number: Some(2),
                },
                // 未探测到规格：只保留时长/方向之外的结论，并记录一条说明。
                spec: EpisodeSpec::default(),
            },
        ];
        let review = review_drama(&profile, &episodes, &[], true, true);
        assert_eq!(review.episodes.len(), 2);
        assert_eq!(review.profile_id, "trailer-cut");
        assert!(review.clean, "{review:?}");
        assert_eq!(review.fail_count, 0);
        assert_eq!(review.notes.len(), 1);
        assert!(review.notes[0].contains("第02集.mp4"));
    }

    #[test]
    fn probe_failures_count_as_blocking() {
        let profile = find_profile("trailer-cut").unwrap();
        let failure = ProbeFailure {
            file: EpisodeFile {
                path: PathBuf::from("/tmp/第03集.mp4"),
                file_name: "第03集.mp4".into(),
                size_bytes: 1,
                episode_number: Some(3),
            },
            reason: "moov atom not found".into(),
        };
        // 全部失败：以前收到空数组会报「无阻断」。
        let review = review_drama(&profile, &[], &[failure], true, true);
        assert!(!review.clean);
        assert_eq!(review.fail_count, 1);
        assert_eq!(review.episodes.len(), 1);
        assert_eq!(review.episodes[0].episode_number, Some(3));
        assert_eq!(review.episodes[0].checks[0].status, "fail");
        assert!(review.notes[0].contains("moov atom not found"));
    }
}
